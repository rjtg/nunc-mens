use crate::fs::{atomic_write, read_regular};
use crate::repo::Repo;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, Local};
use serde::{Deserialize, Serialize};
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
    validate_body(body)?;
    let metadata = Metadata {
        version: 1,
        id: Uuid::new_v4(),
        timestamp: Local::now().fixed_offset(),
        repo: fields.repo.or_else(|| {
            repo.git
                .then(|| repo.root.file_name()?.to_str().map(str::to_owned))
                .flatten()
        }),
        tags: fields.tags,
    };
    let header = serde_json::to_string(&metadata)?;
    ensure!(
        header.len() <= MAX_HEADER,
        "entry metadata exceeds {MAX_HEADER} bytes"
    );
    let bytes = format!("---\n{header}\n---\n{body}");
    repo.prepare()?;
    let path = repo.entries().join(format!("{}.md", metadata.id));
    atomic_write(&path, bytes.as_bytes(), false)?;
    Ok(Receipt {
        id: metadata.id,
        path,
    })
}

pub fn read(path: &Path) -> Result<Entry> {
    let parse = || -> Result<Entry> {
        let raw = read_regular(path, MAX_BODY + MAX_HEADER + 9)?;
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
