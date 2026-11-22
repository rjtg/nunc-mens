use crate::documents::{Kind, reference_source};
use crate::entries;
use crate::index::Reader;
use crate::ledger::Ledger;
use crate::repo::{Repo, SourceLocation, hash, source_path};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_PASSAGES: usize = 64;

#[derive(Serialize)]
pub(crate) struct Reference {
    pub id: String,
    pub path: String,
    pub kind: Kind,
    pub passage_count: usize,
}

pub(super) struct Resolved {
    id: String,
    path: String,
    kind: Kind,
    hash: String,
}

pub(super) fn resolve(
    repo: &Repo,
    ledger: &Ledger,
    archives: &BTreeMap<Uuid, Uuid>,
    roots: &[PathBuf],
    value: &str,
    raw: bool,
) -> Result<Resolved> {
    if let Ok(id) = Uuid::parse_str(value) {
        ensure!(!ledger.forgotten(id), "entry {id} was forgotten");
        ensure!(
            raw || !ledger.hidden(archives, id),
            "reference entry {id} is hidden; use --raw to include it"
        );
        let entry = ledger.read_entry(repo, id)?;
        let path = entries::path(repo, id);
        let path = path
            .strip_prefix(&repo.root)
            .context("reference path is outside the repository")?;
        return Ok(Resolved {
            id: id.to_string(),
            path: path
                .to_str()
                .context("reference path must be UTF-8")?
                .into(),
            kind: Kind::Memory,
            hash: hash(entry.body.as_bytes()),
        });
    }
    let path = source_path(Path::new(value)).context("invalid search reference")?;
    ensure!(
        roots.iter().any(|root| path.starts_with(root)),
        "reference source {} is outside the registered sources",
        path.display()
    );
    ensure!(
        repo.source_location(&path)? == SourceLocation::File,
        "reference source {} must be an existing regular file",
        path.display()
    );
    let doc = reference_source(repo, &path)?.with_context(|| {
        format!(
            "reference source {} is not indexable (ignored, empty or unsupported)",
            path.display()
        )
    })?;
    Ok(Resolved {
        id: doc.provenance.id,
        path: doc.provenance.path,
        kind: Kind::Source,
        hash: doc.hash,
    })
}

impl Resolved {
    pub fn embed(self, reader: &Reader) -> Result<(Vec<f32>, Reference)> {
        let passages = reader
            .pooled_passages(&self.id, &self.hash, MAX_PASSAGES)?
            .with_context(|| {
                format!(
                    "reference {} has no current indexed embeddings; run nunc reindex",
                    self.path
                )
            })?;
        Ok((
            passages.vector,
            Reference {
                id: self.id,
                path: self.path,
                kind: self.kind,
                passage_count: passages.count,
            },
        ))
    }
}
