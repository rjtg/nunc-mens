use crate::chunk::Strategy;
use crate::documents::{Kind, Provenance, Snapshot};
use crate::index::{Index, Reader};
use crate::ledger::Guard;
use crate::model::{LocalModel, MODEL};
use crate::repo::{Repo, SourceLocation, source_path};
use crate::timeline::{self, DatedEvent, NeighborReport, TimeFilter, parse_timestamp};
use crate::vectors::{self, cosine};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

mod budget;
mod example;
pub(crate) mod markdown;
pub(crate) use budget::{Encoding as BudgetEncoding, Usage as BudgetUsage};

#[derive(Serialize)]
pub(crate) struct Results {
    pub mode_used: &'static str,
    pub repo_id: String,
    pub warnings: Vec<String>,
    pub results: Vec<Match>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<budget::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_context: Option<NeighborReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<example::Reference>,
}

#[derive(Clone, Copy)]
pub(crate) enum Query<'a> {
    Text(&'a str),
    Example(&'a str),
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
    pub source_files: &'a [PathBuf],
    pub context: ContextMode,
    pub sort: Sort,
    pub recency_boost: Option<f64>,
    pub explain: bool,
    pub raw: bool,
    pub one_per_parent: bool,
    pub max_tokens: Option<usize>,
    pub budget_encoding: BudgetEncoding,
    pub time: TimeFilter,
    pub neighbor_window: Option<TimeDelta>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<budget::ContextWindow>,
}

struct Ranked {
    hit: Match,
    parent: String,
    chunk: String,
    timestamp: Option<DateTime<Utc>>,
    section: Option<Range<usize>>,
}

const MEMORY_PATH: &str = ".nunc/entries";

struct SourceFilter {
    path: PathBuf,
    recursive: bool,
    missing: bool,
}

struct Filter {
    kind: Option<Kind>,
    memory: bool,
    sources: Vec<SourceFilter>,
}

impl Filter {
    fn new(repo: &Repo, roots: &[PathBuf], options: &Options<'_>) -> Result<Self> {
        let mut memory = false;
        let mut sources = Vec::new();
        for (path, exact) in options
            .sources
            .iter()
            .map(|p| (p, false))
            .chain(options.source_files.iter().map(|p| (p, true)))
        {
            if !exact
                && path
                    .components()
                    .filter(|part| *part != Component::CurDir)
                    .eq(Path::new(MEMORY_PATH).components())
            {
                memory = true;
                continue;
            }
            let path = source_path(path)?;
            ensure!(
                roots.iter().any(|root| path.starts_with(root)),
                "source filter {} is outside the registered sources; register it with nunc init --source or nunc reindex --source",
                path.display()
            );
            let location = repo.source_location(&path)?;
            ensure!(
                location != SourceLocation::Unsupported,
                "source filter is a symlink or special file: {}",
                path.display()
            );
            ensure!(
                !exact || location != SourceLocation::Directory,
                "--source-file requires a file: {}",
                path.display()
            );
            sources.push(SourceFilter {
                path,
                recursive: !exact && location != SourceLocation::File,
                missing: location == SourceLocation::Missing,
            });
        }
        ensure!(
            sources.is_empty() || options.kind != Some(Kind::Memory),
            "project source filters require source results"
        );
        ensure!(
            !memory || options.kind != Some(Kind::Source),
            "--source .nunc/entries requires memory results"
        );
        let kind = if sources.is_empty() && memory {
            Some(Kind::Memory)
        } else if !sources.is_empty() && !memory {
            Some(Kind::Source)
        } else {
            options.kind
        };
        Ok(Self {
            kind,
            memory,
            sources,
        })
    }

    fn validate_missing(&self, reader: &Reader) -> Result<()> {
        for source in self.sources.iter().filter(|source| source.missing) {
            ensure!(
                reader.has_source_path(&source.path, source.recursive)?,
                "source filter {} does not exist and has no indexed matches; check the repository-relative path",
                source.path.display()
            );
        }
        Ok(())
    }

    fn includes(&self, path: &str) -> bool {
        let path = Path::new(path);
        (!self.memory && self.sources.is_empty())
            || (self.memory && path.starts_with(MEMORY_PATH))
            || self.sources.iter().any(|filter| {
                path == filter.path || (filter.recursive && path.starts_with(&filter.path))
            })
    }

    fn includes_activity(&self) -> bool {
        self.kind != Some(Kind::Source) && self.includes(MEMORY_PATH)
    }
}

pub(crate) fn dated(repo: &Repo, options: &Options<'_>) -> Result<Vec<DatedEvent>> {
    let config = repo.config()?;
    let filter = Filter::new(repo, &config.sources, options)?;
    if !filter.includes_activity() {
        return Ok(Vec::new());
    }
    let guard = Guard::acquire(repo, false)?;
    timeline::events(repo, &guard.ledger, options.raw, options.time, None)
}

pub(crate) fn search(repo: &Repo, query: Query<'_>, options: Options<'_>) -> Result<Results> {
    let config = repo.config()?;
    let filter = Filter::new(repo, &config.sources, &options)?;
    let mut settings = config.search;
    if let Some(boost) = options.recency_boost {
        settings.recency_boost = boost;
    }
    settings.validate()?;
    let now = Utc::now();
    let text_vector = match query {
        Query::Text(text) => {
            ensure!(!text.trim().is_empty(), "search query must be nonblank");
            Some(LocalModel::load(repo)?.query(text)?)
        }
        Query::Example(_) => None,
    };
    let guard = Guard::acquire(repo, false)?;
    let archives = guard.ledger.archives()?;
    let example = match query {
        Query::Text(_) => None,
        Query::Example(value) => Some(example::resolve(
            repo,
            &guard.ledger,
            &archives,
            &config.sources,
            value,
            options.raw,
        )?),
    };
    let reader = Index::for_repo(repo)?
        .read(MODEL)?
        .context("index is absent; run nunc reindex")?;
    filter.validate_missing(&reader)?;
    let (query, reference, snapshot_digest) = match example {
        Some(example) => {
            let (vector, reference, digest) = example.embed(&reader)?;
            (vector, Some(reference), digest)
        }
        None => (text_vector.context("missing query embedding")?, None, None),
    };
    let query_norm = vectors::norm(&query, MODEL.dimensions)?;
    let mut best: Vec<Ranked> = Vec::with_capacity(options.limit);
    reader.scan(
        filter.kind,
        |path| filter.includes(path),
        |row, vector| {
            let parent = row.parent_id()?;
            if reference.as_ref().is_some_and(|source| source.id == parent) {
                return Ok(());
            }
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
            if !options.time.includes(timestamp)
                || (options.neighbor_window.is_some() && timestamp.is_none())
            {
                return Ok(());
            }
            let similarity = cosine(&query, query_norm, vector)?;
            let boost = recency(timestamp, now, &settings);
            let score = similarity + boost;
            let order = |item: &Ranked| {
                recent_order(options.sort, item.timestamp, timestamp)
                    .then_with(|| compare(item.hit.score, &item.chunk, score, id))
            };
            if (options.one_per_parent
                || options.max_tokens.is_some()
                || options.neighbor_window.is_some()
                || reference.is_some())
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
                let matched_text = if options.explain && options.max_tokens.is_none() {
                    text.clone()
                } else {
                    String::new()
                };
                let section = if options.max_tokens.is_some() {
                    text.clear();
                    Some(row.section()?)
                } else {
                    None
                };
                if options.max_tokens.is_none() && !matches!(options.context, ContextMode::Chunk) {
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
                        section,
                        hit: Match {
                            source,
                            text,
                            score,
                            rank: 0,
                            context_window: None,
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
    for (offset, item) in best.iter_mut().enumerate() {
        item.hit.rank = offset + 1;
    }
    let (results, budget) = match options.max_tokens {
        Some(limit) => {
            let (results, usage) = budget::pack(
                best,
                limit,
                options.context,
                options.budget_encoding,
                |parent| reader.parent(parent),
            )?;
            (results, Some(usage))
        }
        None => (best.into_iter().map(|item| item.hit).collect(), None),
    };
    let digest = reader.digest.clone();
    drop(reader);
    let current = match snapshot_digest {
        Some(digest) => Ok(digest),
        None => Snapshot::read(repo).map(|snapshot| snapshot.digest),
    };
    let warning = match current {
        Ok(snapshot_digest) if snapshot_digest == digest => None,
        Ok(_) => Some("Index needs refresh; run nunc reindex or wait for nunc serve".to_owned()),
        Err(error) => Some(format!("Cannot verify index freshness: {error:#}")),
    };
    let temporal_context = if let Some(window) = options.neighbor_window {
        let anchors: Vec<_> = results
            .iter()
            .filter_map(|hit| {
                let at = parse_timestamp(hit.source.timestamp.as_deref()?)?;
                Some((hit.source.id.clone(), at))
            })
            .collect();
        let events = if anchors.is_empty() || !filter.includes_activity() {
            Vec::new()
        } else {
            timeline::events(
                repo,
                &guard.ledger,
                options.raw,
                TimeFilter::default(),
                None,
            )?
        };
        Some(timeline::neighbors(events, &anchors, window, options.limit))
    } else {
        None
    };
    Ok(Results {
        mode_used: "semantic",
        repo_id: repo.id.clone(),
        warnings: warning.into_iter().collect(),
        results,
        budget,
        temporal_context,
        reference,
    })
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
        let dir = tempfile::tempdir()?;
        let repo = Repo::new(dir.path())?;
        std::fs::create_dir_all(dir.path().join("docs/design"))?;
        std::fs::create_dir(dir.path().join("other"))?;
        std::fs::write(dir.path().join("other/file"), "decision")?;
        let roots = ["docs".into(), "other/file".into()];
        let options = Options {
            limit: 1,
            kind: None,
            sources: &["./docs/design/".into(), "./.nunc/entries/".into()],
            source_files: &["other/file".into()],
            context: ContextMode::Chunk,
            sort: Sort::Semantic,
            recency_boost: None,
            explain: false,
            raw: false,
            one_per_parent: false,
            max_tokens: None,
            budget_encoding: BudgetEncoding::Json,
            time: TimeFilter::default(),
            neighbor_window: None,
        };
        let filter = Filter::new(&repo, &roots, &options)?;
        assert_eq!(filter.kind, None);
        assert!(filter.includes("docs/design/sub/architecture.md"));
        assert!(filter.includes("other/file"));
        assert!(filter.includes(".nunc/entries/example.md"));
        assert!(!filter.includes("docs/design-old/doc.md"));
        assert!(!filter.includes("other/file.txt"));
        assert!(
            Filter::new(
                &repo,
                &roots,
                &Options {
                    kind: Some(Kind::Memory),
                    ..options
                }
            )
            .is_err()
        );
        assert!(
            Filter::new(
                &repo,
                &roots,
                &Options {
                    sources: &["./docs/../secret".into()],
                    ..options
                }
            )
            .is_err()
        );
        assert!(
            Filter::new(
                &repo,
                &roots,
                &Options {
                    source_files: &["docs/design".into()],
                    ..options
                }
            )
            .is_err()
        );
        assert!(
            Filter::new(
                &repo,
                &roots,
                &Options {
                    sources: &["elsewhere".into()],
                    ..options
                }
            )
            .is_err()
        );
        Ok(())
    }
}
