use crate::fs::{atomic_write, bounded_read};
use crate::repo::Repo;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, Local};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const MAX_BODY: usize = 256 * 1024;
const MAX_HEADER: usize = 16 * 1024;

#[derive(Debug, Default)]
pub struct ContextFields {
    pub repo: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Metadata {
    version: u8,
    pub id: Uuid,
    pub timestamp: DateTime<FixedOffset>,
    pub repo: Option<String>,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "EntryKind::ordinary")]
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<Commit>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    #[default]
    Memory,
    GitCommit,
    Consolidation,
}

impl EntryKind {
    fn ordinary(&self) -> bool {
        *self == Self::Memory
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub author_email: String,
    pub authored_at: DateTime<FixedOffset>,
    pub committed_at: DateTime<FixedOffset>,
    pub paths: Vec<String>,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
}

pub struct Entry {
    pub metadata: Metadata,
    pub body: String,
}

pub struct Receipt {
    pub id: Uuid,
    pub path: PathBuf,
}

pub fn validate_body(body: &str) -> Result<()> {
    ensure!(body.len() <= MAX_BODY, "entry exceeds {MAX_BODY} bytes");
    ensure!(!body.trim().is_empty(), "entry requires nonblank text");
    ensure!(!body.contains('\0'), "entry contains a NUL byte");
    Ok(())
}

pub fn persist(repo: &Repo, body: &str, fields: ContextFields) -> Result<Receipt> {
    let metadata = metadata(repo, Uuid::new_v4(), fields);
    persist_record(repo, body, metadata)
}

pub(crate) fn metadata(repo: &Repo, id: Uuid, fields: ContextFields) -> Metadata {
    Metadata {
        version: 1,
        id,
        timestamp: Local::now().fixed_offset(),
        repo: fields.repo.or_else(|| {
            repo.git
                .then(|| repo.root.file_name()?.to_str().map(str::to_owned))
                .flatten()
        }),
        tags: fields.tags,
        kind: EntryKind::Memory,
        commit: None,
    }
}

pub(crate) fn path(repo: &Repo, id: Uuid) -> PathBuf {
    repo.entries().join(format!("{id}.md"))
}

pub(crate) fn durable_id(bytes: &[u8]) -> Uuid {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes);
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid::from_bytes(id)
}

pub(crate) fn persist_record(repo: &Repo, body: &str, metadata: Metadata) -> Result<Receipt> {
    validate_body(body)?;
    let header = serde_json::to_string(&metadata)?;
    ensure!(
        header.len() <= MAX_HEADER,
        "entry metadata exceeds {MAX_HEADER} bytes"
    );
    let bytes = format!("---\n{header}\n---\n{body}");
    repo.prepare()?;
    let path = path(repo, metadata.id);
    atomic_write(&path, bytes.as_bytes(), false)?;
    Ok(Receipt {
        id: metadata.id,
        path,
    })
}

pub fn read(path: &Path) -> Result<Entry> {
    read_file(path, &open(path)?)
}

pub(crate) fn open(path: &Path) -> Result<File> {
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("invalid entry {}", path.display()))
}

pub(crate) fn read_file(path: &Path, file: &File) -> Result<Entry> {
    let parse = || -> Result<Entry> {
        let raw = bounded_read(file, MAX_BODY + MAX_HEADER + 9)?;
        let text = String::from_utf8(raw).context("entry must be UTF-8")?;
        let text = text.strip_prefix("---\n").context("missing front matter")?;
        let (header, body) = text
            .split_once("\n---\n")
            .context("unterminated front matter")?;
        ensure!(
            header.len() <= MAX_HEADER && !header.contains('\n'),
            "invalid metadata line"
        );
        let metadata: Metadata = serde_json::from_str(header).context("invalid entry metadata")?;
        ensure!(metadata.version == 1, "unsupported entry format version");
        let name = format!("{}.md", metadata.id);
        ensure!(
            path.file_name().and_then(|name| name.to_str()) == Some(name.as_str()),
            "entry filename and ID must agree"
        );
        validate_body(body)?;
        Ok(Entry {
            metadata,
            body: body.to_owned(),
        })
    };
    parse().with_context(|| format!("invalid entry {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_markdown_and_duplicate_notes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = Repo::new(dir.path())?;
        let body = "é\r\n\n```\n---\n```";
        let first = persist(
            &repo,
            body,
            ContextFields {
                tags: vec!["tag\nline".into()],
                repo: Some("context".into()),
            },
        )?;
        let second = persist(&repo, body, ContextFields::default())?;
        assert_ne!(first.id, second.id);
        let decoded = read(&first.path)?;
        assert_eq!(decoded.body, body);
        assert_eq!(decoded.metadata.id, first.id);
        assert_eq!(decoded.metadata.tags, ["tag\nline"]);
        assert_eq!(decoded.metadata.repo.as_deref(), Some("context"));
        assert!(persist(&repo, " \n", ContextFields::default()).is_err());
        Ok(())
    }
}
