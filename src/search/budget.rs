use super::{ContextMode, Match, Ranked};
use crate::chunk;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

#[derive(Serialize)]
pub(crate) struct Usage {
    pub max_tokens: usize,
    pub estimated_tokens: usize,
    pub truncated_results: usize,
    pub omitted_candidates: usize,
    estimate: &'static str,
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct ContextWindow {
    pub byte_start: usize,
    pub byte_end: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub truncated: bool,
}

pub(super) fn pack(
    mut ranked: Vec<Ranked>,
    max_tokens: usize,
    mode: ContextMode,
    read_parent: impl Fn(&str) -> Result<String>,
) -> Result<(Vec<Match>, Usage)> {
    let mut remaining = max_tokens;
    let mut costs = vec![0; ranked.len()];
    let count = ranked.len();
    let reserve = if count > 1 { max_tokens / 4 } else { 0 };
    for (index, hit) in ranked.iter_mut().enumerate() {
        let share = if count > 1 {
            (remaining / 2).min(remaining.saturating_sub(reserve))
        } else {
            remaining
        };
        if let Some(cost) = fit(hit, mode, share, false, &read_parent)? {
            costs[index] = cost;
            remaining -= cost;
        }
    }

    // Keep part of the budget available for context around admitted matches.
    // If the initial shares were too small, try to admit a single match first.
    if costs.iter().all(|cost| *cost == 0) {
        for (hit, spent) in ranked.iter_mut().zip(&mut costs) {
            if let Some(cost) = fit(hit, mode, remaining, false, &read_parent)? {
                *spent = cost;
                remaining -= cost;
                break;
            }
        }
    }
    for missing in [false, true] {
        for (hit, spent) in ranked.iter_mut().zip(&mut costs) {
            if remaining == 0 {
                break;
            }
            if (*spent == 0) != missing || hit.hit.context_window.is_some_and(|win| !win.truncated)
            {
                continue;
            }
            if let Some(cost) = fit(hit, mode, remaining + *spent, !missing, &read_parent)? {
                remaining = remaining + *spent - cost;
                *spent = cost;
            }
        }
    }

    let results: Vec<_> = ranked
        .into_iter()
        .filter_map(|item| item.hit.context_window.map(|_| item.hit))
        .collect();
    let usage = Usage {
        max_tokens,
        estimated_tokens: max_tokens - remaining,
        omitted_candidates: count - results.len(),
        truncated_results: results
            .iter()
            .filter(|hit| hit.context_window.is_some_and(|window| window.truncated))
            .count(),
        estimate: "ceil((json-utf8-bytes+1)/4)-per-result",
    };
    Ok((results, usage))
}

fn fit(
    candidate: &mut Ranked,
    mode: ContextMode,
    allowance: usize,
    expand: bool,
    read_parent: &impl Fn(&str) -> Result<String>,
) -> Result<Option<usize>> {
    if allowance == 0 {
        return Ok(None);
    }
    let mut metadata = ByteCount(1);
    // The initial candidate is text-free. If provenance alone is oversized,
    // avoid loading the source body or repeatedly attempting to truncate it.
    if candidate.hit.context_window.is_none() {
        serde_json::to_writer(&mut metadata, &candidate.hit)?;
        if metadata.0.div_ceil(4) >= allowance {
            return Ok(None);
        }
    }
    let body = read_parent(&candidate.parent)?;
    let window = Window::new(candidate, &body, mode)?;
    let old = candidate.hit.context_window;
    let chosen = window.choose(&mut candidate.hit, allowance, expand)?;
    if let Some((span, cost)) = chosen
        && old.is_none_or(|prior| span.end - span.start > prior.byte_end - prior.byte_start)
    {
        window.render(&mut candidate.hit, span);
        return Ok(Some(cost));
    }
    if let Some(prior) = old {
        window.render(&mut candidate.hit, prior.byte_start..prior.byte_end);
    } else {
        candidate.hit.text.clear();
        candidate.hit.context_window = None;
        if let Some(explanation) = candidate.hit.explanation.as_mut() {
            explanation.matched_text.clear();
        }
    }
    Ok(None)
}

struct Window<'a> {
    body: &'a str,
    matched: Range<usize>,
    requested: Range<usize>,
    blocks: Vec<usize>,
    lines: Vec<usize>,
}

impl<'a> Window<'a> {
    fn new(candidate: &Ranked, body: &'a str, mode: ContextMode) -> Result<Self> {
        let attribution = candidate
            .hit
            .source
            .matched_chunk
            .as_ref()
            .context("missing matched chunk for budgeted context")?;
        let matched = attribution.byte_start..attribution.byte_end;
        let requested = match mode {
            ContextMode::Chunk => matched.clone(),
            ContextMode::Section => candidate
                .section
                .clone()
                .context("missing indexed section")?,
            ContextMode::Parent => 0..body.len(),
        };
        ensure!(
            !matched.is_empty()
                && requested.start <= matched.start
                && matched.end <= requested.end
                && body.get(matched.clone()).is_some()
                && body.get(requested.clone()).is_some(),
            "invalid indexed context range; run nunc reindex --full"
        );
        let source = &candidate.hit.source;
        let markdown = source.kind.is_markdown(Path::new(&source.path));
        let all = chunk::context_boundaries(body, markdown);
        let start = all.partition_point(|offset| *offset <= requested.start) - 1;
        let end = all.partition_point(|offset| *offset < requested.end);
        let lines = std::iter::once(0)
            .chain(body.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        Ok(Self {
            body,
            matched,
            requested,
            blocks: all[start..=end].to_vec(),
            lines,
        })
    }

    fn choose(
        &self,
        hit: &mut Match,
        allowance: usize,
        expand: bool,
    ) -> Result<Option<(Range<usize>, usize)>> {
        let count = self.blocks.len() - 1;
        let full = self.span(0..count);
        if expand && let Some(cost) = self.preview(hit, full.clone(), allowance)? {
            return Ok(Some((full, cost)));
        }
        let overlap = |index: usize| {
            self.blocks[index + 1]
                .min(self.matched.end)
                .saturating_sub(self.blocks[index].max(self.matched.start))
        };
        let start = self.blocks.partition_point(|at| *at <= self.matched.start) - 1;
        let end = self.blocks.partition_point(|at| *at < self.matched.end);
        let cover = start..end;
        let mut atoms: Vec<_> = cover.clone().collect();
        atoms.sort_by_key(|index| (std::cmp::Reverse(overlap(*index)), *index));
        let fallbacks = atoms.into_iter().map(|index| index..index + 1);
        for anchor in std::iter::once(cover).chain(fallbacks) {
            let span = self.span(anchor.clone());
            let Some(cost) = self.preview(hit, span.clone(), allowance)? else {
                continue;
            };
            let chosen = (span, cost);
            return if expand {
                self.extend(hit, anchor, chosen, allowance).map(Some)
            } else {
                Ok(Some(chosen))
            };
        }
        Ok(None)
    }

    fn extend(
        &self,
        hit: &mut Match,
        anchor: Range<usize>,
        mut best: (Range<usize>, usize),
        allowance: usize,
    ) -> Result<(Range<usize>, usize)> {
        let count = self.blocks.len() - 1;
        let mut low = anchor.len();
        let mut high = count;
        while low < high {
            let width = low + (high - low).div_ceil(2);
            let extra_left = (width - anchor.len()) / 2;
            let from = anchor.start.saturating_sub(extra_left).min(count - width);
            let span = self.span(from..from + width);
            if let Some(cost) = self.preview(hit, span.clone(), allowance)? {
                low = width;
                best = (span, cost);
            } else {
                high = width - 1;
            }
        }
        Ok(best)
    }

    fn span(&self, blocks: Range<usize>) -> Range<usize> {
        self.blocks[blocks.start]..self.blocks[blocks.end]
    }

    fn preview(
        &self,
        hit: &mut Match,
        span: Range<usize>,
        allowance: usize,
    ) -> Result<Option<usize>> {
        if span.len() >= allowance.saturating_mul(4) || self.body[span.clone()].trim().is_empty() {
            return Ok(None);
        }
        self.render(hit, span);
        let mut bytes = ByteCount(1);
        serde_json::to_writer(&mut bytes, hit)?;
        let used = bytes.0.div_ceil(4);
        Ok((used <= allowance).then_some(used))
    }

    fn render(&self, hit: &mut Match, span: Range<usize>) {
        hit.text.clear();
        hit.text.push_str(&self.body[span.clone()]);
        hit.context_window = Some(ContextWindow {
            byte_start: span.start,
            byte_end: span.end,
            line_start: self.lines.partition_point(|offset| *offset <= span.start),
            line_end: self.lines.partition_point(|offset| *offset < span.end),
            truncated: span.start > self.requested.start || span.end < self.requested.end,
        });
        if let Some(explanation) = hit.explanation.as_mut() {
            let overlap = span.start.max(self.matched.start)..span.end.min(self.matched.end);
            explanation.matched_text.clear();
            explanation.matched_text.push_str(&self.body[overlap]);
        }
    }
}

struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::documents::{Kind, MatchedChunk, Provenance};

    fn candidate(rank: usize, parent: &str, body: &str, evidence: &str) -> Ranked {
        let start = body.find(evidence).expect("fixture evidence");
        let matched = MatchedChunk {
            id: format!("chunk-{parent}"),
            parent_id: parent.into(),
            content_hash: "hash".into(),
            parent_hash: "parent-hash".into(),
            heading_path: vec!["Root".into(), "Café".into()],
            line_start: 1,
            line_end: 1,
            byte_start: start,
            byte_end: start + evidence.len(),
        };
        Ranked {
            hit: Match {
                source: Provenance {
                    id: parent.into(),
                    path: format!("docs/{parent}.md"),
                    kind: Kind::Source,
                    timestamp: None,
                    line_start: Some(1),
                    line_end: Some(1),
                    matched_chunk: Some(matched),
                    entry_kind: None,
                    git: None,
                    visibility: None,
                },
                text: String::new(),
                score: 0.75,
                rank,
                explanation: None,
                context_window: None,
            },
            parent: parent.into(),
            chunk: format!("chunk-{parent}"),
            timestamp: None,
            section: Some(0..body.len()),
        }
    }

    #[test]
    fn shares_budget_across_ranked_parents_and_reports_exact_windows() -> Result<()> {
        let large = format!(
            "# Root\n\n## Café\n\nAn attributed UTF-8 answer ⛄.\n\n{}",
            "Another paragraph about contextual rendering.\n\n".repeat(250)
        );
        let brief = "A concise supporting answer.\n";
        let ranked = vec![
            candidate(1, "long", &large, "UTF-8 answer"),
            candidate(2, "brief", brief, "supporting answer"),
        ];
        let (hits, usage) = pack(ranked, 475, ContextMode::Parent, |id| {
            Ok(if id == "long" { &large } else { brief }.to_owned())
        })?;
        assert_eq!(hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(), [1, 2]);
        let mut used = 0;
        for hit in hits {
            let parent = if hit.rank == 1 { &large } else { brief };
            let window = hit.context_window.context("missing context window")?;
            assert_eq!(hit.text, parent[window.byte_start..window.byte_end]);
            assert_eq!(window.truncated, hit.rank == 1);
            let matched = hit.source.matched_chunk.as_ref().context("missing match")?;
            assert_eq!(matched.heading_path, ["Root", "Café"]);
            assert!(window.byte_start <= matched.byte_start && window.byte_end >= matched.byte_end);
            used += (serde_json::to_vec(&hit)?.len() + 1).div_ceil(4);
        }
        assert_eq!(usage.estimated_tokens, used);
        assert!(used <= usage.max_tokens);
        assert_eq!(usage.truncated_results, 1);
        assert_eq!(usage.omitted_candidates, 0);
        Ok(())
    }

    #[test]
    fn fence_stays_whole_and_unaffordable_hits_keep_their_original_rank() -> Result<()> {
        let fence = "```rust\nuse example::Decision;\n```\n";
        let body = format!("# Root\n\n{fence}\n{}\n", "outside focus ".repeat(400));
        let entry = || candidate(1, "code", &body, "example::Decision");
        let (hits, _) = pack(vec![entry()], 200, ContextMode::Chunk, |_| Ok(body.clone()))?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text.trim_end(), fence.trim_end());
        let huge = format!("```rust\n{}\n```\n", "let value = 10;\n".repeat(1_000));
        let brief = "Useful fallback.";
        let ranked = || {
            vec![
                candidate(1, "huge", &huge, "let value"),
                candidate(2, "brief", brief, "fallback"),
            ]
        };
        let parent = |id: &str| Ok(if id == "huge" { &huge } else { brief }.to_owned());
        let (hits, usage) = pack(ranked(), 190, ContextMode::Parent, parent)?;
        assert_eq!(hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(), [2]);
        assert_eq!(usage.omitted_candidates, 1);
        let (hits, usage) = pack(ranked(), 1, ContextMode::Parent, |_| {
            panic!("a tiny allowance cannot pay for metadata")
        })?;
        assert!(hits.is_empty());
        assert_eq!(usage.omitted_candidates, 2);
        assert_eq!(usage.estimated_tokens, 0);
        Ok(())
    }

    #[test]
    fn small_allowance_prefers_high_ranks_over_cheap_late_slots() -> Result<()> {
        let body = "One short matched passage.";
        let ranked = (1..=12)
            .map(|rank| candidate(rank, &format!("memory-{rank:02}"), body, "matched"))
            .collect();
        let (hits, usage) = pack(ranked, 500, ContextMode::Parent, |_| Ok(body.into()))?;
        assert!(hits.len() >= 2);
        assert!(usage.omitted_candidates > 0);
        assert!(
            hits.iter()
                .enumerate()
                .all(|(index, hit)| hit.rank == index + 1)
        );
        Ok(())
    }

    #[test]
    fn preserves_a_fitting_match_across_paragraph_and_fence() -> Result<()> {
        let command = "sparrow replay-cursor reset --from-checkpoint";
        let body = format!(
            "{}\n\n```sh\n{command}\n{}```\n",
            "Recover from the latest validated checkpoint. ".repeat(16),
            "# Record the durable offset before restarting.\n".repeat(5)
        );
        let brief = "A separate match from a supporting document.";
        let mut ranked = vec![candidate(1, "runbook", &body, &body)];
        ranked.extend(
            (2..=9).map(|rank| candidate(rank, &format!("memory-{rank:02}"), brief, "supporting")),
        );
        let (hits, usage) = pack(ranked, 1_100, ContextMode::Parent, |id| {
            Ok(if id == "runbook" { &body } else { brief }.to_owned())
        })?;
        assert!(hits.len() > 1);
        assert!(hits[0].text.contains(command));
        assert_eq!(hits[0].text, body);
        assert!(usage.estimated_tokens <= 1_100);
        Ok(())
    }
}
