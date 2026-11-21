use super::{Item, Pair};
use crate::documents::{Document, MatchedChunk};
use crate::entries::EntryKind;
use crate::repo::hash;
use chrono::DateTime;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Serialize)]
pub(crate) struct Group {
    pub id: String,
    pub detection: &'static str,
    pub reference_id: String,
    pub reference: Item,
    pub members: Vec<Member>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_hash: Option<String>,
    pub suggested_forget_ids: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct Member {
    pub relation_to_reference: &'static str,
    pub scope: &'static str,
    pub similarity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_identical: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_chunk: Option<MatchedChunk>,
    #[serde(flatten)]
    pub item: Item,
}

fn reference_order(left: (&str, Option<&str>), right: (&str, Option<&str>)) -> Ordering {
    let parse = |timestamp: Option<&str>| {
        timestamp.and_then(|text| DateTime::parse_from_rfc3339(text).ok())
    };
    let (a, b) = (parse(left.1), parse(right.1));
    a.is_none()
        .cmp(&b.is_none())
        .then_with(|| a.cmp(&b))
        .then_with(|| left.0.cmp(right.0))
}

pub(super) fn exact(
    digest: &str,
    documents: &[&Document],
    unprotected: &impl Fn(&Document) -> bool,
) -> Group {
    let mut entries = documents.to_vec();
    entries.sort_by(|left, right| {
        let (a, b) = (&left.provenance, &right.provenance);
        reference_order(
            (&a.id, a.timestamp.as_deref()),
            (&b.id, b.timestamp.as_deref()),
        )
    });
    let reference = entries[0];
    let keep = Item::with_text(reference, &reference.text, None);
    let eligible =
        |doc: &Document| doc.provenance.entry_kind == Some(EntryKind::Memory) && unprotected(doc);
    let keep_eligible = eligible(reference);
    let mut group = Group {
        id: format!("exact:{digest}"),
        detection: "exact",
        reference_id: keep.id.clone(),
        reference: keep,
        members: Vec::new(),
        normalized_hash: Some(digest.into()),
        suggested_forget_ids: Vec::new(),
    };
    for doc in &entries[1..] {
        let byte_identical = doc.text == reference.text;
        if byte_identical && keep_eligible && eligible(doc) {
            group.suggested_forget_ids.push(doc.provenance.id.clone());
        }
        group.members.push(Member {
            relation_to_reference: "exact",
            scope: "entry",
            similarity: 1.0,
            byte_identical: Some(byte_identical),
            reference_chunk: None,
            item: Item::with_text(doc, &doc.text, None),
        });
    }
    group
}

pub(super) fn semantic(
    pairs: &[Pair],
    parents: &BTreeMap<&str, &Document>,
    selected: Option<&str>,
) -> Vec<Group> {
    let mut groups = Vec::<Group>::new();
    let mut positions = BTreeMap::new();
    for pair in pairs {
        let (a, b) = (&pair.left, &pair.right);
        let a_first = selected.map_or_else(
            || {
                reference_order(
                    (&a.id, a.timestamp.as_deref()),
                    (&b.id, b.timestamp.as_deref()),
                )
                .is_le()
            },
            |id| id == a.id,
        );
        let (keep, member) = if a_first { (a, b) } else { (b, a) };
        let index = *positions.entry(&keep.id).or_insert_with(|| {
            let index = groups.len();
            let parent = parents[keep.id.as_str()];
            let reference = Item::with_text(parent, &parent.text, None);
            groups.push(Group {
                id: format!("semantic:{}", hash(keep.id.as_bytes())),
                detection: "semantic",
                reference_id: keep.id.clone(),
                reference,
                members: Vec::new(),
                normalized_hash: None,
                suggested_forget_ids: Vec::new(),
            });
            index
        });
        groups[index].members.push(Member {
            relation_to_reference: "semantic",
            scope: pair.scope,
            similarity: pair.similarity,
            byte_identical: None,
            reference_chunk: keep.chunk.clone(),
            item: member.clone(),
        });
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::documents::{Kind, Provenance};

    #[test]
    fn semantic_edges_do_not_imply_transitive_matches() {
        let document = |id: &str, timestamp: &str| Document {
            provenance: Provenance {
                id: id.into(),
                kind: Kind::Memory,
                path: format!("{id}.md"),
                timestamp: Some(timestamp.into()),
                line_start: None,
                line_end: None,
                matched_chunk: None,
                entry_kind: Some(EntryKind::Memory),
                git: None,
                visibility: None,
            },
            text: id.into(),
            hash: hash(id.as_bytes()),
            markdown: true,
        };
        let a = document("a", "2026-01-01T14:00:00+02:00");
        let b = document("b", "2026-01-01T13:00:00+00:00");
        let c = document("c", "2026-01-01T14:00:00+00:00");
        let pair = |a: &Document, b: &Document| Pair {
            detection: "semantic",
            scope: "chunk",
            similarity: 0.95,
            left: Item::with_text(a, &a.text, None),
            right: Item::with_text(b, &b.text, None),
        };
        let parents = BTreeMap::from([("a", &a), ("b", &b), ("c", &c)]);
        let pairs = [pair(&a, &b), pair(&b, &c)];
        let groups = semantic(&pairs, &parents, None);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].reference_id, "a");
        assert_eq!(groups[0].members[0].item.id, "b");
        assert_eq!(groups[1].reference_id, "b");
        assert_eq!(groups[1].members[0].item.id, "c");
        assert!(
            groups
                .iter()
                .all(|group| group.suggested_forget_ids.is_empty())
        );
        let selected = semantic(&pairs, &parents, Some("b"));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].reference_id, "b");
        assert_eq!(selected[0].members.len(), 2);
    }
}
