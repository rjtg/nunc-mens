use crate::documents::{Kind, Provenance, Snapshot};
use crate::index::Index;
use crate::model::{LocalModel, MODEL};
use crate::repo::{Repo, source_path};
use crate::vectors;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub(crate) struct Results {
    pub mode_used: &'static str,
    pub repo_id: String,
    pub warnings: Vec<String>,
    pub results: Vec<Match>,
}

#[derive(Serialize)]
pub(crate) struct Match {
    #[serde(flatten)]
    pub source: Provenance,
    pub text: String,
    pub score: f64,
    pub rank: usize,
}

struct Filter {
    kind: Option<Kind>,
    sources: Vec<PathBuf>,
}

impl Filter {
    fn new(kind: Option<Kind>, sources: &[PathBuf]) -> Result<Self> {
        let sources = sources
            .iter()
            .map(|path| source_path(path))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            sources.is_empty() || kind != Some(Kind::Memory),
            "--source requires source results"
        );
        Ok(Self {
            kind: if sources.is_empty() {
                kind
            } else {
                Some(Kind::Source)
            },
            sources,
        })
    }

    fn includes(&self, path: &str) -> bool {
        self.sources.is_empty()
            || self
                .sources
                .iter()
                .any(|prefix| Path::new(path).starts_with(prefix))
    }
}

pub(crate) fn search(
    repo: &Repo,
    query: &str,
    limit: usize,
    kind: Option<Kind>,
    sources: &[PathBuf],
) -> Result<Results> {
    ensure!(!query.trim().is_empty(), "search query must be nonblank");
    let filter = Filter::new(kind, sources)?;
    let mut model = LocalModel::load(repo)?;
    let reader = Index::for_repo(repo)?
        .read(MODEL)?
        .context("index is absent; run nunc reindex")?;
    let query = model.query(query)?;
    let query_norm = vectors::norm(&query, MODEL.dimensions)?;
    drop(model);

    let mut best: Vec<Match> = Vec::with_capacity(limit);
    reader.scan(
        filter.kind,
        |path| filter.includes(path),
        |row, vector| {
            let score = cosine(&query, query_norm, vector)?;
            let id = row.id()?;
            let offset = best
                .partition_point(|item| compare(item.score, &item.source.id, score, id).is_lt());
            if offset < limit {
                let (source, text) = row.document()?;
                best.insert(
                    offset,
                    Match {
                        source,
                        text,
                        score,
                        rank: 0,
                    },
                );
                if best.len() > limit {
                    best.pop();
                }
            }
            Ok(())
        },
    )?;
    for (offset, item) in best.iter_mut().enumerate() {
        item.rank = offset + 1;
    }
    let digest = reader.digest.clone();
    drop(reader);
    let warning = match Snapshot::read(repo) {
        Ok(snapshot) if snapshot.digest == digest => None,
        Ok(_) => Some("Index needs refresh; run nunc reindex or wait for nunc serve".to_owned()),
        Err(error) => Some(format!("Cannot verify index freshness: {error:#}")),
    };
    Ok(Results {
        mode_used: "semantic",
        repo_id: repo.id.clone(),
        warnings: warning.into_iter().collect(),
        results: best,
    })
}

fn compare(a: f64, a_id: &str, b: f64, b_id: &str) -> Ordering {
    b.total_cmp(&a).then_with(|| a_id.cmp(b_id))
}

fn cosine(query: &[f32], query_norm: f64, candidate: &[f32]) -> Result<f64> {
    let candidate_norm = vectors::norm(candidate, query.len())?;
    let dot: f64 = query
        .iter()
        .zip(candidate)
        .map(|(&q, &v)| f64::from(q) * f64::from(v))
        .sum();
    Ok((dot / (query_norm * candidate_norm)).clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_orders_and_rejects_invalid_vectors() -> Result<()> {
        let a = [1.0, 0.0];
        assert_eq!(cosine(&a, 1.0, &[2.0, 0.0])?, 1.0);
        assert_eq!(cosine(&a, 1.0, &[0.0, 2.0])?, 0.0);
        assert_eq!(cosine(&a, 1.0, &[-1.0, 0.0])?, -1.0);
        assert!(cosine(&a, 1.0, &[0.0, 0.0]).is_err());
        assert!(cosine(&a, 1.0, &[1.0]).is_err());
        assert!(compare(1.0, "z", 0.5, "a").is_lt());
        assert!(compare(0.5, "a", 0.5, "z").is_lt());
        Ok(())
    }

    #[test]
    fn filters_are_component_aware_and_repeatable() -> Result<()> {
        let filter = Filter::new(None, &["./docs".into(), "other/file".into()])?;
        assert_eq!(filter.kind, Some(Kind::Source));
        assert!(filter.includes("docs/sub/architecture.md"));
        assert!(filter.includes("other/file"));
        assert!(!filter.includes("docs-old/doc.md"));
        assert!(!filter.includes("other/file.txt"));
        assert!(Filter::new(Some(Kind::Memory), &["docs".into()]).is_err());
        Ok(())
    }
}
