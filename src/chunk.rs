use crate::vectors::{self, Embedder};
use anyhow::{Result, ensure};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Strategy {
    #[default]
    Structural,
    Semantic,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub strategy: Strategy,
    pub tokens: usize,
    pub semantic_threshold: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            strategy: Strategy::Structural,
            tokens: 384,
            semantic_threshold: 0.80,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (32..=480).contains(&self.tokens),
            "chunking.tokens must be between 32 and 480"
        );
        ensure!(
            (-1.0..=1.0).contains(&self.semantic_threshold),
            "chunking.semantic_threshold must be between -1 and 1"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Chunk {
    pub span: Range<usize>,
    pub section: Range<usize>,
    pub headings: Vec<String>,
    pub lines: [usize; 2],
}

struct Section {
    span: Range<usize>,
    headings: Vec<String>,
}

pub(crate) fn layout(
    text: &str,
    markdown: bool,
    settings: &Settings,
    model: &mut impl Embedder,
) -> Result<Vec<Chunk>> {
    let mut blocks = Vec::new();
    let sections = if markdown {
        structure(text, &mut blocks)
    } else {
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            blocks.push(offset);
            offset += line.len();
        }
        vec![Section {
            span: 0..text.len(),
            headings: Vec::new(),
        }]
    };
    blocks.sort_unstable();
    blocks.dedup();
    let mut lines = vec![0];
    lines.extend(text.match_indices('\n').map(|(pos, _)| pos + 1));
    let mut out = Vec::new();
    for section in sections {
        if text[section.span.clone()].trim().is_empty() {
            continue;
        }
        let range = section.span.clone();
        let first = blocks.partition_point(|start| *start <= range.start);
        let last = blocks.partition_point(|start| *start < range.end);
        let boundaries = std::iter::once(range.start)
            .chain(blocks[first..last].iter().copied())
            .chain([range.end])
            .collect::<Vec<_>>();
        let mut units = Vec::new();
        for pair in boundaries.windows(2) {
            let unit = pair[0]..pair[1];
            split_unit(text, unit, settings.tokens, model, &mut units)?;
        }
        let breaks = semantic_breaks(text, &units, settings, model)?;
        let mut from = 0;
        while from < units.len() {
            let mut to = from + 1;
            let mut tokens = units[from].1;
            while to < units.len() && !breaks[to] && tokens + units[to].1 <= settings.tokens {
                tokens += units[to].1;
                to += 1;
            }
            let mut span = units[from].0.start..units[to - 1].0.end;
            // Verify the concatenated text using the same tokenizer as inference.
            while to > from + 1 && model.token_offsets(&text[span.clone()])?.len() > settings.tokens
            {
                to -= 1;
                span.end = units[to - 1].0.end;
            }
            if !text[span.clone()].trim().is_empty() {
                out.push(Chunk {
                    lines: [
                        lines.partition_point(|line| *line <= span.start),
                        lines.partition_point(|line| *line < span.end),
                    ],
                    section: range.clone(),
                    headings: section.headings.clone(),
                    span,
                });
            }
            from = to;
        }
    }
    Ok(out)
}

fn structure(text: &str, blocks: &mut Vec<usize>) -> Vec<Section> {
    let mut headings: Vec<(u8, String)> = Vec::new();
    let mut sections = Vec::new();
    let mut section = Section {
        span: 0..0,
        headings: Vec::new(),
    };
    let mut heading = None;
    let mut depth = 0;
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    blocks.push(range.start);
                    if let Tag::Heading { level, .. } = tag {
                        section.span.end = range.start;
                        sections.push(section);
                        heading = Some((level as u8, String::new()));
                        section = Section {
                            span: range.start..0,
                            headings: Vec::new(),
                        };
                    }
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth -= 1;
                if depth == 0
                    && matches!(tag, TagEnd::Heading(_))
                    && let Some((level, title)) = heading.take()
                {
                    headings.retain(|(ancestor, _)| *ancestor < level);
                    headings.push((level, title));
                    section.headings = headings.iter().map(|(_, title)| title.clone()).collect();
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, title)) = &mut heading {
                    title.push_str(&text);
                }
            }
            _ => (),
        }
    }
    section.span.end = text.len();
    sections.push(section);
    sections
}

pub(crate) fn context_boundaries(text: &str, markdown: bool) -> Vec<usize> {
    let mut blocks = vec![0];
    if markdown {
        structure(text, &mut blocks);
    } else {
        blocks.extend(text.match_indices('\n').map(|(offset, _)| offset + 1));
    }
    blocks.push(text.len());
    blocks.sort_unstable();
    blocks.dedup();
    blocks
}

fn split_unit(
    text: &str,
    mut span: Range<usize>,
    budget: usize,
    model: &impl Embedder,
    out: &mut Vec<(Range<usize>, usize)>,
) -> Result<()> {
    let full = &text[span.clone()];
    let offsets = model.token_offsets(full)?;
    if offsets.len() <= budget {
        out.push((span, offsets.len()));
        return Ok(());
    }
    let origin = span.start;
    let mut finger = 0;
    while span.start < span.end {
        let body = &text[span.start..span.end];
        let mut end = offsets.get(finger + budget).map_or(body.len(), |offset| {
            (origin + offset.0)
                .saturating_sub(span.start)
                .min(body.len())
        });
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = body.chars().next().map_or(0, char::len_utf8);
        }
        let mut count = model.token_offsets(&body[..end])?.len();
        while count > budget
            && let Some((shorter, _)) = body[..end].char_indices().next_back()
            && shorter > 0
        {
            end = shorter;
            count = model.token_offsets(&body[..end])?.len();
        }
        ensure!(
            end > 0 && count <= budget,
            "tokenizer could not split text within the token budget"
        );
        out.push((span.start..span.start + end, count));
        span.start += end;
        while offsets
            .get(finger)
            .is_some_and(|offset| origin + offset.0 < span.start)
        {
            finger += 1;
        }
    }
    Ok(())
}

fn semantic_breaks(
    text: &str,
    units: &[(Range<usize>, usize)],
    settings: &Settings,
    model: &mut impl Embedder,
) -> Result<Vec<bool>> {
    let mut breaks = vec![false; units.len()];
    if settings.strategy == Strategy::Structural {
        return Ok(breaks);
    }
    let candidates = units
        .iter()
        .enumerate()
        .filter(|(_, (_, tokens))| *tokens >= 8);
    let mut previous: Option<(usize, Vec<f32>)> = None;
    let mut pending = Vec::new();
    for (index, (span, _)) in candidates {
        pending.push((index, &text[span.clone()]));
        if pending.len() == 16 {
            mark_breaks(
                model,
                &pending,
                &mut previous,
                &mut breaks,
                settings.semantic_threshold,
            )?;
            pending.clear();
        }
    }
    mark_breaks(
        model,
        &pending,
        &mut previous,
        &mut breaks,
        settings.semantic_threshold,
    )?;
    Ok(breaks)
}

fn mark_breaks(
    model: &mut impl Embedder,
    pending: &[(usize, &str)],
    previous: &mut Option<(usize, Vec<f32>)>,
    breaks: &mut [bool],
    threshold: f64,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let passages = pending.iter().map(|(_, text)| *text).collect::<Vec<_>>();
    let embeddings = model.passages(&passages)?;
    ensure!(
        embeddings.len() == pending.len(),
        "wrong number of semantic split embeddings"
    );
    for ((index, _), vector) in pending.iter().zip(embeddings) {
        if let Some((before, last)) = previous.as_ref()
            && *before + 1 == *index
        {
            let norm = vectors::norm(last, model.model().dimensions)?;
            breaks[*index] = vectors::cosine(last, norm, &vector)? < threshold;
        }
        *previous = Some((*index, vector));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors::Model;

    struct Fake;

    impl Embedder for Fake {
        fn model(&self) -> Model {
            Model {
                id: "fixture",
                revision: "1",
                dimensions: 2,
            }
        }

        fn token_offsets(&self, text: &str) -> Result<Vec<(usize, usize)>> {
            Ok(text
                .char_indices()
                .map(|(i, c)| (i, i + c.len_utf8()))
                .collect())
        }

        fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|text| {
                    if text.contains("rust") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    #[test]
    fn preserves_headings_lists_fences_and_unicode_provenance() -> Result<()> {
        let body = format!(
            "# Root\n\nPreamble\n\n## Nested\n\n- first\n- second\n\n```rust\nlet x = 1;\n```\n\n{}\n",
            "日本語".repeat(30)
        );
        let settings = Settings {
            tokens: 32,
            ..Settings::default()
        };
        let chunks = layout(&body, true, &settings, &mut Fake)?;
        assert_eq!(
            chunks
                .iter()
                .map(|item| &body[item.span.clone()])
                .collect::<String>(),
            body
        );
        let fence = chunks
            .iter()
            .find(|item| body[item.span.clone()].contains("let x"))
            .expect("fence");
        assert_eq!(fence.headings, ["Root", "Nested"]);
        assert_eq!(fence.lines, [10, 13]);
        assert!(body[fence.span.clone()].contains("```rust\nlet x = 1;\n```"));
        assert!(
            chunks
                .iter()
                .all(|chunk| body[chunk.span.clone()].chars().count() <= 32)
        );
        assert!(chunks.iter().any(|chunk| chunk.lines == [14, 14]));
        Ok(())
    }

    #[test]
    fn semantic_split_changes_layout_at_topic_boundaries() -> Result<()> {
        let text = "rust memory design\n\nA tropical recipe\n";
        let structural = Settings {
            tokens: 80,
            ..Settings::default()
        };
        let semantic = Settings {
            strategy: Strategy::Semantic,
            ..structural.clone()
        };
        assert_eq!(layout(text, true, &structural, &mut Fake)?.len(), 1);
        assert_eq!(layout(text, true, &semantic, &mut Fake)?.len(), 2);
        Ok(())
    }
}
