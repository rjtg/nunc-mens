use super::Candidate;
use serde::Serialize;
use std::collections::BTreeMap;

const K: usize = 60;
const RECENCY_SCALE: f64 = 0.25;

pub(super) fn limit(requested: usize) -> usize {
    (2 * requested).max(100)
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Contribution {
    pub k: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_contribution: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_contribution: Option<f64>,
}

impl Contribution {
    fn new(semantic_rank: Option<usize>, keyword_rank: Option<usize>) -> Self {
        // Normalize by the maximum possible two-branch reciprocal rank sum.
        let contribution = |rank| (K + 1) as f64 / (2.0 * (K + rank) as f64);
        Self {
            k: K,
            semantic_rank,
            keyword_rank,
            semantic_contribution: semantic_rank.map(contribution),
            keyword_contribution: keyword_rank.map(contribution),
        }
    }

    pub(super) fn score(self) -> f64 {
        self.semantic_contribution.unwrap_or(0.0) + self.keyword_contribution.unwrap_or(0.0)
    }
}

pub(super) fn combine(
    semantic: Vec<Candidate>,
    keyword: Vec<Candidate>,
) -> impl Iterator<Item = Candidate> {
    let mut matches = BTreeMap::new();
    for (index, candidate) in semantic.into_iter().enumerate() {
        matches.insert(candidate.chunk.clone(), (candidate, Some(index + 1), None));
    }
    for (index, candidate) in keyword.into_iter().enumerate() {
        let rank = Some(index + 1);
        if let Some((hit, _, keyword_rank)) = matches.get_mut(&candidate.chunk) {
            *keyword_rank = rank;
            hit.bm25 = candidate.bm25;
            hit.exact_match = candidate.exact_match;
        } else {
            matches.insert(candidate.chunk.clone(), (candidate, None, rank));
        }
    }
    matches
        .into_values()
        .map(|(mut candidate, semantic, keyword)| {
            let fusion = Contribution::new(semantic, keyword);
            candidate.boost *= RECENCY_SCALE;
            candidate.score = fusion.score() + candidate.boost;
            candidate.fusion = Some(fusion);
            candidate
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(chunk: &str, similarity: Option<f64>, bm25: Option<f64>) -> Candidate {
        Candidate {
            parent: "same-parent".into(),
            chunk: chunk.into(),
            timestamp: None,
            visibility: None,
            score: 0.0,
            similarity,
            bm25,
            exact_match: bm25.map(|_| true),
            boost: 0.04,
            fusion: None,
        }
    }

    #[test]
    fn combines_only_equal_chunks_and_preserves_actual_branch_evidence() {
        let hits: Vec<_> = combine(
            vec![
                candidate("both", Some(0.7), None),
                candidate("semantic", Some(0.6), None),
            ],
            vec![
                candidate("keyword", None, Some(-2.0)),
                candidate("both", None, Some(-1.0)),
            ],
        )
        .collect();
        let both = &hits[0];
        let keyword = &hits[1];
        let semantic = &hits[2];
        let ranks = both.fusion.expect("fusion evidence");
        assert_eq!(ranks.semantic_rank, Some(1));
        assert_eq!(ranks.keyword_rank, Some(2));
        assert_eq!(both.similarity, Some(0.7));
        assert_eq!(both.bm25, Some(-1.0));
        assert!((both.score - (0.5 + 61.0 / 124.0 + 0.01)).abs() < 1e-12);
        assert_eq!(keyword.similarity, None);
        assert_eq!(semantic.bm25, None);
        assert!(both.score > keyword.score);
        assert!(both.score > semantic.score);
        assert_eq!(limit(5), 100);
        assert_eq!(limit(1000), 2000);
    }
}
