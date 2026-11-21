use crate::chunk::Strategy;
use crate::documents::{Kind, Provenance, Snapshot};
use crate::index::Index;
use crate::ledger::Guard;
use crate::model::{LocalModel, MODEL};
use crate::repo::{Repo, source_path};
use crate::vectors::{self, cosine};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub(crate) struct Results {
    pub mode_used: &'static str,
    pub repo_id: String,
    pub warnings: Vec<String>,
    pub results: Vec<Match>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ContextMode {
    #[default]
    Chunk,
    Section,
    Parent,
}

#[derive(Clone, Copy, Debug, Default, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Sort {
    #[default]
    Semantic,
    Recent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub recency_boost: f64,
    pub half_life_days: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            recency_boost: 0.0,
            half_life_days: 90.0,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (0.0..=0.2).contains(&self.recency_boost),
            "recency boost must be between 0 and 0.2"
        );
        ensure!(
            self.half_life_days.is_finite() && self.half_life_days > 0.0,
            "half_life_days must be finite and positive"
        );
        Ok(())
    }
}

pub(crate) struct Options<'a> {
    pub limit: usize,
    pub kind: Option<Kind>,
    pub sources: &'a [PathBuf],
    pub context: ContextMode,
    pub sort: Sort,
    pub recency_boost: Option<f64>,
    pub explain: bool,
    pub raw: bool,
    pub one_per_parent: bool,
}

#[derive(Serialize)]
pub(crate) struct Explanation {
    pub similarity: f64,
    pub recency_contribution: f64,
    pub final_score: f64,
    pub model: vectors::Model,
    pub chunking: Strategy,
    pub sort: Sort,
    pub context: ContextMode,
    pub matched_text: String,
}

#[derive(Serialize)]
pub(crate) struct Match {
    #[serde(flatten)]
    pub source: Provenance,
    pub text: String,
    pub score: f64,
    pub rank: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<Explanation>,
}

struct Ranked {
    hit: Match,
    parent: String,
    chunk: String,
    timestamp: Option<DateTime<Utc>>,
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

pub(crate) fn search(repo: &Repo, query: &str, options: Options<'_>) -> Result<Results> {
    ensure!(!query.trim().is_empty(), "search query must be nonblank");
    let filter = Filter::new(options.kind, options.sources)?;
    let mut settings = repo.config()?.search;
    if let Some(boost) = options.recency_boost {
        settings.recency_boost = boost;
    }
    settings.validate()?;
    let now = Utc::now();
    let mut model = LocalModel::load(repo)?;
    let query = model.query(query)?;
    let query_norm = vectors::norm(&query, MODEL.dimensions)?;
    drop(model);
    let guard = Guard::acquire(repo, false)?;
    let archives = guard.ledger.archives()?;
    let reader = Index::for_repo(repo)?
        .read(MODEL)?
        .context("index is absent; run nunc reindex")?;
    let mut best: Vec<Ranked> = Vec::with_capacity(options.limit);
    reader.scan(
        filter.kind,
        |path| filter.includes(path),
        |row, vector| {
            let parent = row.parent_id()?;
            let entry = uuid::Uuid::parse_str(parent).ok();
            let visibility = if let Some(id) = entry {
                if guard.ledger.forgotten(id) {
                    return Ok(());
                }
                if !options.raw && guard.ledger.hidden(&archives, id) {
                    return Ok(());
                }
                Some(guard.ledger.visibility(&archives, id))
            } else {
                None
            };
            let id = row.id()?;
            let timestamp = row.timestamp()?.and_then(parse_timestamp);
            let similarity = cosine(&query, query_norm, vector)?;
            let boost = recency(timestamp, now, &settings);
            let score = similarity + boost;
            let order = |item: &Ranked| {
                recent_order(options.sort, item.timestamp, timestamp)
                    .then_with(|| compare(item.hit.score, &item.chunk, score, id))
            };
            if options.one_per_parent
                && let Some(existing) = best.iter().position(|hit| hit.parent == parent)
            {
                if order(&best[existing]).is_le() {
                    return Ok(());
                }
                best.remove(existing);
            }
            let offset = best.partition_point(|item| order(item).is_lt());
            if offset < options.limit {
                let (mut source, mut text) = row.document()?;
                if entry.is_some() {
                    (source.entry_kind, source.git) = reader.origin(parent)?;
                }
                source.visibility = visibility;
                let matched_text = if options.explain {
                    text.clone()
                } else {
                    String::new()
                };
                if !matches!(options.context, ContextMode::Chunk) {
                    let original = reader.parent(parent)?;
                    text = match options.context {
                        ContextMode::Section => original
                            .get(row.section()?)
                            .context("invalid indexed section")?
                            .to_owned(),
                        _ => original,
                    };
                }
                best.insert(
                    offset,
                    Ranked {
                        parent: parent.to_owned(),
                        chunk: id.to_owned(),
                        timestamp,
                        hit: Match {
                            source,
                            text,
                            score,
                            rank: 0,
                            explanation: options.explain.then_some(Explanation {
                                similarity,
                                recency_contribution: boost,
                                final_score: score,
                                model: MODEL,
                                chunking: reader.strategy,
                                sort: options.sort,
                                context: options.context,
                                matched_text,
                            }),
                        },
                    },
                );
                if best.len() > options.limit {
                    best.pop();
                }
            }
            Ok(())
        },
    )?;
    let results = best
        .into_iter()
        .enumerate()
        .map(|(offset, mut item)| {
            item.hit.rank = offset + 1;
            item.hit
        })
        .collect();
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
        results,
    })
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.to_utc())
}

fn recency(timestamp: Option<DateTime<Utc>>, now: DateTime<Utc>, settings: &Settings) -> f64 {
    timestamp.map_or(0.0, |timestamp| {
        let age_days = (now - timestamp).num_seconds().max(0) as f64 / 86400.0;
        settings.recency_boost * (-age_days / settings.half_life_days).exp2()
    })
}

fn recent_order(sort: Sort, a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match sort {
        Sort::Semantic => Ordering::Equal,
        Sort::Recent => b.cmp(&a),
    }
}

fn compare(a: f64, a_id: &str, b: f64, b_id: &str) -> Ordering {
    b.total_cmp(&a).then_with(|| a_id.cmp(b_id))
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
    fn ranking_uses_original_dates_and_exponential_decay() -> Result<()> {
        let now = parse_timestamp("2026-06-01T12:15:00+02:00").context("invalid fixture date")?;
        let old = parse_timestamp("2026-03-03T10:15:00Z").context("invalid fixture date")?;
        let future = parse_timestamp("2028-01-01T00:00:00Z");
        let settings = Settings {
            recency_boost: 0.10,
            ..Settings::default()
        };
        assert!((recency(Some(old), now, &settings) - 0.05).abs() < 1e-9);
        assert_eq!(recency(future, now, &settings), 0.10);
        assert_eq!(recency(None, now, &settings), 0.0);
        assert_eq!(recency(Some(old), now, &Settings::default()), 0.0);
        assert!(parse_timestamp("2026-02-30").is_none());
        assert!(recent_order(Sort::Recent, None, Some(old)).is_gt());
        assert!(recent_order(Sort::Recent, Some(now), Some(old)).is_lt());
        assert_eq!(
            recent_order(Sort::Semantic, Some(now), Some(old)),
            Ordering::Equal
        );
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
