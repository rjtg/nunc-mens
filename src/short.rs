use crate::chunk::offline_token_offsets;
use crate::documents;
use crate::ledger::Guard;
use crate::repo::Repo;
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use uuid::Uuid;

pub(crate) const DEFAULT_MAX_TOKENS: usize = 50;

#[derive(Serialize)]
pub(crate) struct Hit {
    pub id: Uuid,
    pub tokens: usize,
    pub preview: String,
    pub path: PathBuf,
}

pub(crate) fn find(repo: &Repo, max_tokens: usize) -> Result<Vec<Hit>> {
    let guard = Guard::acquire(repo, false)?;
    let archives = guard.ledger.archives()?;
    let mut hits = Vec::new();
    for document in documents::memories(repo, &guard.ledger)? {
        let id = Uuid::parse_str(&document.provenance.id)?;
        if guard.ledger.hidden(&archives, id) {
            continue;
        }
        let tokens = offline_token_offsets(&document.text)
            .take(max_tokens.saturating_add(1))
            .count();
        if tokens <= max_tokens {
            hits.push(Hit {
                id,
                tokens,
                preview: preview(&document.text),
                path: document.provenance.path.into(),
            });
        }
    }
    hits.sort_unstable_by_key(|hit| (hit.tokens, hit.id));
    Ok(hits)
}

fn preview(text: &str) -> String {
    let mut chars = text
        .split_whitespace()
        .enumerate()
        .flat_map(|(index, word)| (index > 0).then_some(' ').into_iter().chain(word.chars()))
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        });
    let mut preview = chars.by_ref().take(100).collect::<String>();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}
