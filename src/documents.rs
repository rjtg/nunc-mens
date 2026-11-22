use crate::chunk::{Chunk, Settings};
use crate::entries::{self, Commit, EntryKind};
use crate::fs::{bounded_read, private_dir};
use crate::ledger::{Ledger, Visibility};
use crate::repo::{Config, Repo, excluded, hash};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Memory,
    Source,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Source => "source",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "memory" => Ok(Self::Memory),
            "source" => Ok(Self::Source),
            _ => bail!("unknown indexed document kind"),
        }
    }

    pub(crate) fn is_markdown(self, path: &Path) -> bool {
        self == Self::Memory
            || path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ["md", "markdown", "mdown"]
                        .iter()
                        .any(|name| ext.eq_ignore_ascii_case(name))
                })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Provenance {
    pub id: String,
    pub kind: Kind,
    pub path: String,
    pub timestamp: Option<String>,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_chunk: Option<MatchedChunk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_kind: Option<EntryKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<Commit>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct MatchedChunk {
    pub id: String,
    pub parent_id: String,
    pub content_hash: String,
    pub parent_hash: String,
    pub heading_path: Vec<String>,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(default)]
    pub byte_start: usize,
    #[serde(default)]
    pub byte_end: usize,
}

impl MatchedChunk {
    pub fn new(id: String, hash: String, parent: &Document, chunk: &Chunk) -> Self {
        Self {
            id,
            parent_id: parent.provenance.id.clone(),
            content_hash: hash,
            parent_hash: parent.hash.clone(),
            heading_path: chunk.headings.clone(),
            line_start: chunk.lines[0],
            line_end: chunk.lines[1],
            byte_start: chunk.span.start,
            byte_end: chunk.span.end,
        }
    }
}

pub(crate) struct Document {
    pub provenance: Provenance,
    pub text: String,
    pub hash: String,
    pub markdown: bool,
}

impl Document {
    fn new(provenance: Provenance, text: String, markdown: bool) -> Self {
        let hash = hash(text.as_bytes());
        Self {
            provenance,
            text,
            hash,
            markdown,
        }
    }
}

pub(crate) struct Snapshot {
    pub documents: Vec<Document>,
    pub digest: String,
    pub chunking: Settings,
    pub freshness: Option<String>,
    pub input_manifest: InputManifest,
}

impl Snapshot {
    pub(crate) fn read(repo: &Repo) -> Result<Self> {
        let config = repo.config()?;
        let ledger = Ledger::read(repo)?;
        let mut documents = Vec::new();
        let mut fingerprint = Freshness::new(&config, &ledger)?;
        let mut journal = BTreeMap::new();
        for path in memory_paths(repo, &ledger)? {
            let full_path = repo.root.join(&path);
            let file = entries::open(&full_path)?;
            let before = stamp(&file.metadata()?);
            let (mut state, document) = read_journal(&full_path, &file, &ledger)?;
            let stable = before == stamp(&file.metadata()?);
            fingerprint.reliable &= stable;
            state.file.stamp = stable.then_some(before);
            fingerprint.record(&path, state.file.stamp.as_ref());
            journal.insert(path, state);
            documents.extend(document);
        }
        let mut input_manifest = InputManifest {
            basis: fingerprint.basis.clone(),
            files: BTreeMap::new(),
            journal,
        };
        let mut sources = Sources::open(repo)?;
        for path in source_files(repo, &config.sources)? {
            let state = if let Some(file) = sources.file(&path)? {
                let meta = file.metadata()?;
                let before = stamp(&meta);
                let document = source_text(&file, &meta, &path)?
                    .map(|text| source_document(&path, text))
                    .transpose()?;
                let revision = document.as_ref().map(|doc| doc.hash.clone());
                documents.extend(document);
                // A racing read still gets a content digest. It cannot seed the shortcut.
                let stable = before == stamp(&file.metadata()?);
                fingerprint.reliable &= stable;
                SourceState {
                    stamp: stable.then_some(before),
                    revision,
                }
            } else {
                SourceState {
                    stamp: None,
                    revision: None,
                }
            };
            fingerprint.record(&path, state.stamp.as_ref());
            input_manifest.files.insert(path, state);
        }
        documents.sort_unstable_by(|a, b| a.provenance.id.cmp(&b.provenance.id));
        let mut digest = configuration_digest(&config)?;
        hash_documents(&mut digest, &documents)?;
        Ok(Self {
            documents,
            digest: format!("{:x}", digest.finalize()),
            chunking: config.chunking,
            freshness: fingerprint.finish(),
            input_manifest,
        })
    }
}

fn hash_documents(digest: &mut Sha256, documents: &[Document]) -> Result<()> {
    for doc in documents {
        digest.update(serde_json::to_vec(&doc.provenance)?);
        digest.update(doc.hash.as_bytes());
    }
    Ok(())
}

fn configuration_digest(config: &Config) -> Result<Sha256> {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&(&config.sources, &config.chunking))?);
    Ok(digest)
}

struct Freshness {
    basis: String,
    digest: Sha256,
    reliable: bool,
}

impl Freshness {
    fn new(config: &Config, ledger: &Ledger) -> Result<Self> {
        let mut digest = configuration_digest(config)?;
        let basis = format!("{:x}", digest.clone().finalize());
        digest.update(b"nunc-input-stamps-v2");
        let mut summaries = Sha256::new();
        for id in ledger.published_summary_ids() {
            summaries.update(id.as_bytes());
        }
        digest.update(summaries.finalize());
        Ok(Self {
            basis,
            digest,
            reliable: true,
        })
    }

    fn record(&mut self, path: &Path, stamp: Option<&SourceStamp>) {
        let path = path.as_os_str().as_bytes();
        self.digest.update((path.len() as u64).to_le_bytes());
        self.digest.update(path);
        if let Some(stamp) = stamp {
            self.digest.update([1]);
            self.reliable &= stamp.changed.1 != 0;
            for field in stamp.fields {
                self.digest.update(field.to_le_bytes());
            }
            for field in [
                stamp.changed.0,
                stamp.changed.1,
                stamp.modified.0,
                stamp.modified.1,
            ] {
                self.digest.update(field.to_le_bytes());
            }
        } else {
            self.digest.update([0]);
        }
    }

    fn finish(self) -> Option<String> {
        self.reliable
            .then(|| format!("v2:{:x}", self.digest.finalize()))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct InputManifest {
    basis: String,
    files: BTreeMap<PathBuf, SourceState>,
    journal: BTreeMap<PathBuf, JournalState>,
}

#[derive(Deserialize, Serialize)]
struct SourceState {
    stamp: Option<SourceStamp>,
    revision: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct JournalState {
    file: SourceState,
    summary: Option<uuid::Uuid>,
    indexed: bool,
}

impl JournalState {
    fn revision(&self, included: bool) -> Option<&str> {
        if included {
            self.file.revision.as_deref()
        } else {
            None
        }
    }

    fn included_now(&self, ledger: &Ledger) -> bool {
        self.summary.is_none_or(|id| ledger.summary_published(id))
    }
}

fn read_journal(
    path: &Path,
    file: &File,
    ledger: &Ledger,
) -> Result<(JournalState, Option<Document>)> {
    let entry = entries::read_file(path, file)?;
    let indexed = !ledger.pending_summary(&entry);
    let summary = (entry.metadata.kind == EntryKind::Consolidation).then_some(entry.metadata.id);
    let doc = memory_document(entry);
    let mut hash = Sha256::new();
    hash_documents(&mut hash, std::slice::from_ref(&doc))?;
    let state = JournalState {
        file: SourceState {
            stamp: None,
            revision: Some(format!("{:x}", hash.finalize())),
        },
        summary,
        indexed,
    };
    Ok((state, indexed.then_some(doc)))
}

struct InputObservations {
    basis: String,
    fingerprint: Option<String>,
    observed: Vec<(PathBuf, Option<SourceStamp>)>,
    journal: Vec<(PathBuf, SourceStamp)>,
}

impl InputObservations {
    fn read(repo: &Repo, ledger: &Ledger) -> Result<Self> {
        let config = repo.config()?;
        let mut fingerprint = Freshness::new(&config, ledger)?;
        let mut journal = Vec::new();
        for path in memory_paths(repo, ledger)? {
            let file = entries::open(&repo.root.join(&path))?;
            let state = stamp(&file.metadata()?);
            fingerprint.record(&path, Some(&state));
            journal.push((path, state));
        }
        let paths = source_files(repo, &config.sources)?;
        let mut sources = Sources::open(repo)?;
        let mut observed = Vec::with_capacity(paths.len());
        for path in paths {
            let stamp = sources
                .file(&path)?
                .map(|file| file.metadata().map(|metadata| stamp(&metadata)))
                .transpose()?;
            fingerprint.record(&path, stamp.as_ref());
            observed.push((path, stamp));
        }
        Ok(Self {
            basis: fingerprint.basis.clone(),
            fingerprint: fingerprint.finish(),
            observed,
            journal,
        })
    }

    fn journal_matches(
        &self,
        ledger: &Ledger,
        mut expected: BTreeMap<PathBuf, JournalState>,
        mut reopen: impl FnMut(&Path) -> Result<JournalState>,
    ) -> Result<bool> {
        let mut unchanged = true;
        for (path, observed) in &self.journal {
            let previous = expected.remove(path);
            let old_revision = previous
                .as_ref()
                .and_then(|state| state.revision(state.indexed));
            if observed.changed.1 != 0
                && let Some(state) = &previous
                && state.file.stamp.as_ref() == Some(observed)
            {
                unchanged &= state.revision(state.included_now(ledger)) == old_revision;
            } else {
                let current = reopen(path)?;
                unchanged &= current.revision(current.indexed) == old_revision;
            }
        }
        Ok(unchanged && expected.values().all(|state| !state.indexed))
    }

    fn matches_manifest(
        self,
        mut files: BTreeMap<PathBuf, SourceState>,
        mut revision_for: impl FnMut(&Path) -> Result<Option<String>>,
    ) -> Result<bool> {
        let mut unchanged = true;
        for (path, observed) in self.observed {
            let expected = files.remove(&path);
            if observed.as_ref().is_some_and(|stamp| stamp.changed.1 != 0)
                && expected
                    .as_ref()
                    .is_some_and(|state| state.stamp == observed)
            {
                continue;
            }
            let revision = if observed.is_some() {
                revision_for(&path)?
            } else {
                None
            };
            unchanged &= revision == expected.and_then(|state| state.revision);
        }
        Ok(unchanged && files.values().all(|file| file.revision.is_none()))
    }
}

pub(crate) fn snapshot_is_current(
    repo: &Repo,
    ledger: &Ledger,
    digest: &str,
    fingerprint: Option<&str>,
    load_manifest: impl FnOnce() -> Result<Option<InputManifest>>,
) -> Result<bool> {
    let observed = fingerprint
        .map(|_| InputObservations::read(repo, ledger))
        .transpose()?;
    if let Some(observed) = &observed
        && fingerprint == observed.fingerprint.as_deref()
    {
        return Ok(true);
    }
    if let Some(manifest) = load_manifest()? {
        let observed = match observed {
            Some(observed) => observed,
            None => InputObservations::read(repo, ledger)?,
        };
        let journal_matches = observed.journal_matches(ledger, manifest.journal, |path| {
            let path = repo.root.join(path);
            read_journal(&path, &entries::open(&path)?, ledger).map(|(state, _)| state)
        })?;
        let unchanged = observed.basis == manifest.basis && journal_matches;
        let mut sources = Sources::open(repo)?;
        let sources_match = observed.matches_manifest(manifest.files, |path| {
            // Reopen safely. A changed path can have changed again since enumeration.
            Ok(sources
                .file(path)?
                .map(|file| source_text(&file, &file.metadata()?, path))
                .transpose()?
                .flatten()
                .map(|text| hash(text.as_bytes())))
        })?;
        return Ok(unchanged && sources_match);
    }
    Ok(Snapshot::read(repo)?.digest == digest)
}

pub(crate) fn reference_source(repo: &Repo, path: &Path) -> Result<Option<Document>> {
    if source_files(repo, &[path.to_path_buf()])?.contains(path)
        && let Some(file) = Sources::open(repo)?.file(path)?
        && let Some(text) = source_text(&file, &file.metadata()?, path)?
    {
        return Ok(Some(source_document(path, text)?));
    }
    Ok(None)
}

fn source_document(path: &Path, text: String) -> Result<Document> {
    let name = path.to_str().context("source path must be UTF-8")?;
    Ok(Document::new(
        Provenance {
            id: format!("source:{}", hash(name.as_bytes())),
            kind: Kind::Source,
            path: name.to_owned(),
            timestamp: None,
            line_start: None,
            line_end: None,
            matched_chunk: None,
            entry_kind: None,
            git: None,
            visibility: None,
        },
        text,
        Kind::Source.is_markdown(path),
    ))
}

fn memory_paths(repo: &Repo, ledger: &Ledger) -> Result<BTreeSet<PathBuf>> {
    let dir = repo.entries();
    match fs::symlink_metadata(&dir) {
        Ok(_) => private_dir(&dir)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(err).with_context(|| format!("inspect {}", dir.display())),
    }
    let mut result = BTreeSet::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("list {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
            .is_some_and(|id| ledger.forgotten(id))
        {
            continue;
        }
        result.insert(path.strip_prefix(&repo.root)?.to_path_buf());
    }
    Ok(result)
}

fn memory_document(entry: entries::Entry) -> Document {
    let id = entry.metadata.id.to_string();
    Document::new(
        Provenance {
            path: format!(".nunc/entries/{id}.md"),
            id,
            kind: Kind::Memory,
            timestamp: Some(entry.metadata.timestamp.to_rfc3339()),
            line_start: None,
            line_end: None,
            matched_chunk: None,
            entry_kind: Some(entry.metadata.kind),
            git: entry.metadata.commit,
            visibility: None,
        },
        entry.body,
        true,
    )
}

pub(crate) fn memories(repo: &Repo, ledger: &Ledger) -> Result<Vec<Document>> {
    let mut documents = Vec::new();
    for path in memory_paths(repo, ledger)? {
        let entry = entries::read(&repo.root.join(path))?;
        if !ledger.pending_summary(&entry) {
            documents.push(memory_document(entry));
        }
    }
    Ok(documents)
}

fn source_files(repo: &Repo, roots: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    let mut present = Vec::new();
    for path in roots {
        if repo.validate_source(path, true)? {
            present.push(path.clone());
        }
    }
    if present.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut files = BTreeSet::new();
    if repo.git {
        let output = Command::new("git")
            .args([
                "--literal-pathspecs",
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
            ])
            .args(present)
            .current_dir(&repo.root)
            .output()
            .context("enumerate configured Git sources")?;
        if !output.status.success() {
            bail!(
                "enumerate configured Git sources: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        files.extend(output.stdout.split(|byte| *byte == 0).filter_map(|bytes| {
            let path = std::str::from_utf8(bytes).ok()?;
            (!path.is_empty()).then(|| PathBuf::from(path))
        }));
    } else {
        let anchor = repo.root.clone();
        let walk = ignore::WalkBuilder::new(&repo.root)
            .hidden(false)
            .parents(false)
            .require_git(false)
            .filter_entry(move |entry| {
                let Ok(path) = entry.path().strip_prefix(&anchor) else {
                    return false;
                };
                !excluded(path)
                    && present
                        .iter()
                        .any(|root| path.starts_with(root) || root.starts_with(path))
            })
            .build();
        for entry in walk {
            let entry = entry.context("enumerate configured standalone sources")?;
            if let Some(error) = entry.error() {
                return Err(anyhow::anyhow!("{error}")).context("read standalone ignore rules");
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.insert(entry.path().strip_prefix(&repo.root)?.to_path_buf());
            }
        }
    }
    files.retain(|path| path.to_str().is_some() && !excluded(path));
    Ok(files)
}

struct Sources {
    root: File,
    parent: Option<(PathBuf, File)>,
}

impl Sources {
    fn open(repo: &Repo) -> Result<Self> {
        Ok(Self {
            root: File::open(&repo.root)?,
            parent: None,
        })
    }

    fn file(&mut self, path: &Path) -> Result<Option<File>> {
        const FLAGS: OFlags = OFlags::RDONLY
            .union(OFlags::CLOEXEC)
            .union(OFlags::NOFOLLOW)
            .union(OFlags::NONBLOCK);
        use rustix::io::Errno;

        let mut open = || -> Result<File, Errno> {
            let parent = path.parent().ok_or(Errno::INVAL)?;
            let name = path.file_name().ok_or(Errno::INVAL)?;
            if !self
                .parent
                .as_ref()
                .is_some_and(|(cached, _)| cached == parent)
            {
                self.parent = None;
                let mut dir = None;
                for component in parent.components() {
                    dir = Some(
                        openat(
                            dir.as_ref().unwrap_or(&self.root),
                            component.as_os_str(),
                            FLAGS | OFlags::DIRECTORY,
                            Mode::empty(),
                        )?
                        .into(),
                    );
                }
                self.parent = dir.map(|dir| (parent.to_path_buf(), dir));
            }
            let dir = self.parent.as_ref().map_or(&self.root, |(_, dir)| dir);
            openat(dir, name, FLAGS, Mode::empty()).map(File::from)
        };
        match open() {
            Ok(file) => Ok(Some(file)),
            Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read source {}", path.display())),
        }
    }
}

#[derive(PartialEq, Eq, Deserialize, Serialize)]
struct SourceStamp {
    fields: [u64; 6],
    changed: (i64, i64),
    modified: (i64, i64),
}

fn stamp(meta: &fs::Metadata) -> SourceStamp {
    SourceStamp {
        fields: [
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mode().into(),
            meta.uid().into(),
            meta.gid().into(),
        ],
        changed: (meta.ctime(), meta.ctime_nsec()),
        modified: (meta.mtime(), meta.mtime_nsec()),
    }
}

fn source_text(file: &File, metadata: &fs::Metadata, path: &Path) -> Result<Option<String>> {
    if !metadata.is_file() || metadata.len() > SOURCE_LIMIT as u64 {
        return Ok(None);
    }
    let bytes = bounded_read(file, SOURCE_LIMIT)
        .with_context(|| format!("read source {}", path.display()))?;
    Ok(String::from_utf8(bytes)
        .ok()
        .filter(|text| !text.contains('\0') && !text.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_manifest_reuses_stamps_and_checks_summary_publication() -> Result<()> {
        let summary = uuid::Uuid::new_v4();
        let ledger: Ledger = serde_json::from_value(serde_json::json!({
            "summaries": { summary.to_string(): {"sources": [], "undone": false} }
        }))?;
        let stable = || SourceStamp {
            fields: [0; 6],
            changed: (0, 1),
            modified: (0, 1),
        };
        let seen = InputObservations {
            basis: "basis".into(),
            fingerprint: None,
            observed: vec![],
            journal: vec![
                ("regular.md".into(), stable()),
                ("summary.md".into(), stable()),
                ("touched.md".into(), stable()),
            ],
        };
        let state = |stamp, summary, indexed| JournalState {
            file: SourceState {
                stamp,
                revision: Some("revision".into()),
            },
            summary,
            indexed,
        };
        let previous = || {
            BTreeMap::from([
                ("regular.md".into(), state(Some(stable()), None, true)),
                (
                    "summary.md".into(),
                    state(Some(stable()), Some(summary), false),
                ),
                ("touched.md".into(), state(None, None, true)),
            ])
        };
        let reopen = |path: &Path| {
            assert_eq!(path, Path::new("touched.md"));
            Ok(state(Some(stable()), None, true))
        };
        assert!(seen.journal_matches(&Ledger::default(), previous(), reopen)?);
        assert!(!seen.journal_matches(&ledger, previous(), reopen)?);
        Ok(())
    }

    #[test]
    fn source_manifest_rereads_only_changed_or_unreliable_files() -> Result<()> {
        let stable = || SourceStamp {
            fields: [0; 6],
            changed: (0, 1),
            modified: (0, 1),
        };
        let seen = InputObservations {
            basis: "basis".into(),
            fingerprint: None,
            journal: vec![],
            observed: vec![
                ("good.md".into(), Some(stable())),
                ("touched.md".into(), Some(stable())),
                ("new-empty.md".into(), Some(stable())),
            ],
        };
        let files = BTreeMap::from([
            (
                "good.md".into(),
                SourceState {
                    stamp: Some(stable()),
                    revision: Some("good".into()),
                },
            ),
            (
                "touched.md".into(),
                SourceState {
                    stamp: None,
                    revision: Some("same".into()),
                },
            ),
            (
                "removed-empty.md".into(),
                SourceState {
                    stamp: Some(stable()),
                    revision: None,
                },
            ),
        ]);
        let mut opened = Vec::new();
        assert!(seen.matches_manifest(files, |path| {
            opened.push(path.to_path_buf());
            Ok((path == Path::new("touched.md")).then(|| "same".into()))
        })?);
        assert_eq!(opened, [PathBuf::from("touched.md"), "new-empty.md".into()]);
        Ok(())
    }

    #[test]
    fn paths_ignore_git_ignored_files_and_symlinks_and_deduplicate() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()?
                .success()
        );
        let repo = Repo::new(dir.path())?;
        fs::create_dir(dir.path().join("docs"))?;
        fs::create_dir(dir.path().join("src"))?;
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n")?;
        fs::write(dir.path().join("docs/a.md"), "one\ntwo\n")?;
        fs::write(dir.path().join("docs/ignored.txt"), "ignore this")?;
        fs::write(dir.path().join("docs/binary"), b"abc\0def")?;
        fs::write(dir.path().join("src/x.rs"), "fn main() {}\n")?;
        std::os::unix::fs::symlink("a.md", dir.path().join("docs/link"))?;
        repo.configure(&["docs".into(), "docs/a.md".into(), "src".into()], true)?;
        let first = Snapshot::read(&repo)?;
        let names: BTreeSet<_> = first
            .documents
            .iter()
            .map(|doc| doc.provenance.path.as_str())
            .collect();
        assert_eq!(names, BTreeSet::from(["docs/a.md", "src/x.rs"]));
        fs::write(dir.path().join("docs/a.md"), "changed\n")?;
        fs::remove_file(dir.path().join("src/x.rs"))?;
        let second = Snapshot::read(&repo)?;
        assert_eq!(second.documents.len(), 1);
        assert_eq!(second.documents[0].text, "changed\n");
        let original = first
            .documents
            .iter()
            .find(|doc| doc.provenance.path == "docs/a.md")
            .context("missing source")?;
        assert_eq!(original.provenance.id, second.documents[0].provenance.id);
        assert_ne!(first.digest, second.digest);
        assert!(repo.configure(&["docs/link".into()], true).is_err());
        assert!(repo.configure(&["../outside".into()], true).is_err());
        Ok(())
    }
}
