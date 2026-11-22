use crate::documents::Kind;
use crate::index::Index;
use crate::model::MODEL;
use crate::repo::Repo;
use crate::timeline::{Anchor, DatedEvent, Event};
use crate::vectors;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const DEFAULT_THRESHOLD: f64 = 0.90;

#[derive(Serialize)]
pub(crate) struct Report {
    pub mode_used: &'static str,
    pub method: &'static str,
    pub heuristic: bool,
    pub threshold: f64,
    pub model: vectors::Model,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    pub topics: Vec<Topic>,
    pub ungrouped: Vec<Ungrouped>,
}

#[derive(Serialize)]
pub(crate) struct Scope {
    pub anchor: Anchor,
    pub window_seconds: i64,
}

#[derive(Serialize)]
pub(crate) struct Topic {
    pub label: String,
    pub representative_id: String,
    pub minimum_similarity: f64,
    pub members: Vec<Event>,
}

#[derive(Serialize)]
pub(crate) struct Ungrouped {
    #[serde(flatten)]
    pub event: Event,
    pub reason: &'static str,
}

pub(crate) fn validate_threshold(threshold: f64) -> Result<()> {
    ensure!(
        (0.0..=1.0).contains(&threshold),
        "topic threshold must be between 0 and 1"
    );
    Ok(())
}

pub(crate) fn group(
    repo: &Repo,
    mut events: Vec<DatedEvent>,
    threshold: f64,
    scope: Option<Scope>,
) -> Result<Report> {
    validate_threshold(threshold)?;
    // Around selection admits the nearest events; clustering always visits the
    // selected records chronologically for a stable seed order.
    events.sort_by(|a, b| {
        a.at.cmp(&b.at)
            .then_with(|| a.event.source.id.cmp(&b.event.source.id))
    });
    let mut warnings = Vec::new();
    let vectors = match indexed_vectors(repo, &events) {
        Ok(vectors) => vectors,
        Err(error) => {
            warnings.push(format!("Topic embeddings unavailable: {error:#}"));
            vec![None; events.len()]
        }
    };

    let present: Vec<_> = vectors.iter().map(Option::is_some).collect();
    let scores = similarities(&vectors)?;
    let (groups, ambiguous) = clusters(&scores, &present, threshold);
    let mut events: Vec<_> = events.into_iter().map(|dated| Some(dated.event)).collect();
    let mut topics = Vec::new();
    for group in groups {
        let representative = group
            .iter()
            .copied()
            .max_by(|&a, &b| {
                let sum = |member: usize| {
                    group
                        .iter()
                        .map(|&other| scores[member][other])
                        .sum::<f64>()
                };
                sum(a).total_cmp(&sum(b)).then_with(|| b.cmp(&a))
            })
            .context("topic has no representative")?;
        let event = events[representative]
            .as_ref()
            .context("missing topic representative")?;
        let representative_id = event.source.id.clone();
        let label = event.title.clone();
        let minimum_similarity = group
            .iter()
            .enumerate()
            .flat_map(|(index, &a)| group[index + 1..].iter().map(move |&b| (a, b)))
            .map(|(a, b)| scores[a][b])
            .fold(1.0, f64::min);
        let members = group
            .into_iter()
            .map(|slot| events[slot].take().context("activity was already assigned"))
            .collect::<Result<_>>()?;
        topics.push(Topic {
            label,
            representative_id,
            minimum_similarity,
            members,
        });
    }
    let ungrouped = events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| {
            Some(Ungrouped {
                event: event?,
                reason: if !present[index] {
                    "missing-embedding"
                } else if ambiguous.contains(&index) {
                    "ambiguous"
                } else {
                    "no-close-topic"
                },
            })
        })
        .collect();
    Ok(Report {
        mode_used: "topics",
        method: "chronological-complete-link",
        heuristic: true,
        threshold,
        model: MODEL,
        warnings,
        scope,
        topics,
        ungrouped,
    })
}

fn indexed_vectors(repo: &Repo, events: &[DatedEvent]) -> Result<Vec<Option<Vec<f32>>>> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let reader = Index::for_repo(repo)?
        .read(MODEL)?
        .context("index is absent; run nunc reindex")?;
    let paths: BTreeSet<_> = events
        .iter()
        .map(|event| event.event.source.path.as_str())
        .collect();
    let by_id: BTreeMap<_, _> = events
        .iter()
        .enumerate()
        .map(|(index, event)| (event.event.source.id.as_str(), index))
        .collect();
    let mut chunks = vec![BTreeMap::<String, Vec<f32>>::new(); events.len()];
    reader.scan(
        Some(Kind::Memory),
        |path| paths.contains(path),
        |row, vector| {
            let Some(&index) = by_id.get(row.parent_id()?) else {
                return Ok(());
            };
            let Some(chunk) = row.matched()? else {
                return Ok(());
            };
            if chunk.parent_hash == events[index].content_hash {
                chunks[index]
                    .entry(chunk.content_hash)
                    .or_insert_with(|| vector.to_vec());
            }
            Ok(())
        },
    )?;
    chunks
        .into_iter()
        .map(|chunks| {
            if chunks.is_empty() {
                return Ok(None);
            }
            // Unit-normalize each distinct passage before averaging in hash order.
            let mut sum = vec![0.0f64; MODEL.dimensions];
            for vector in chunks.values() {
                let norm = vectors::norm(vector, MODEL.dimensions)?;
                for (value, component) in sum.iter_mut().zip(vector) {
                    *value += f64::from(*component) / norm;
                }
            }
            let center: Vec<_> = sum
                .into_iter()
                .map(|value| (value / chunks.len() as f64) as f32)
                .collect();
            Ok(vectors::norm(&center, MODEL.dimensions)
                .is_ok()
                .then_some(center))
        })
        .collect()
}

fn similarities(vectors: &[Option<Vec<f32>>]) -> Result<Vec<Vec<f64>>> {
    let mut scores = vec![vec![0.0; vectors.len()]; vectors.len()];
    for (index, vector) in vectors.iter().enumerate() {
        let Some(vector) = vector else { continue };
        let norm = vectors::norm(vector, MODEL.dimensions)?;
        scores[index][index] = 1.0;
        for (other, candidate) in vectors.iter().enumerate().skip(index + 1) {
            if let Some(candidate) = candidate {
                let score = vectors::cosine(vector, norm, candidate)?;
                scores[index][other] = score;
                scores[other][index] = score;
            }
        }
    }
    Ok(scores)
}

fn clusters(
    scores: &[Vec<f64>],
    present: &[bool],
    threshold: f64,
) -> (Vec<Vec<usize>>, BTreeSet<usize>) {
    let close = |entry: usize, group: &[usize]| {
        group
            .iter()
            .all(|&member| scores[entry][member] >= threshold)
    };
    let mut groups = Vec::<Vec<usize>>::new();
    let mut ambiguous = BTreeSet::new();
    for (index, &embedded) in present.iter().enumerate() {
        if !embedded {
            continue;
        }
        let mut choices = groups
            .iter()
            .enumerate()
            .filter(|(_, members)| close(index, members));
        match (choices.next(), choices.next()) {
            (None, _) => groups.push(vec![index]),
            (Some((slot, _)), None) => groups[slot].push(index),
            _ => {
                ambiguous.insert(index);
            }
        }
    }
    // A later group can reveal that an earlier member was ambiguous. Compare
    // against the original groups so removals cannot change other decisions.
    for (index, members) in groups.iter().enumerate() {
        for &member in members {
            if groups
                .iter()
                .enumerate()
                .any(|(other, group)| other != index && group.len() > 1 && close(member, group))
            {
                ambiguous.insert(member);
            }
        }
    }
    for members in &mut groups {
        members.retain(|member| !ambiguous.contains(member));
    }
    groups.retain(|members| members.len() > 1);
    (groups, ambiguous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_workstreams_separate_and_rejects_early_and_late_bridges() {
        let mut scores = vec![vec![0.30; 8]; 8];
        for (i, j, score) in [
            (0, 2, 0.96),
            (3, 4, 0.97),
            (0, 1, 0.95),
            (2, 1, 0.94),
            (3, 1, 0.94),
            (4, 1, 0.95),
            (0, 5, 0.92),
            (2, 5, 0.93),
            (1, 5, 0.92),
            (3, 5, 0.94),
            (4, 5, 0.92),
        ] {
            scores[i][j] = score;
            scores[j][i] = score;
        }
        let (groups, ambiguous) = clusters(
            &scores,
            &[true, true, true, true, true, true, false, true],
            0.90,
        );
        assert_eq!(groups, [vec![0, 2], vec![3, 4]]);
        assert_eq!(ambiguous, BTreeSet::from([1, 5]));
    }
}
