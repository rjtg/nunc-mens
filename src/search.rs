use crate::chunk::Strategy;
use crate::documents::{Kind, Provenance, snapshot_is_current};
use crate::index::{Index, IndexedRow, ReadCache, Reader};
use crate::ledger::Guard;
use crate::model::{LocalModel, MODEL, SharedModel};
use crate::profile::{self, Stage};
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
mod fusion;
pub(crate) mod markdown;
mod pipeline;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<pipeline::Diagnostics>,
}

#[derive(Clone, Copy)]
pub(crate) enum Query<'a> {
    Text(&'a str),
    Example(&'a str),
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Mode {
    Semantic,
    Keyword,
    Hybrid,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Keyword => "keyword",
            Self::Hybrid => "hybrid",
        }
    }
}

enum Evidence<'a> {
    Vector(&'a [f32]),
    Keyword { bm25: Option<f64>, exact: bool },
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
    pub mode: Mode,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion: Option<fusion::Contribution>,
    pub recency_contribution: f64,
    pub final_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<vectors::Model>,
    pub chunking: Strategy,
    pub sort: Sort,
    pub context: ContextMode,
    pub matched_text: String,
}

impl Explanation {
    pub fn score_summary(&self) -> String {
        let mut evidence = Vec::new();
        if let Some(similarity) = self.similarity {
            evidence.push(format!("cosine {similarity:.4}"));
        }
        if let Some(exact) = self.exact_match {
            let lexical = self.bm25.map_or_else(
                || "metadata match".into(),
                |score| format!("BM25 {score:.4}"),
            );
            evidence.push(format!("{lexical}, exact {exact}"));
        }
        if let Some(fusion) = self.fusion {
            evidence.push(format!(
                "semantic rank {}, keyword rank {}, RRF {:.4}",
                fusion
                    .semantic_rank
                    .map_or_else(|| "—".into(), |rank| rank.to_string()),
                fusion
                    .keyword_rank
                    .map_or_else(|| "—".into(), |rank| rank.to_string()),
                fusion.score(),
            ));
        }
        let evidence = evidence.join(", ");
        format!(
            "{evidence}; recency {:.4}; final {:.4}",
            self.recency_contribution, self.final_score
        )
    }
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
    section: Option<Range<usize>>,
}

struct Candidate {
    parent: String,
    chunk: String,
    timestamp: Option<DateTime<Utc>>,
    visibility: Option<crate::ledger::Visibility>,
    score: f64,
    similarity: Option<f64>,
    bm25: Option<f64>,
    exact_match: Option<bool>,
    boost: f64,
    fusion: Option<fusion::Contribution>,
}

struct RankKey<'a> {
    parent: &'a str,
    chunk: &'a str,
    timestamp: Option<DateTime<Utc>>,
    score: f64,
}

fn rank_position(
    best: &mut Vec<Candidate>,
    key: RankKey<'_>,
    limit: usize,
    sort: Sort,
    one_per_parent: bool,
) -> Option<usize> {
    let order = |item: &Candidate| {
        recent_order(sort, item.timestamp, key.timestamp)
            .then_with(|| compare(item.score, &item.chunk, key.score, key.chunk))
    };
    if one_per_parent && let Some(existing) = best.iter().position(|hit| hit.parent == key.parent) {
        if order(&best[existing]).is_le() {
            return None;
        }
        best.remove(existing);
    }
    let offset = best.partition_point(|item| order(item).is_lt());
    (offset < limit).then_some(offset)
}

fn insert_ranked(best: &mut Vec<Candidate>, offset: usize, candidate: Candidate, limit: usize) {
    if best.len() == limit {
        best.pop();
    }
    best.insert(offset, candidate);
}

fn materialize(
    reader: &Reader,
    candidate: Candidate,
    rank: usize,
    options: &Options<'_>,
) -> Result<Ranked> {
    reader.chunk(&candidate.chunk, |row| {
        let (mut source, mut text) = row.document()?;
        if source.kind == Kind::Memory {
            (source.entry_kind, source.git) = reader.origin(&candidate.parent)?;
        }
        source.visibility = candidate.visibility;
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
            let original = reader.parent(&candidate.parent)?;
            text = match options.context {
                ContextMode::Section => original
                    .get(row.section()?)
                    .context("invalid indexed section")?
                    .to_owned(),
                _ => original,
            };
        }
        Ok(Ranked {
            parent: candidate.parent,
            section,
            hit: Match {
                source,
                text,
                score: candidate.score,
                rank,
                context_window: None,
                explanation: options.explain.then_some(Explanation {
                    similarity: candidate.similarity,
                    bm25: candidate.bm25,
                    exact_match: candidate.exact_match,
                    fusion: candidate.fusion,
                    recency_contribution: candidate.boost,
                    final_score: candidate.score,
                    model: candidate.similarity.map(|_| MODEL),
                    chunking: reader.strategy,
                    sort: options.sort,
                    context: options.context,
                    matched_text,
                }),
            },
        })
    })
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

    fn indexed_scope(&self) -> Result<Option<String>> {
        if self.sources.is_empty() {
            return Ok(None);
        }
        let mut paths = Vec::new();
        for source in &self.sources {
            let path = source.path.to_str().context("source path must be UTF-8")?;
            paths.push((path, source.recursive.then(|| format!("{path}0"))));
        }
        if self.memory {
            paths.push((MEMORY_PATH, Some(format!("{MEMORY_PATH}0"))));
        }
        // Binary collation places '0' immediately after '/'. Only the
        // explicitly selected directory and its descendants qualify.
        Ok(Some(serde_json::to_string(&paths)?))
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
    search_with(
        repo,
        query,
        options,
        None,
        &mut ReadCache::default(),
        &|| Ok(()),
    )
}

pub(crate) fn search_with(
    repo: &Repo,
    query: Query<'_>,
    options: Options<'_>,
    shared_model: Option<&SharedModel>,
    read_cache: &mut ReadCache,
    checkpoint: &dyn Fn() -> Result<()>,
) -> Result<Results> {
    checkpoint()?;
    let setup = profile::span(Stage::SearchSetup);
    let config = repo.config()?;
    let filter = Filter::new(repo, &config.sources, &options)?;
    let mut settings = config.search;
    if let Some(boost) = options.recency_boost {
        settings.recency_boost = boost;
    }
    settings.validate()?;
    let now = Utc::now();
    ensure!(
        options.mode == Mode::Semantic || matches!(query, Query::Text(_)),
        "--similar-to requires semantic mode"
    );
    drop(setup);
    let text_vector = match query {
        Query::Text(text) => {
            ensure!(!text.trim().is_empty(), "search query must be nonblank");
            if options.mode != Mode::Keyword {
                let embed = |model: &mut LocalModel| {
                    profile::measure(Stage::EmbedQuery, || model.query(text))
                };
                Some(if let Some(shared) = shared_model {
                    shared.query(repo, text)?
                } else {
                    embed(&mut profile::measure(Stage::ModelLoad, || {
                        LocalModel::load(repo)
                    })?)?
                })
            } else {
                None
            }
        }
        Query::Example(_) => None,
    };
    checkpoint()?;
    let index_read = profile::span(Stage::IndexRead);
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
    let index = Index::for_repo(repo)?;
    let reader = index
        .read_mode(
            MODEL,
            options.mode != Mode::Keyword,
            options.mode != Mode::Semantic,
            read_cache,
        )?
        .with_context(|| {
            if options.mode == Mode::Keyword {
                "index is absent; run nunc reindex --keyword-only"
            } else {
                "index is absent; run nunc reindex"
            }
        })?;
    filter.validate_missing(&reader)?;
    let (vector, reference) = match example {
        Some(example) => {
            let (vector, reference) = example.embed(&reader)?;
            (Some(vector), Some(reference))
        }
        None => (text_vector, None),
    };
    let query_norm = vector
        .as_ref()
        .map(|vector| vectors::norm(vector, MODEL.dimensions))
        .transpose()?;
    drop(index_read);
    let one_per_parent = options.one_per_parent
        || options.max_tokens.is_some()
        || options.neighbor_window.is_some()
        || reference.is_some();
    let source_counts = options.explain.then(pipeline::SourceCounts::default);
    let mut counts = options
        .explain
        .then(|| pipeline::RankingCounts::new(one_per_parent));
    let hybrid = options.mode == Mode::Hybrid;
    let branch_limit = if hybrid {
        fusion::limit(options.limit)
    } else {
        options.limit
    };
    let mut semantic = Vec::new();
    let mut keyword = Vec::new();
    let keyword_scope = options.explain.then(pipeline::SourceCounts::default);
    let mut keyword_eligible = 0;
    let include = |path: &str| {
        let _filter = profile::span(Stage::SourceFilter);
        let included = filter.includes(path);
        if let Some(counts) = &source_counts {
            counts.check(included);
        }
        included
    };
    let mut visited = 0;
    let mut visit = |row: IndexedRow<'_, '_>, evidence: Evidence<'_>| -> Result<()> {
        if visited == 0 {
            checkpoint()?;
        }
        visited = (visited + 1) % 256;
        let filter_span = profile::span(Stage::VisibilityFilter);
        let keyword_branch = matches!(evidence, Evidence::Keyword { .. });
        let counts = if hybrid && keyword_branch {
            None
        } else {
            counts.as_mut()
        };
        let parent = row.parent_id()?;
        if reference.as_ref().is_some_and(|source| source.id == parent) {
            if let Some(counts) = counts {
                counts.reference += 1;
            }
            return Ok(());
        }
        let entry = uuid::Uuid::parse_str(parent).ok();
        let visibility = if let Some(id) = entry {
            if guard.ledger.forgotten(id) {
                if let Some(counts) = counts {
                    counts.forgotten += 1;
                }
                return Ok(());
            }
            if !options.raw && guard.ledger.hidden(&archives, id) {
                if let Some(counts) = counts {
                    counts.archived += 1;
                }
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
            if let Some(counts) = counts {
                counts.temporal += 1;
            }
            return Ok(());
        }
        drop(filter_span);
        let (base, similarity, bm25, exact_match) = match evidence {
            Evidence::Vector(candidate) => {
                let similarity = cosine(
                    vector.as_deref().context("missing query vector")?,
                    query_norm.context("missing query norm")?,
                    candidate,
                )?;
                (similarity, Some(similarity), None, None)
            }
            Evidence::Keyword { bm25, exact } => {
                // FTS5's native BM25 is negative with stronger matches.
                let strength = bm25.map_or(0.0, |rank| (-rank).max(0.0));
                let base = if exact { 1.0 } else { 0.0 } + strength / (1.0 + strength);
                (base, None, bm25, Some(exact))
            }
        };
        if let Some(counts) = counts
            && !hybrid
        {
            counts.scored(parent);
        }
        if hybrid && keyword_branch {
            keyword_eligible += 1;
        }
        let boost = recency(timestamp, now, &settings);
        let score = if hybrid { base } else { base + boost };
        let best = if keyword_branch {
            &mut keyword
        } else {
            &mut semantic
        };
        let key = RankKey {
            parent,
            chunk: id,
            timestamp,
            score,
        };
        if let Some(offset) = rank_position(
            best,
            key,
            branch_limit,
            if hybrid { Sort::Semantic } else { options.sort },
            !hybrid && one_per_parent,
        ) {
            insert_ranked(
                best,
                offset,
                Candidate {
                    parent: parent.to_owned(),
                    chunk: id.to_owned(),
                    timestamp,
                    visibility,
                    score,
                    similarity,
                    bm25,
                    exact_match,
                    boost,
                    fusion: None,
                },
                branch_limit,
            );
        }
        Ok(())
    };
    // Explain must still count the complete type scope before the path filter.
    let indexed_scope = if options.explain {
        None
    } else {
        filter.indexed_scope()?
    };
    if options.mode != Mode::Keyword {
        let _scan = profile::span(Stage::VectorScan);
        reader.scan_scoped(
            filter.kind,
            indexed_scope.as_deref(),
            include,
            |row, vector| visit(row, Evidence::Vector(vector)),
        )?;
    }
    if options.mode != Mode::Semantic {
        let _scan = profile::span(Stage::KeywordScan);
        let Query::Text(text) = query else {
            anyhow::bail!("keyword and hybrid modes require a text query")
        };
        reader.scan_keyword(
            filter.kind,
            text,
            indexed_scope.as_deref(),
            |path| {
                if hybrid {
                    let _filter = profile::span(Stage::SourceFilter);
                    let included = filter.includes(path);
                    if let Some(counts) = &keyword_scope {
                        counts.check(included);
                    }
                    included
                } else {
                    include(path)
                }
            },
            |row, bm25, exact| visit(row, Evidence::Keyword { bm25, exact }),
        )?;
    }
    checkpoint()?;
    let mut fusion_counts = None;
    let candidates = if hybrid {
        let _fusion = profile::span(Stage::Fusion);
        fusion_counts = keyword_scope.map(|scope| {
            pipeline::FusionCounts::new(
                branch_limit,
                semantic.len(),
                keyword.len(),
                scope,
                keyword_eligible,
            )
        });
        let mut best = Vec::new();
        for candidate in fusion::combine(semantic, keyword) {
            if let Some(counts) = &mut counts {
                counts.scored(&candidate.parent);
            }
            let key = RankKey {
                parent: &candidate.parent,
                chunk: &candidate.chunk,
                timestamp: candidate.timestamp,
                score: candidate.score,
            };
            if let Some(offset) =
                rank_position(&mut best, key, options.limit, options.sort, one_per_parent)
            {
                insert_ranked(&mut best, offset, candidate, options.limit);
            }
        }
        best
    } else if options.mode == Mode::Keyword {
        keyword
    } else {
        semantic
    };
    let context = profile::span(Stage::Context);
    let best = candidates
        .into_iter()
        .enumerate()
        .map(|(offset, candidate)| materialize(&reader, candidate, offset + 1, &options))
        .collect::<Result<Vec<_>>>()?;
    let before_budget = best.len();
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
    drop(context);
    let digest = reader.digest.clone();
    let stored_freshness = reader.freshness()?;
    drop(reader);
    checkpoint()?;
    let current = profile::measure(Stage::Freshness, || {
        snapshot_is_current(
            repo,
            &guard.ledger,
            &digest,
            stored_freshness.as_deref(),
            || {
                // Reopen only on mismatch and never pair a new index manifest with older results.
                if let Some(reader) = index.read_mode(MODEL, false, false, read_cache)?
                    && reader.digest == digest
                {
                    return reader.source_manifest();
                }
                Ok(None)
            },
        )
    });
    let (freshness, warning) = match current {
        Ok(true) => ("current", None),
        Ok(false) => (
            "stale",
            Some(
                if options.mode == Mode::Keyword {
                    "Index needs refresh; run nunc reindex --keyword-only or wait for nunc serve"
                } else {
                    "Index needs refresh; run nunc reindex or wait for nunc serve"
                }
                .to_owned(),
            ),
        ),
        Err(error) => (
            "unknown",
            Some(format!("Cannot verify index freshness: {error:#}")),
        ),
    };
    checkpoint()?;
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
    let pipeline = counts.zip(source_counts).map(|(counts, source)| {
        counts
            .finish(
                source,
                options.mode,
                freshness,
                options.limit,
                before_budget,
                results.len(),
            )
            .with_fusion(fusion_counts)
    });
    Ok(Results {
        mode_used: options.mode.name(),
        repo_id: repo.id.clone(),
        warnings: warning.into_iter().collect(),
        results,
        budget,
        temporal_context,
        reference,
        pipeline,
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
            mode: Mode::Semantic,
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
