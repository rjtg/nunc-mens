use crate::documents::{Document, MatchedChunk, Snapshot};
use crate::index::Index;
use crate::ledger::Guard;
use crate::model::{self, LocalModel, MODEL};
use crate::repo::{Repo, hash};
use crate::vectors::{self, Embedder};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

pub(crate) const DEFAULT_THRESHOLD: f64 = 0.90;

#[derive(Serialize)]
pub(crate) struct Report {
    pub warnings: Vec<String>,
    pub matches: Vec<Pair>,
}

#[derive(Serialize)]
pub(crate) struct Pair {
    pub detection: &'static str,
    pub scope: &'static str,
    pub similarity: f64,
    pub left: Item,
    pub right: Item,
}

#[derive(Clone, Serialize)]
pub(crate) struct Item {
    pub id: String,
    pub path: String,
    pub timestamp: Option<String>,
    pub excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk: Option<MatchedChunk>,
}

impl Item {
    fn with_text(parent: &Document, text: &str, chunk: Option<MatchedChunk>) -> Self {
        let p = &parent.provenance;
        Self {
            id: p.id.clone(),
            path: p.path.clone(),
            timestamp: p.timestamp.clone(),
            excerpt: excerpt(text),
            chunk,
        }
    }
}

fn excerpt(text: &str) -> String {
    let mut chars = text.trim().chars();
    let mut result: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn normalized(text: &str) -> String {
    hash(
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .as_bytes(),
    )
}

fn valid_threshold(threshold: f64) -> Result<()> {
    ensure!(
        (0.0..=1.0).contains(&threshold),
        "duplicate threshold must be between 0 and 1"
    );
    Ok(())
}

struct Top {
    limit: usize,
    pairs: Vec<Pair>,
}

impl Top {
    fn offer(&mut self, pair: Pair) {
        let cmp = |entry: &Pair| {
            (entry.detection != "exact")
                .cmp(&(pair.detection != "exact"))
                .then_with(|| pair.similarity.total_cmp(&entry.similarity))
                .then_with(|| entry.left.id.cmp(&pair.left.id))
                .then_with(|| entry.right.id.cmp(&pair.right.id))
        };
        if let Some(index) = self
            .pairs
            .iter()
            .position(|entry| entry.left.id == pair.left.id && entry.right.id == pair.right.id)
        {
            if cmp(&self.pairs[index]).is_le() {
                return;
            }
            self.pairs.remove(index);
        }
        let index = self.pairs.partition_point(|entry| cmp(entry).is_lt());
        if index < self.limit {
            self.pairs.insert(index, pair);
            if self.pairs.len() > self.limit {
                self.pairs.pop();
            }
        }
    }
}

fn eligible_parents<'a>(
    guard: &Guard<'_>,
    snapshot: &'a Snapshot,
    raw: bool,
) -> Result<BTreeMap<&'a str, &'a Document>> {
    let archives = guard.ledger.archives()?;
    Ok(snapshot
        .documents
        .iter()
        .filter_map(|doc| {
            let id = doc.provenance.id.as_str();
            let hidden =
                uuid::Uuid::parse_str(id).is_ok_and(|id| guard.ledger.hidden(&archives, id));
            (raw || !hidden).then_some((id, doc))
        })
        .collect())
}

pub(crate) fn find(
    repo: &Repo,
    id: Option<&str>,
    threshold: f64,
    limit: usize,
    exact: bool,
    raw: bool,
) -> Result<Report> {
    valid_threshold(threshold)?;
    let guard = Guard::acquire(repo, false)?;
    let snapshot = Snapshot::read(repo)?;
    let parents = eligible_parents(&guard, &snapshot, raw)?;
    ensure!(
        id.is_none_or(|id| parents.contains_key(id)),
        "entry or source is unavailable; use --raw to include archived entries"
    );
    let hashes: HashMap<_, _> = parents
        .iter()
        .map(|(&id, doc)| (id, normalized(&doc.text)))
        .collect();
    let mut groups = BTreeMap::<&str, Vec<&Document>>::new();
    for (&id, doc) in &parents {
        groups.entry(hashes[id].as_str()).or_default().push(doc);
    }
    let mut top = Top {
        limit,
        pairs: Vec::new(),
    };
    for group in groups.values().filter(|members| members.len() > 1) {
        for (position, left) in group.iter().enumerate() {
            for right in &group[position + 1..] {
                if id.is_some_and(|id| id != left.provenance.id && id != right.provenance.id) {
                    continue;
                }
                top.offer(exact_pair(left, right));
            }
        }
    }
    let mut warnings = Vec::new();
    if !exact {
        let semantic = || -> Result<_> {
            model::check_cache(repo)?;
            Index::for_repo(repo)?
                .read(MODEL)?
                .context("index unavailable; run nunc reindex")
        };
        match semantic() {
            Ok(reader) => {
                if reader.digest != snapshot.digest {
                    warnings.push("Index needs refresh; semantic results may be incomplete".into());
                }
                let mut indexed = Vec::new();
                reader.scan(
                    None,
                    |_| true,
                    |row, vector| {
                        let parent = row.parent_id()?;
                        if let Some(doc) = parents.get(parent) {
                            let (_, text) = row.document()?;
                            let chunk = row.matched()?.context("missing chunk provenance")?;
                            if chunk.parent_hash == doc.hash {
                                indexed.push((
                                    parent.to_owned(),
                                    vector.to_vec(),
                                    Item::with_text(doc, &text, Some(chunk)),
                                    text.len() == doc.text.len(),
                                ));
                            }
                        }
                        Ok(())
                    },
                )?;
                for (pos, (left, vector, item, whole)) in indexed.iter().enumerate() {
                    let norm = vectors::norm(vector, MODEL.dimensions)?;
                    for (right, candidate, other, other_whole) in &indexed[pos + 1..] {
                        if left == right
                            || hashes[left.as_str()] == hashes[right.as_str()]
                            || id.is_some_and(|id| id != left && id != right)
                        {
                            continue;
                        }
                        let similarity = vectors::cosine(vector, norm, candidate)?;
                        if similarity >= threshold {
                            let (a, b) = if left < right {
                                (item, other)
                            } else {
                                (other, item)
                            };
                            top.offer(Pair {
                                detection: "semantic",
                                scope: if *whole && *other_whole {
                                    "entry"
                                } else {
                                    "chunk"
                                },
                                similarity,
                                left: a.clone(),
                                right: b.clone(),
                            });
                        }
                    }
                }
            }
            Err(error) => warnings.push(format!("Semantic duplicate check unavailable: {error:#}")),
        }
    }
    Ok(Report {
        warnings,
        matches: top.pairs,
    })
}

fn exact_pair(left: &Document, right: &Document) -> Pair {
    Pair {
        detection: "exact",
        scope: "entry",
        similarity: 1.0,
        left: Item::with_text(left, &left.text, None),
        right: Item::with_text(right, &right.text, None),
    }
}

pub(crate) fn check_write(repo: &Repo, text: &str, threshold: f64) -> Result<Report> {
    valid_threshold(threshold)?;
    let guard = Guard::acquire(repo, false)?;
    let snapshot = Snapshot::read(repo)?;
    let digest = normalized(text);
    let incoming = || Item {
        id: "incoming".into(),
        path: String::new(),
        timestamp: None,
        excerpt: excerpt(text),
        chunk: None,
    };
    let mut top = Top {
        limit: 5,
        pairs: Vec::new(),
    };
    let parents = eligible_parents(&guard, &snapshot, false)?;
    for parent in parents.values() {
        if normalized(&parent.text) == digest {
            top.offer(Pair {
                detection: "exact",
                scope: "entry",
                similarity: 1.0,
                left: incoming(),
                right: Item::with_text(parent, &parent.text, None),
            });
        }
    }
    let mut warnings = Vec::new();
    let semantic = || -> Result<()> {
        model::check_cache(repo)?;
        let reader = Index::for_repo(repo)?
            .read(MODEL)?
            .context("index unavailable; run nunc reindex")?;
        if reader.digest != snapshot.digest {
            warnings.push("Index needs refresh; semantic results may be incomplete".into());
        }
        let mut model = LocalModel::load(repo)?;
        let mut layout = snapshot.chunking.clone();
        layout.strategy = crate::chunk::Strategy::Structural;
        let chunks = crate::chunk::layout(text, true, &layout, &mut model)?;
        for chunk in &chunks {
            let piece = &text[chunk.span.clone()];
            let mut vectors = model.passages(&[piece])?;
            let vector = vectors.pop().context("no embedding for incoming passage")?;
            let magnitude = vectors::norm(&vector, MODEL.dimensions)?;
            reader.scan(
                None,
                |_| true,
                |row, indexed| {
                    let parent = row.parent_id()?;
                    if let Some(doc) = parents.get(parent) {
                        let similarity = vectors::cosine(&vector, magnitude, indexed)?;
                        let chunk = row.matched()?.context("missing chunk provenance")?;
                        if similarity >= threshold && chunk.parent_hash == doc.hash {
                            let (_, content) = row.document()?;
                            top.offer(Pair {
                                detection: "semantic",
                                scope: if chunks.len() == 1 && content.len() == doc.text.len() {
                                    "entry"
                                } else {
                                    "chunk"
                                },
                                similarity,
                                left: incoming(),
                                right: Item::with_text(doc, &content, Some(chunk)),
                            });
                        }
                    }
                    Ok(())
                },
            )?;
        }
        Ok(())
    };
    let mut semantic = semantic;
    if let Err(error) = semantic() {
        warnings.push(format!("Semantic duplicate check unavailable: {error:#}"));
    }
    Ok(Report {
        warnings,
        matches: top.pairs,
    })
}
