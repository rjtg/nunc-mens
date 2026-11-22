use super::{
    INDEX_NAME, Index, VERSION, check, chunk_rows, existing_private_file, offline_layout_key, open,
    semantic_ready,
};
use crate::chunk::Chunk;
use crate::documents::{Document, MatchedChunk, Snapshot};
use crate::entries::{self, EntryKind};
use crate::fs::{existing_lockfile, lockfile, private_dir};
use crate::ledger::{Guard, Ledger};
use crate::model::{self, LocalModel, MODEL};
use crate::repo::{Repo, hash};
use crate::vectors;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use uuid::Uuid;

#[derive(Default, Serialize)]
pub(crate) struct Report {
    pub index_state: &'static str,
    pub model_state: &'static str,
    pub action: &'static str,
    pub durable_parents: usize,
    pub indexed_parents: usize,
    pub indexed_chunks: usize,
    pub findings: BTreeMap<&'static str, Finding>,
}

#[derive(Serialize)]
pub(crate) struct Finding {
    pub severity: &'static str,
    pub count: usize,
    pub examples: Vec<String>,
}

impl Report {
    fn issue(&mut self, code: &'static str, severity: &'static str, example: impl Into<String>) {
        let finding = self.findings.entry(code).or_insert_with(|| Finding {
            severity,
            count: 0,
            examples: Vec::new(),
        });
        finding.count += 1;
        if finding.examples.len() < 5 {
            finding.examples.push(example.into());
        }
    }

    pub fn print(&self, out: &mut impl Write, json: bool) -> Result<()> {
        if json {
            serde_json::to_writer(&mut *out, self)?;
            writeln!(out)?;
            return Ok(());
        }
        writeln!(
            out,
            "Index {} · model {} · {}",
            self.index_state, self.model_state, self.action
        )?;
        writeln!(
            out,
            "{} durable parents, {} indexed parents, {} indexed chunks",
            self.durable_parents, self.indexed_parents, self.indexed_chunks
        )?;
        for (code, finding) in &self.findings {
            writeln!(out, "{} {} ({})", finding.severity, code, finding.count)?;
            for example in &finding.examples {
                writeln!(out, "  {example}")?;
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Drift {
    parents: BTreeSet<String>,
    chunks: BTreeSet<String>,
}

struct Audit {
    report: Report,
    snapshot: Option<Snapshot>,
    drift: Drift,
}

pub(crate) fn run(repo: &Repo, repair: bool, dry_run: bool) -> Result<Report> {
    let index = Index::for_repo(repo)?;
    // An audit never creates lock files. Existing writers take the memory lock
    // before the index lock. A real repair takes both in the same order.
    let write = repair && !dry_run;
    let guard = if write {
        Some(Guard::acquire(repo, true)?)
    } else {
        None
    };
    let memory_lock = if write {
        None
    } else {
        existing_lockfile(&repo.local().join("memory.lock"))?
    };
    if let Some(lock) = &memory_lock {
        lock.lock_shared()?;
    }
    let path = index.path();
    let present = existing_private_file(&path)?;
    let index_lock = if write {
        index.prepare()?;
        let lock = lockfile(&index.dir.join("index.lock"))?;
        lock.lock()?;
        Some(lock)
    } else if present {
        private_dir(&index.dir)?;
        let lock = existing_lockfile(&index.dir.join("index.lock"))?
            .context("index has no lock file; run nunc repair")?;
        lock.lock_shared()?;
        Some(lock)
    } else {
        None
    };

    let cache = model::check_cache(repo);
    let mut audit = inspect(repo, &path, existing_private_file(&path)?, cache.is_ok())?;
    if audit.report.action == "rebuild_index"
        && let Err(error) = &cache
    {
        audit
            .report
            .issue("MODEL_UNAVAILABLE", "warning", format!("{error:#}"));
    }
    if write && audit.report.action == "rebuild_index" {
        let snapshot = audit.snapshot.take().context("missing durable snapshot")?;
        match LocalModel::load(repo) {
            Ok(mut model) => {
                index.rebuild(
                    &path,
                    snapshot,
                    &mut model,
                    existing_private_file(&path)?,
                    false,
                )?;
                audit = inspect(repo, &path, true, true)?;
                audit.report.action = "rebuilt_index";
            }
            Err(error) => {
                if cache.is_ok() {
                    audit
                        .report
                        .issue("MODEL_UNAVAILABLE", "warning", format!("{error:#}"));
                }
                audit.report.action = "embeddings_pending";
                if !audit.drift.parents.is_empty() || !audit.drift.chunks.is_empty() {
                    let mut connection = open(&path, false)?;
                    prune(&mut connection, &audit.drift)?;
                    audit.report.action = "removed_stale_rows_embeddings_pending";
                }
            }
        }
    }
    drop(index_lock);
    drop(guard);
    drop(memory_lock);
    Ok(audit.report)
}

fn inspect(
    repo: &Repo,
    path: &std::path::Path,
    present: bool,
    model_cached: bool,
) -> Result<Audit> {
    let mut report = Report {
        index_state: if present { "present" } else { "missing" },
        model_state: if model_cached {
            "cached"
        } else {
            "unavailable"
        },
        action: "none",
        ..Report::default()
    };
    let mut drift = Drift::default();
    let (ledger, snapshot) =
        match Ledger::read(repo).and_then(|ledger| Ok((ledger, Snapshot::read(repo)?))) {
            Ok(pair) => pair,
            Err(error) => {
                report.issue("DURABLE_INPUT_INVALID", "error", error.to_string());
                report.action = "inspect_durable_inputs";
                return Ok(Audit {
                    report,
                    snapshot: None,
                    drift,
                });
            }
        };
    report.durable_parents = snapshot.documents.len();
    let parents: BTreeMap<_, _> = snapshot
        .documents
        .iter()
        .map(|document| (document.provenance.id.as_str(), document))
        .collect();
    check_lineage(repo, &ledger, &parents, &mut report)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = path.with_file_name(format!("{INDEX_NAME}{suffix}"));
        match std::fs::symlink_metadata(&sidecar) {
            Ok(_) => report.issue(
                "UNEXPECTED_SQLITE_SIDECAR",
                "error",
                format!("{INDEX_NAME}{suffix}"),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect SQLite sidecar"),
        }
    }
    if report.findings.contains_key("UNEXPECTED_SQLITE_SIDECAR") {
        report.index_state = "unusable";
        report.action = "inspect_sqlite_sidecars";
        return Ok(Audit {
            report,
            snapshot: Some(snapshot),
            drift,
        });
    }

    if !present {
        report.issue("INDEX_MISSING", "warning", "no disposable index");
        report.action = "rebuild_index";
    } else if let Err(error) = open(path, true).and_then(|conn| {
        audit_rows(
            &conn,
            repo,
            &snapshot,
            &ledger,
            &parents,
            &mut report,
            &mut drift,
        )
    }) {
        report.index_state = "unusable";
        report.issue("INDEX_UNREADABLE", "error", error.to_string());
        report.action = "rebuild_index";
        drift = Drift::default();
    }
    if report.findings.contains_key("LINEAGE_TARGET_MISSING")
        || report.findings.contains_key("LINEAGE_TARGET_KIND")
        || report
            .findings
            .contains_key("MANUAL_ARCHIVE_TARGET_MISSING")
    {
        report.action = "inspect_durable_inputs";
    }
    Ok(Audit {
        report,
        snapshot: Some(snapshot),
        drift,
    })
}

fn check_lineage(
    repo: &Repo,
    ledger: &Ledger,
    parents: &BTreeMap<&str, &Document>,
    report: &mut Report,
) -> Result<()> {
    for (summary, sources) in ledger.lineage() {
        if !ledger.forgotten(summary) {
            match parents.get(summary.to_string().as_str()) {
                None => report.issue("LINEAGE_TARGET_MISSING", "error", summary.to_string()),
                Some(parent) if parent.provenance.entry_kind != Some(EntryKind::Consolidation) => {
                    report.issue("LINEAGE_TARGET_KIND", "error", summary.to_string());
                }
                Some(_) => (),
            }
        }
        for source in sources {
            if !ledger.forgotten(*source) && !parents.contains_key(source.to_string().as_str()) {
                report.issue("LINEAGE_SOURCE_MISSING", "warning", source.to_string());
            }
        }
    }
    for id in ledger.manual_archives() {
        if !parents.contains_key(id.to_string().as_str()) {
            report.issue("MANUAL_ARCHIVE_TARGET_MISSING", "error", id.to_string());
        }
    }
    for id in ledger.tombstones() {
        match std::fs::symlink_metadata(entries::path(repo, id)) {
            Ok(_) => report.issue("FORGET_FILE_PENDING", "warning", id.to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn audit_rows(
    conn: &Connection,
    repo: &Repo,
    snapshot: &Snapshot,
    ledger: &Ledger,
    parents: &BTreeMap<&str, &Document>,
    report: &mut Report,
    drift: &mut Drift,
) -> Result<()> {
    let integrity: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if integrity != "ok" {
        anyhow::bail!("SQLite quick_check reported damage");
    }
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != VERSION {
        report.issue("INDEX_SCHEMA_MISMATCH", "warning", version.to_string());
        report.action = "rebuild_index";
        return Ok(());
    }
    let digest = match check(conn, repo, MODEL) {
        Ok(value) => value,
        Err(error) => {
            report.issue("INDEX_METADATA_MISMATCH", "error", error.to_string());
            report.action = "rebuild_index";
            return Ok(());
        }
    };
    if digest != snapshot.digest {
        report.issue("SNAPSHOT_MISMATCH", "warning", "durable snapshot changed");
        report.action = "rebuild_index";
    }
    let strategy: String = conn.query_row(
        "SELECT chunking FROM metadata WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if strategy != serde_json::to_string(&snapshot.chunking.strategy)? {
        report.issue(
            "INDEX_STRATEGY_MISMATCH",
            "warning",
            "configured chunk strategy differs from indexed strategy",
        );
        report.action = "rebuild_index";
    }
    let layout_key = hash(&serde_json::to_vec(&snapshot.chunking)?);
    let offline_key = offline_layout_key(&snapshot.chunking)?;
    let ready = semantic_ready(conn)?;
    if !ready {
        report.issue(
            "EMBEDDINGS_PENDING",
            "warning",
            "keyword index is available; run nunc reindex with the model for semantic search",
        );
    }
    let mut layouts = BTreeMap::new();
    let mut parent_query =
        conn.prepare("SELECT id,text,revision,layout_key,layout,origin FROM parents")?;
    let mut rows = parent_query.query([])?;
    while let Some(row) = rows.next()? {
        report.indexed_parents += 1;
        let id: String = row.get(0)?;
        let Some(parent) = parents.get(id.as_str()) else {
            report.issue(
                parent_code(ledger, &id, "ORPHAN_PARENT"),
                "error",
                id.clone(),
            );
            drift.parents.insert(id);
            continue;
        };
        let origin =
            serde_json::to_string(&(&parent.provenance.entry_kind, &parent.provenance.git))?;
        let layout = serde_json::from_str::<Vec<Chunk>>(row.get_ref(4)?.as_str()?);
        let stored_key = row.get_ref(3)?.as_str()?;
        if row.get_ref(1)?.as_str()? != parent.text
            || row.get_ref(2)?.as_str()? != parent.hash
            || (stored_key != layout_key && (ready || stored_key != offline_key))
            || row.get_ref(5)?.as_str()? != origin
            || layout.as_ref().is_err()
            || !valid_layout(&parent.text, layout.as_deref().unwrap_or_default())
        {
            report.issue("STALE_PARENT", "error", id.clone());
            drift.parents.insert(id);
        } else {
            layouts.insert(id, layout?);
        }
    }
    for &id in parents.keys() {
        if !layouts.contains_key(id) && !drift.parents.contains(id) {
            report.issue("MISSING_PARENT", "warning", id);
        }
    }

    let mut expected = BTreeMap::new();
    for (id, layout) in &layouts {
        let parent = parents[id.as_str()];
        for data in chunk_rows(parent, layout)? {
            expected.insert(data.provenance.id.clone(), (parent, data));
        }
    }
    let mut query = conn.prepare("SELECT id,parent_id,kind,path,timestamp,line_start,line_end,text,hash,vector,chunk,section_start,section_end FROM documents")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        report.indexed_chunks += 1;
        let id: String = row.get(0)?;
        let parent_id = row.get_ref(1)?.as_str()?;
        let Some((parent, data)) = expected.remove(&id) else {
            report.issue(
                parent_code(ledger, parent_id, "ORPHAN_CHUNK"),
                "error",
                id.clone(),
            );
            drift.chunks.insert(id);
            continue;
        };
        let p = &parent.provenance;
        let parsed = serde_json::from_str::<MatchedChunk>(row.get_ref(10)?.as_str()?);
        let correct = parent_id == p.id
            && row.get_ref(2)?.as_str()? == p.kind.as_str()
            && row.get_ref(3)?.as_str()? == p.path
            && row.get::<_, Option<String>>(4)? == p.timestamp
            && row.get::<_, usize>(5)? == data.provenance.line_start
            && row.get::<_, usize>(6)? == data.provenance.line_end
            && row.get_ref(7)?.as_str()? == data.text
            && row.get_ref(8)?.as_str()? == data.provenance.content_hash
            && parsed.is_ok_and(|p| p == data.provenance)
            && row.get::<_, usize>(11)? == data.chunk.section.start
            && row.get::<_, usize>(12)? == data.chunk.section.end;
        if !correct {
            report.issue("STALE_CHUNK", "error", id.clone());
            drift.chunks.insert(id.clone());
        }
        let blob = row.get_ref(9)?.as_blob()?;
        if (ready || !blob.is_empty())
            && vectors::decode(blob, MODEL.dimensions)
                .and_then(|v| vectors::norm(&v, MODEL.dimensions))
                .is_err()
        {
            report.issue("INVALID_EMBEDDING", "error", id.clone());
            drift.chunks.insert(id);
        }
    }
    for id in expected.into_keys() {
        report.issue("MISSING_CHUNK", "warning", id);
    }

    if report.findings.keys().any(|code| {
        matches!(
            *code,
            "MISSING_PARENT"
                | "STALE_PARENT"
                | "ORPHAN_PARENT"
                | "FORGOTTEN_INDEX_ROWS"
                | "STALE_CHUNK"
                | "ORPHAN_CHUNK"
                | "INVALID_EMBEDDING"
                | "MISSING_CHUNK"
        )
    }) {
        report.action = "rebuild_index";
    }
    Ok(())
}

fn valid_layout(text: &str, chunks: &[Chunk]) -> bool {
    let lines: Vec<_> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut end = 0;
    for chunk in chunks {
        let start = chunk.span.start;
        if start < end
            || chunk.section.start > start
            || chunk.section.end < chunk.span.end
            || text.get(chunk.section.clone()).is_none()
            || !text
                .get(end..start)
                .is_some_and(|gap| gap.trim().is_empty())
            || !text
                .get(chunk.span.clone())
                .is_some_and(|body| !body.trim().is_empty())
            || chunk.lines
                != [
                    lines.partition_point(|line| *line <= start),
                    lines.partition_point(|line| *line < chunk.span.end),
                ]
        {
            return false;
        }
        end = chunk.span.end;
    }
    text[end..].trim().is_empty()
}

fn parent_code(ledger: &Ledger, id: &str, ordinary: &'static str) -> &'static str {
    if Uuid::parse_str(id).is_ok_and(|id| ledger.forgotten(id)) {
        "FORGOTTEN_INDEX_ROWS"
    } else {
        ordinary
    }
}

fn prune(conn: &mut Connection, drift: &Drift) -> Result<()> {
    let tx = conn.transaction()?;
    for id in &drift.parents {
        tx.execute("DELETE FROM documents WHERE parent_id=?1", params![id])?;
        tx.execute("DELETE FROM parents WHERE id=?1", params![id])?;
    }
    for id in &drift.chunks {
        tx.execute("DELETE FROM documents WHERE id=?1", params![id])?;
    }
    tx.execute("UPDATE metadata SET digest=''", [])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_layout_must_preserve_utf8_coverage_with_exact_line_ranges() {
        let text = "é\n\nsecond";
        let first = Chunk {
            span: 0..3,
            section: 0..text.len(),
            headings: Vec::new(),
            lines: [1, 1],
        };
        let second = Chunk {
            span: 4..text.len(),
            section: 0..text.len(),
            headings: Vec::new(),
            lines: [3, 3],
        };
        assert!(valid_layout(text, &[first.clone(), second.clone()]));
        assert!(!valid_layout(text, std::slice::from_ref(&second)));
        let mut invalid = first;
        invalid.span = 1..3;
        assert!(!valid_layout(text, &[invalid, second]));
    }
}
