use serde::Serialize;
use std::cell::Cell;
use std::collections::HashSet;
use std::io::{self, Write};

#[derive(Default)]
pub(super) struct SourceCounts {
    checked: Cell<usize>,
    passed: Cell<usize>,
}

impl SourceCounts {
    pub fn check(&self, included: bool) {
        self.checked.set(self.checked.get() + 1);
        self.passed.set(self.passed.get() + usize::from(included));
    }
}

pub(super) struct RankingCounts {
    pub reference: usize,
    pub archived: usize,
    pub forgotten: usize,
    pub temporal: usize,
    scored: usize,
    parents: Option<HashSet<String>>,
}

impl RankingCounts {
    pub fn new(one_per_parent: bool) -> Self {
        Self {
            reference: 0,
            archived: 0,
            forgotten: 0,
            temporal: 0,
            scored: 0,
            parents: one_per_parent.then(HashSet::new),
        }
    }

    pub fn scored(&mut self, parent: &str) {
        self.scored += 1;
        if let Some(parents) = &mut self.parents
            && !parents.contains(parent)
        {
            parents.insert(parent.to_owned());
        }
    }

    pub fn finish(
        self,
        source: SourceCounts,
        index_freshness: &'static str,
        limit: usize,
        before_budget: usize,
        returned: usize,
    ) -> Diagnostics {
        let ranked = self.parents.as_ref().map_or(self.scored, HashSet::len);
        Diagnostics {
            mode: "semantic",
            semantic: "applied",
            keyword: "unavailable",
            fusion: "unavailable",
            reranker: "unavailable",
            exact: true,
            index_freshness,
            chunks_after_type_scope: source.checked.get(),
            chunks_after_source_filter: source.passed.get(),
            excluded_reference_chunks: self.reference,
            excluded_archived_or_undone_chunks: self.archived,
            excluded_forgotten_chunks: self.forgotten,
            excluded_temporal_chunks: self.temporal,
            scored_chunks: self.scored,
            deduplicated_chunks: self.scored - ranked,
            ranking_unit: if self.parents.is_some() {
                "parent"
            } else {
                "chunk"
            },
            ranked,
            limit,
            beyond_limit: ranked - before_budget,
            before_budget,
            omitted_by_budget: before_budget - returned,
            returned,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Diagnostics {
    mode: &'static str,
    semantic: &'static str,
    keyword: &'static str,
    fusion: &'static str,
    reranker: &'static str,
    exact: bool,
    index_freshness: &'static str,
    chunks_after_type_scope: usize,
    chunks_after_source_filter: usize,
    excluded_reference_chunks: usize,
    excluded_archived_or_undone_chunks: usize,
    excluded_forgotten_chunks: usize,
    excluded_temporal_chunks: usize,
    scored_chunks: usize,
    deduplicated_chunks: usize,
    ranking_unit: &'static str,
    ranked: usize,
    limit: usize,
    beyond_limit: usize,
    before_budget: usize,
    omitted_by_budget: usize,
    returned: usize,
}

impl Diagnostics {
    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "nunc: semantic retrieval, index {}",
            self.index_freshness
        )?;
        writeln!(
            out,
            "  Exact chunk counts: {} in type scope, {} after source filter, {} scored",
            self.chunks_after_type_scope, self.chunks_after_source_filter, self.scored_chunks
        )?;
        writeln!(
            out,
            "  Excluded chunks: {} reference, {} archived or undone, {} forgotten, {} temporal",
            self.excluded_reference_chunks,
            self.excluded_archived_or_undone_chunks,
            self.excluded_forgotten_chunks,
            self.excluded_temporal_chunks
        )?;
        writeln!(
            out,
            "  Ranking by {}: {} considered, {} chunks deduplicated, {} beyond limit {}, {} omitted by budget, {} returned",
            self.ranking_unit,
            self.ranked,
            self.deduplicated_chunks,
            self.beyond_limit,
            self.limit,
            self.omitted_by_budget,
            self.returned
        )
    }
}
