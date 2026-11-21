use crate::chunk::{Chunk, Settings};
use crate::entries::{self, Commit, EntryKind};
use crate::fs::{bounded_read, private_dir};
use crate::ledger::{Ledger, Visibility};
use crate::repo::{Repo, excluded, hash};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
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
}

impl Snapshot {
    pub(crate) fn read(repo: &Repo) -> Result<Self> {
        let config = repo.config()?;
        let ledger = Ledger::read(repo)?;
        let mut documents = memories(repo, &ledger)?;
        for path in source_files(repo, &config.sources)? {
            if let Some(text) = read_source(&repo.root, &path)? {
                let name = path.to_str().context("source path must be UTF-8")?;
                let markdown = Kind::Source.is_markdown(&path);
                if !text.trim().is_empty() {
                    documents.push(Document::new(
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
                        markdown,
                    ));
                }
            }
        }
        documents.sort_unstable_by(|a, b| a.provenance.id.cmp(&b.provenance.id));
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(&(&config.sources, &config.chunking))?);
        for doc in &documents {
            digest.update(serde_json::to_vec(&doc.provenance)?);
            digest.update(doc.hash.as_bytes());
        }
        Ok(Self {
            documents,
            digest: format!("{:x}", digest.finalize()),
            chunking: config.chunking,
        })
    }
}

fn memories(repo: &Repo, ledger: &Ledger) -> Result<Vec<Document>> {
    let dir = repo.entries();
    match fs::symlink_metadata(&dir) {
        Ok(_) => private_dir(&dir)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("inspect {}", dir.display())),
    }
    let mut result = Vec::new();
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
        let entry = entries::read(&path)?;
        if ledger.pending_summary(&entry) {
            continue;
        }
        let id = entry.metadata.id.to_string();
        result.push(Document::new(
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
        ));
    }
    Ok(result)
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

fn read_source(root: &Path, path: &Path) -> Result<Option<String>> {
    use rustix::io::Errno;

    let open = |mut dir: File| -> Result<Option<File>, Errno> {
        let mut parts = path.components().peekable();
        while let Some(part) = parts.next() {
            let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            if parts.peek().is_none() {
                return openat(&dir, part.as_os_str().as_bytes(), flags, Mode::empty())
                    .map(File::from)
                    .map(Some);
            }
            dir = openat(
                &dir,
                part.as_os_str().as_bytes(),
                flags | OFlags::DIRECTORY,
                Mode::empty(),
            )?
            .into();
        }
        Ok(None)
    };
    let read = || -> Result<Option<String>> {
        let file = match open(File::open(root)?) {
            Ok(Some(file)) => file,
            Ok(None) | Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > SOURCE_LIMIT as u64 {
            return Ok(None);
        }
        let bytes = bounded_read(file, SOURCE_LIMIT)?;
        Ok(String::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0')))
    };
    read().with_context(|| format!("read source {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
