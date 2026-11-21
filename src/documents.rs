use crate::entries;
use crate::fs::{bounded_read, private_dir};
use crate::repo::{Repo, excluded, hash};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rustix::fs::{Mode, OFlags, openat};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE_LIMIT: usize = 2 * 1024 * 1024;
const CHUNK_BYTES: usize = 1200;
const CHUNKER: &str = "line-bytes-v1";

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
}

#[derive(Debug, Serialize)]
pub(crate) struct Provenance {
    pub id: String,
    pub kind: Kind,
    pub path: String,
    pub timestamp: Option<String>,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
}

pub(crate) struct Document {
    pub provenance: Provenance,
    pub text: String,
    pub hash: String,
}

impl Document {
    fn new(provenance: Provenance, text: String) -> Self {
        let hash = hash(text.as_bytes());
        Self {
            provenance,
            text,
            hash,
        }
    }
}

pub(crate) struct Snapshot {
    pub documents: Vec<Document>,
    pub digest: String,
}

impl Snapshot {
    pub(crate) fn read(repo: &Repo) -> Result<Self> {
        let config = repo.config()?;
        let mut documents = memories(repo)?;
        for path in source_files(repo, &config.sources)? {
            if let Some(text) = read_source(&repo.root, &path)? {
                let name = path.to_str().context("source path must be UTF-8")?;
                for (ordinal, (text, first, last)) in chunks(&text).into_iter().enumerate() {
                    let id = hash(format!("{CHUNKER}\0{name}\0{ordinal}").as_bytes());
                    documents.push(Document::new(
                        Provenance {
                            id: format!("source:{id}"),
                            kind: Kind::Source,
                            path: name.to_owned(),
                            timestamp: None,
                            line_start: Some(first),
                            line_end: Some(last),
                        },
                        text,
                    ));
                }
            }
        }
        documents.sort_unstable_by(|a, b| a.provenance.id.cmp(&b.provenance.id));
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(&config.sources)?);
        for doc in &documents {
            digest.update(serde_json::to_vec(&doc.provenance)?);
            digest.update(doc.hash.as_bytes());
        }
        Ok(Self {
            documents,
            digest: format!("{:x}", digest.finalize()),
        })
    }
}

fn memories(repo: &Repo) -> Result<Vec<Document>> {
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
        let entry = entries::read(&path)?;
        let id = entry.metadata.id.to_string();
        result.push(Document::new(
            Provenance {
                path: format!(".nunc/entries/{id}.md"),
                id,
                kind: Kind::Memory,
                timestamp: Some(entry.metadata.timestamp.to_rfc3339()),
                line_start: None,
                line_end: None,
            },
            entry.body,
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

fn chunks(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut first = 1;
    let mut last = 1;
    let flush = |out: &mut Vec<_>, buf: &mut String, first, last| {
        if buf.trim().is_empty() {
            buf.clear();
        } else {
            out.push((std::mem::take(buf), first, last));
        }
    };
    for (offset, line) in text.split_inclusive('\n').enumerate() {
        let number = offset + 1;
        let mut remaining = line;
        while !remaining.is_empty() {
            let mut take = remaining.len().min(CHUNK_BYTES);
            while !remaining.is_char_boundary(take) {
                take -= 1;
            }
            if buffer.len() + take > CHUNK_BYTES {
                flush(&mut out, &mut buffer, first, last);
            }
            if buffer.is_empty() {
                first = number;
            }
            buffer.push_str(&remaining[..take]);
            last = number;
            remaining = &remaining[take..];
        }
    }
    flush(&mut out, &mut buffer, first, last);
    out
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
        assert_eq!(second.documents[0].provenance.line_end, Some(1));
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

    #[test]
    fn long_unicode_lines_have_deterministic_provenance() {
        let line = format!("x{}\r\n", "é".repeat(CHUNK_BYTES));
        let parts = chunks(&format!("{line}done\n"));
        assert!(parts.iter().all(|(text, _, _)| text.len() <= CHUNK_BYTES));
        assert_eq!(
            parts.first().map(|(_, start, end)| (*start, *end)),
            Some((1, 1))
        );
        assert_eq!(parts.last().map(|(_, _, end)| *end), Some(2));
        assert_eq!(
            parts
                .into_iter()
                .map(|(text, _, _)| text)
                .collect::<String>(),
            format!("{line}done\n")
        );
    }
}
