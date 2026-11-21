use crate::chunk::{self, Chunk, Strategy};
use crate::documents::{Document, Kind, MatchedChunk, Provenance, Snapshot};
use crate::entries::{Commit, EntryKind};
use crate::fs::{effective_uid, lockfile, private_dir, sync_dir};
use crate::ledger::Guard;
use crate::repo::{Repo, hash};
use crate::vectors::{self, Embedder, Model};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, params};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const INDEX_NAME: &str = "index.sqlite3";
const VERSION: u32 = 2;
const BATCH: usize = 16;
const SCHEMA: &str = "
    CREATE TABLE metadata (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        repo_id TEXT NOT NULL,
        model_id TEXT NOT NULL,
        model_revision TEXT NOT NULL,
        dimensions INTEGER NOT NULL CHECK (dimensions > 0),
        digest TEXT NOT NULL,
        chunking TEXT NOT NULL
    ) STRICT;
    CREATE TABLE parents (
        id TEXT PRIMARY KEY,
        text TEXT NOT NULL,
        revision TEXT NOT NULL,
        layout_key TEXT NOT NULL,
        layout TEXT NOT NULL,
        origin TEXT NOT NULL
    ) STRICT;
    CREATE TABLE documents (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('memory', 'source')),
        path TEXT NOT NULL,
        timestamp TEXT,
        line_start INTEGER NOT NULL,
        line_end INTEGER NOT NULL,
        text TEXT NOT NULL,
        hash TEXT NOT NULL,
        vector BLOB NOT NULL,
        parent_id TEXT NOT NULL,
        chunk TEXT NOT NULL,
        section_start INTEGER NOT NULL,
        section_end INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX chunk_hash ON documents(hash);
    CREATE INDEX chunk_parent ON documents(parent_id);
";

pub(crate) struct Index<'a> {
    repo: &'a Repo,
    dir: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct Refresh {
    pub documents: usize,
    pub embedded: usize,
    pub deleted: usize,
}

pub(crate) struct Reader {
    conn: Connection,
    _lock: File,
    pub digest: String,
    pub strategy: Strategy,
    dimensions: usize,
}

impl<'a> Index<'a> {
    pub(crate) fn for_repo(repo: &'a Repo) -> Result<Self> {
        Ok(Self {
            repo,
            dir: repo.state()?,
        })
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.dir.join(INDEX_NAME)
    }

    fn prepare(&self) -> Result<()> {
        fs::create_dir_all(self.dir.parent().context("missing index state parent")?)?;
        private_dir(&self.dir)
    }

    pub(crate) fn refresh(&self, embedder: &mut impl Embedder, full: bool) -> Result<Refresh> {
        let guard = Guard::acquire(self.repo, true)?;
        guard.cleanup_files()?;
        self.prepare()?;
        let lock = lockfile(&self.dir.join("index.lock"))?;
        lock.lock().context("lock index for refresh")?;
        let path = self.path();
        let existing = existing_private_file(&path)?;
        let snapshot = Snapshot::read(self.repo)?;
        let model = embedder.model();
        let update = || -> Result<Refresh> {
            if full || !existing {
                self.rebuild(&path, snapshot, embedder, existing)
            } else {
                let mut connection = open(&path, false)?;
                let version: u32 =
                    connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
                if version == 1 {
                    drop(connection);
                    return self.rebuild(&path, snapshot, embedder, existing);
                }
                let digest = check(&connection, self.repo, model)?;
                if digest == snapshot.digest {
                    return Ok(Refresh {
                        documents: connection.query_row(
                            "SELECT count(*) FROM documents",
                            [],
                            |row| row.get(0),
                        )?,
                        ..Refresh::default()
                    });
                }
                update_rows(&mut connection, snapshot, embedder, self.repo)
            }
        };
        update().with_context(|| {
            if full {
                format!("rebuild SQLite index {}", path.display())
            } else {
                format!("refresh SQLite index {}; use nunc reindex --full to recover a damaged or incompatible index", path.display())
            }
        })
    }

    fn rebuild(
        &self,
        path: &Path,
        snapshot: Snapshot,
        embedder: &mut impl Embedder,
        existing: bool,
    ) -> Result<Refresh> {
        let temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
        let report = {
            let mut connection = open(temporary.path(), false)?;
            let transaction = connection.transaction()?;
            transaction.execute_batch(SCHEMA)?;
            transaction.pragma_update(None, "user_version", VERSION)?;
            transaction.commit()?;
            update_rows(&mut connection, snapshot, embedder, self.repo)?
        };
        temporary.as_file().sync_all()?;
        // A crashed rollback journal belongs to the old inode and must be recovered
        // before publishing a new inode at that name.
        let journal = self.dir.join(format!("{INDEX_NAME}-journal"));
        if existing_private_file(&journal)? {
            if existing {
                let connection = open(path, false)?;
                connection
                    .pragma_query_value(None, "schema_version", |_| Ok(()))
                    .context("recover the old rollback journal; move the old index and journal aside if they are damaged")?;
            }
            if existing_private_file(&journal)? {
                fs::remove_file(&journal).context("remove the inactive SQLite rollback journal")?;
            }
        }
        temporary.persist(path)?;
        sync_dir(&self.dir)?;
        Ok(report)
    }

    pub(crate) fn purge(&self, ids: impl Iterator<Item = uuid::Uuid>) -> Result<()> {
        let path = self.path();
        if !existing_private_file(&path)? {
            return Ok(());
        }
        private_dir(&self.dir)?;
        let lock = lockfile(&self.dir.join("index.lock"))?;
        lock.lock()?;
        let mut conn = open(&path, false)?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == 1 || version == VERSION,
            "incompatible index; run nunc reindex --full"
        );
        let tx = conn.transaction()?;
        let mut erased = 0;
        for id in ids {
            let id = id.to_string();
            if version == 1 {
                erased += tx.execute("DELETE FROM documents WHERE id=?1", [&id])?;
            } else {
                erased += tx.execute("DELETE FROM documents WHERE parent_id=?1", [&id])?;
                erased += tx.execute("DELETE FROM parents WHERE id=?1", [&id])?;
            }
        }
        if erased != 0 {
            tx.execute("UPDATE metadata SET digest=''", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn read(&self, model: Model) -> Result<Option<Reader>> {
        let path = self.path();
        if !existing_private_file(&path)? {
            return Ok(None);
        }
        private_dir(&self.dir)?;
        let lock = lockfile(&self.dir.join("index.lock"))?;
        lock.lock_shared().context("lock index for reading")?;
        let open_reader = || -> Result<Option<Reader>> {
            if !existing_private_file(&path)? {
                return Ok(None);
            }
            let conn = open(&path, true)?;
            let digest = check(&conn, self.repo, model)?;
            let chunking: String =
                conn.query_row("SELECT chunking FROM metadata", [], |row| row.get(0))?;
            let strategy = serde_json::from_str(&chunking)?;
            Ok(Some(Reader {
                conn,
                _lock: lock,
                digest,
                strategy,
                dimensions: model.dimensions,
            }))
        };
        open_reader().with_context(|| {
            format!(
                "read index {}; use nunc reindex --full to recover a damaged or incompatible index",
                path.display()
            )
        })
    }
}

fn existing_private_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file()
                    && meta.uid() == effective_uid()
                    && meta.mode() & 0o077 == 0
                    && meta.nlink() == 1,
                "index artifact must be a private regular file: {}",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn open(path: &Path, read_only: bool) -> Result<Connection> {
    let mode = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    Ok(Connection::open_with_flags(
        path,
        mode | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?)
}

fn check(conn: &Connection, repo: &Repo, model: Model) -> Result<String> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        version == VERSION,
        "SQLite schema {version} is incompatible with schema {VERSION}"
    );
    let (repo_id, id, revision, dims, digest): (String, String, String, usize, String) = conn.query_row(
        "SELECT repo_id, model_id, model_revision, dimensions, digest FROM metadata WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    ensure!(
        repo_id == repo.id,
        "index belongs to a different repository"
    );
    ensure!(
        (id.as_str(), revision.as_str(), dims) == (model.id, model.revision, model.dimensions),
        "index embedding model or dimensions are incompatible"
    );
    Ok(digest)
}

fn update_rows(
    conn: &mut Connection,
    snapshot: Snapshot,
    embedder: &mut impl Embedder,
    repo: &Repo,
) -> Result<Refresh> {
    let model = embedder.model();
    let transaction = conn.transaction()?;
    transaction.execute_batch(
        "
        CREATE TEMP TABLE seen (id TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TEMP TABLE seen_parents (id TEXT PRIMARY KEY) WITHOUT ROWID;
    ",
    )?;
    let mut report = Refresh::default();
    let layout_key = hash(&serde_json::to_vec(&snapshot.chunking)?);
    for parent in &snapshot.documents {
        let id = &parent.provenance.id;
        transaction.execute("INSERT INTO seen_parents VALUES (?1)", [id])?;
        let origin =
            serde_json::to_string(&(&parent.provenance.entry_kind, &parent.provenance.git))?;
        let known: Option<String> = transaction
            .query_row(
                "SELECT layout FROM parents WHERE id=?1 AND revision=?2 AND layout_key=?3",
                params![id, parent.hash, layout_key],
                |row| row.get(0),
            )
            .optional()?;
        let layout: Vec<Chunk> = match known {
            Some(json) => {
                transaction.execute(
                    "UPDATE parents SET origin=?2 WHERE id=?1 AND origin!=?2",
                    params![id, origin],
                )?;
                serde_json::from_str(&json)?
            }
            None => {
                let layout =
                    chunk::layout(&parent.text, parent.markdown, &snapshot.chunking, embedder)
                        .with_context(|| format!("chunk {}", parent.provenance.path))?;
                transaction.execute(
                    "INSERT OR REPLACE INTO parents VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        id,
                        parent.text,
                        parent.hash,
                        layout_key,
                        serde_json::to_string(&layout)?,
                        origin
                    ],
                )?;
                layout
            }
        };
        report.documents += layout.len();
        let mut occurrences = HashMap::new();
        let mut changed = Vec::new();
        for chunk in &layout {
            let text = parent
                .text
                .get(chunk.span.clone())
                .context("invalid cached chunk span")?;
            let digest = hash(text.as_bytes());
            let occurrence = occurrences.entry(digest.clone()).or_insert(0);
            let id = format!(
                "chunk:{}",
                hash(format!("{id}\0{digest}\0{occurrence}").as_bytes())
            );
            *occurrence += 1;
            transaction.execute("INSERT INTO seen VALUES (?1)", [&id])?;
            let existing: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT vector FROM documents WHERE hash=?1 LIMIT 1",
                    [&digest],
                    |row| row.get(0),
                )
                .optional()?;
            let data = ChunkData {
                provenance: MatchedChunk::new(id, digest, parent, chunk),
                text,
                chunk,
            };
            if let Some(vector) = existing {
                store(&transaction, parent, &data, &vector)?;
            } else {
                changed.push(data);
                if changed.len() == BATCH {
                    report.embedded += store_batch(&transaction, embedder, parent, &changed)?;
                    changed.clear();
                }
            }
        }
        report.embedded += store_batch(&transaction, embedder, parent, &changed)?;
    }
    report.deleted = transaction.execute(
        "DELETE FROM documents WHERE id NOT IN (SELECT id FROM seen)",
        [],
    )?;
    transaction.execute(
        "DELETE FROM parents WHERE id NOT IN (SELECT id FROM seen_parents)",
        [],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO metadata VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            repo.id,
            model.id,
            model.revision,
            model.dimensions,
            snapshot.digest,
            serde_json::to_string(&snapshot.chunking.strategy)?
        ],
    )?;
    transaction.commit()?;
    Ok(report)
}

struct ChunkData<'a> {
    provenance: MatchedChunk,
    text: &'a str,
    chunk: &'a Chunk,
}

fn store(
    tx: &Transaction<'_>,
    parent: &Document,
    data: &ChunkData<'_>,
    vector: &[u8],
) -> Result<()> {
    let p = &parent.provenance;
    let c = &data.provenance;
    tx.prepare_cached(
        "INSERT INTO documents VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(id) DO UPDATE SET timestamp=excluded.timestamp, line_start=excluded.line_start,
         line_end=excluded.line_end, chunk=excluded.chunk, section_start=excluded.section_start, section_end=excluded.section_end
         WHERE timestamp IS NOT excluded.timestamp OR chunk != excluded.chunk OR section_start != excluded.section_start OR section_end != excluded.section_end",
    )?.execute(params![c.id, p.kind.as_str(), p.path, p.timestamp, c.line_start, c.line_end,
        data.text, c.content_hash, vector, c.parent_id, serde_json::to_string(c)?, data.chunk.section.start, data.chunk.section.end])?;
    Ok(())
}

fn store_batch(
    tx: &Transaction<'_>,
    embedder: &mut impl Embedder,
    parent: &Document,
    chunks: &[ChunkData<'_>],
) -> Result<usize> {
    if chunks.is_empty() {
        return Ok(0);
    }
    let passages: Vec<_> = chunks.iter().map(|chunk| chunk.text).collect();
    let embeddings = embedder
        .passages(&passages)
        .context("embed changed chunks")?;
    ensure!(
        embeddings.len() == chunks.len(),
        "model returned the wrong number of embeddings"
    );
    for (chunk, vector) in chunks.iter().zip(embeddings) {
        let blob = vectors::encode(&vector, embedder.model().dimensions)
            .with_context(|| format!("validate vector at {}", parent.provenance.path))?;
        store(tx, parent, chunk, &blob)?;
    }
    Ok(chunks.len())
}

impl Reader {
    pub(crate) fn origin(&self, id: &str) -> Result<(Option<EntryKind>, Option<Commit>)> {
        let value: String =
            self.conn
                .query_row("SELECT origin FROM parents WHERE id=?1", [id], |row| {
                    row.get(0)
                })?;
        Ok(serde_json::from_str(&value)?)
    }

    pub(crate) fn parent(&self, id: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT text FROM parents WHERE id=?1", [id], |row| {
                row.get(0)
            })?)
    }

    pub(crate) fn scan(
        &self,
        kind: Option<Kind>,
        include: impl Fn(&str) -> bool,
        mut visit: impl FnMut(IndexedRow<'_, '_>, &[f32]) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, path, timestamp, line_start, line_end, text, vector, parent_id, chunk, section_start, section_end
             FROM documents WHERE ?1 IS NULL OR kind = ?1",
        )?;
        let mut rows = stmt.query([kind.map(Kind::as_str)])?;
        while let Some(row) = rows.next()? {
            let path: &str = row.get_ref(2)?.as_str()?;
            if !include(path) {
                continue;
            }
            let blob = row.get_ref(7)?.as_blob()?;
            let vector = vectors::decode(blob, self.dimensions).with_context(|| {
                format!("invalid indexed vector at {path}; run nunc reindex --full")
            })?;
            visit(IndexedRow { row }, &vector)
                .with_context(|| format!("inspect indexed vector at {path}"))?;
        }
        Ok(())
    }
}

pub(crate) struct IndexedRow<'row, 'stmt> {
    row: &'row Row<'stmt>,
}

impl IndexedRow<'_, '_> {
    pub(crate) fn id(&self) -> Result<&str> {
        Ok(self.row.get_ref(0)?.as_str()?)
    }

    pub(crate) fn parent_id(&self) -> Result<&str> {
        Ok(self.row.get_ref(8)?.as_str()?)
    }

    pub(crate) fn timestamp(&self) -> Result<Option<&str>> {
        Ok(self.row.get_ref(3)?.as_str_or_null()?)
    }

    pub(crate) fn section(&self) -> Result<std::ops::Range<usize>> {
        Ok(self.row.get(10)?..self.row.get(11)?)
    }

    pub(crate) fn matched(&self) -> Result<Option<MatchedChunk>> {
        Ok(Some(serde_json::from_str(self.row.get_ref(9)?.as_str()?)?))
    }

    pub(crate) fn document(&self) -> Result<(Provenance, String)> {
        let row = self.row;
        let kind = Kind::parse(row.get_ref(1)?.as_str()?)?;
        let source = Provenance {
            id: if kind == Kind::Memory {
                row.get(8)?
            } else {
                row.get(0)?
            },
            kind,
            path: row.get(2)?,
            timestamp: row.get(3)?,
            line_start: row.get(4)?,
            line_end: row.get(5)?,
            matched_chunk: self.matched()?,
            entry_kind: None,
            git: None,
            visibility: None,
        };
        Ok((source, row.get(6)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::{self, ContextFields};
    use anyhow::bail;

    const MODEL: Model = Model {
        id: "fixture",
        revision: "1",
        dimensions: 2,
    };

    #[derive(Default)]
    struct Mock {
        invalid: bool,
        calls: usize,
    }

    impl Embedder for Mock {
        fn model(&self) -> Model {
            MODEL
        }

        fn token_offsets(&self, text: &str) -> Result<Vec<(usize, usize)>> {
            Ok(text
                .char_indices()
                .map(|(i, c)| (i, i + c.len_utf8()))
                .collect())
        }

        fn passages(&mut self, passages: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.calls += passages.len();
            if self.invalid {
                bail!("fixture embedding failed");
            }
            Ok(passages.iter().map(|_| vec![1.0, -0.5]).collect())
        }
    }

    #[test]
    fn unchanged_chunk_keeps_its_identity_and_embedding_after_line_shift() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        fs::create_dir(&root)?;
        let repo = Repo::new(&root)?;
        let index = Index {
            repo: &repo,
            dir: temp.path().join("state"),
        };
        let path = root.join("design.md");
        let original = "# Before\n\nA changing note.\n\n## Stable\n\nUnchanged writer policy.\n";
        fs::write(&path, original)?;
        repo.configure(&["design.md".into()], true)?;
        let mut model = Mock::default();
        assert_eq!(index.refresh(&mut model, false)?.embedded, 2);
        let stable = || -> Result<(String, usize)> {
            let reader = index.read(MODEL)?.context("missing index")?;
            let mut found = None;
            reader.scan(
                None,
                |_| true,
                |row, _| {
                    let chunk = row.matched()?.context("missing provenance")?;
                    if chunk
                        .heading_path
                        .last()
                        .is_some_and(|heading| heading == "Stable")
                    {
                        found = Some((chunk.id, chunk.line_start));
                    }
                    Ok(())
                },
            )?;
            found.context("missing stable chunk")
        };
        let first = stable()?;
        fs::write(
            path,
            original.replace("A changing note.", "A changing note.\nA second line."),
        )?;
        assert_eq!(index.refresh(&mut model, false)?.embedded, 1);
        let second = stable()?;
        assert_eq!(first.0, second.0);
        assert_eq!(second.1, first.1 + 1);
        Ok(())
    }

    #[test]
    fn reconcile_reuses_vectors_rolls_back_and_rebuilds() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        fs::create_dir(&root)?;
        let repo = Repo::new(&root)?;
        let index = Index {
            repo: &repo,
            dir: temp.path().join("state"),
        };
        assert!(index.read(MODEL)?.is_none());
        let mut model = Mock::default();
        let entry = entries::persist(&repo, "first decision", ContextFields::default())?;
        let path = root.join("design.md");
        fs::write(&path, "Original source\n")?;
        repo.configure(&["design.md".into()], true)?;
        let first = index.refresh(&mut model, false)?;
        assert_eq!((first.documents, first.embedded, first.deleted), (2, 2, 0));
        assert_eq!(index.refresh(&mut model, false)?.embedded, 0);
        let original = index.read(MODEL)?.context("no index")?.digest;
        assert!(
            index
                .read(Model {
                    dimensions: 3,
                    ..MODEL
                })
                .is_err()
        );

        fs::remove_file(entry.path)?;
        fs::write(path, "Modified source\n")?;
        model.invalid = true;
        assert!(index.refresh(&mut model, false).is_err());
        assert!(index.refresh(&mut model, true).is_err());
        assert_eq!(index.read(MODEL)?.context("no index")?.digest, original);
        model.invalid = false;
        let next = index.refresh(&mut model, false)?;
        assert_eq!((next.documents, next.embedded, next.deleted), (1, 1, 2));
        let reader = index.read(MODEL)?.context("no index")?;
        reader.scan(
            None,
            |_| true,
            |row, vector| {
                assert_eq!(vector, [1.0, -0.5]);
                let (source, text) = row.document()?;
                assert_eq!(source.path, "design.md");
                assert_eq!(source.line_start, Some(1));
                assert_eq!(source.kind, Kind::Source);
                assert_eq!(text, "Modified source\n");
                Ok(())
            },
        )?;
        drop(reader);
        fs::write(index.path(), b"corrupt database")?;
        assert!(index.read(MODEL).is_err());
        assert_eq!(index.refresh(&mut model, true)?.embedded, 1);
        assert!(index.read(MODEL)?.is_some());

        let other = temp.path().join("other");
        fs::create_dir(&other)?;
        let other = Repo::new(&other)?;
        assert_ne!(other.id, repo.id);
        let mistaken = Index {
            repo: &other,
            dir: index.dir.clone(),
        };
        assert!(mistaken.read(MODEL).is_err());
        Ok(())
    }
}
