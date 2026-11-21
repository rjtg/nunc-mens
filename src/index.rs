use crate::documents::{Document, Kind, Provenance, Snapshot};
use crate::fs::{effective_uid, lockfile, private_dir, sync_dir};
use crate::repo::Repo;
use crate::vectors::{self, Embedder, Model};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, params};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const BATCH: usize = 16;
const SCHEMA: &str = "
    CREATE TABLE metadata (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        repo_id TEXT NOT NULL,
        model_id TEXT NOT NULL,
        model_revision TEXT NOT NULL,
        dimensions INTEGER NOT NULL CHECK (dimensions > 0),
        digest TEXT NOT NULL
    ) STRICT;
    CREATE TABLE documents (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('memory', 'source')),
        path TEXT NOT NULL,
        timestamp TEXT,
        line_start INTEGER,
        line_end INTEGER,
        text TEXT NOT NULL,
        hash TEXT NOT NULL,
        vector BLOB NOT NULL,
        CHECK ((kind = 'memory' AND timestamp IS NOT NULL AND line_start IS NULL AND line_end IS NULL)
             OR (kind = 'source' AND timestamp IS NULL AND line_start >= 1 AND line_end >= line_start))
    ) STRICT;
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
        self.dir.join("index.sqlite3")
    }

    fn prepare(&self) -> Result<()> {
        fs::create_dir_all(self.dir.parent().context("missing index state parent")?)?;
        private_dir(&self.dir)
    }

    pub(crate) fn refresh(&self, embedder: &mut impl Embedder, full: bool) -> Result<Refresh> {
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
                let digest = check(&connection, self.repo, model)?;
                if digest == snapshot.digest {
                    return Ok(Refresh {
                        documents: snapshot.documents.len(),
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
        if existing && existing_private_file(&self.dir.join("index.sqlite3-journal"))? {
            let connection = open(path, false)?;
            connection
                .pragma_query_value(None, "schema_version", |_| Ok(()))
                .context("recover the old SQLite rollback journal before replacing the index")?;
        }
        temporary.persist(path)?;
        sync_dir(&self.dir)?;
        Ok(report)
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
            Ok(Some(Reader {
                conn,
                _lock: lock,
                digest,
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
                meta.is_file() && meta.uid() == effective_uid() && meta.mode() & 0o077 == 0,
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
    transaction.execute(
        "CREATE TEMP TABLE seen (id TEXT PRIMARY KEY) WITHOUT ROWID",
        [],
    )?;
    let mut report = Refresh {
        documents: snapshot.documents.len(),
        ..Refresh::default()
    };
    {
        let mut seen = transaction.prepare("INSERT INTO seen(id) VALUES (?1)")?;
        let mut known = transaction.prepare("SELECT hash FROM documents WHERE id = ?1")?;
        let mut changed = Vec::new();
        for doc in &snapshot.documents {
            let id = &doc.provenance.id;
            seen.execute([id])?;
            let hash: Option<String> = known.query_row([id], |row| row.get(0)).optional()?;
            if hash.as_ref() == Some(&doc.hash) {
                update_provenance(&transaction, doc)?;
            } else {
                changed.push(doc);
                if changed.len() == BATCH {
                    report.embedded += store_batch(&transaction, embedder, &changed)?;
                    changed.clear();
                }
            }
        }
        report.embedded += store_batch(&transaction, embedder, &changed)?;
    }
    report.deleted = transaction.execute(
        "DELETE FROM documents WHERE id NOT IN (SELECT id FROM seen)",
        [],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO metadata VALUES (1, ?1, ?2, ?3, ?4, ?5)",
        params![
            repo.id,
            model.id,
            model.revision,
            model.dimensions,
            snapshot.digest
        ],
    )?;
    transaction.commit()?;
    Ok(report)
}

fn update_provenance(tx: &Transaction<'_>, doc: &Document) -> Result<()> {
    let p = &doc.provenance;
    tx.prepare_cached(
        "UPDATE documents SET path = ?2, timestamp = ?3, line_start = ?4, line_end = ?5
         WHERE id = ?1 AND (path IS NOT ?2 OR timestamp IS NOT ?3 OR line_start IS NOT ?4 OR line_end IS NOT ?5)",
    )?.execute(
        params![p.id, p.path, p.timestamp, p.line_start, p.line_end],
    )?;
    Ok(())
}

fn store_batch(
    tx: &Transaction<'_>,
    embedder: &mut impl Embedder,
    docs: &[&Document],
) -> Result<usize> {
    if docs.is_empty() {
        return Ok(0);
    }
    let passages: Vec<_> = docs.iter().map(|doc| doc.text.as_str()).collect();
    let embeddings = embedder
        .passages(&passages)
        .context("embed changed documents")?;
    ensure!(
        embeddings.len() == docs.len(),
        "model returned the wrong number of embeddings"
    );
    let mut stmt = tx.prepare_cached(
        "INSERT OR REPLACE INTO documents
         (id, kind, path, timestamp, line_start, line_end, text, hash, vector)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let dimensions = embedder.model().dimensions;
    for (doc, vector) in docs.iter().zip(embeddings) {
        let p = &doc.provenance;
        let blob = vectors::encode(&vector, dimensions)
            .with_context(|| format!("validate vector at {}", p.path))?;
        stmt.execute(params![
            p.id,
            p.kind.as_str(),
            p.path,
            p.timestamp,
            p.line_start,
            p.line_end,
            doc.text,
            doc.hash,
            blob
        ])?;
    }
    Ok(docs.len())
}

impl Reader {
    pub(crate) fn scan(
        &self,
        kind: Option<Kind>,
        mut visit: impl FnMut(&str, &[f32], &Row<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, path, timestamp, line_start, line_end, text, vector
             FROM documents WHERE ?1 IS NULL OR kind = ?1",
        )?;
        let mut rows = stmt.query([kind.map(Kind::as_str)])?;
        while let Some(row) = rows.next()? {
            let path: &str = row.get_ref(2)?.as_str()?;
            let blob = row.get_ref(7)?.as_blob()?;
            let vector = vectors::decode(blob, self.dimensions).with_context(|| {
                format!("invalid indexed vector at {path}; run nunc reindex --full")
            })?;
            visit(path, &vector, row)?;
        }
        Ok(())
    }
}

pub(crate) fn provenance(row: &Row<'_>) -> Result<Provenance> {
    Ok(Provenance {
        id: row.get(0)?,
        kind: Kind::parse(row.get_ref(1)?.as_str()?)?,
        path: row.get(2)?,
        timestamp: row.get(3)?,
        line_start: row.get(4)?,
        line_end: row.get(5)?,
    })
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

        fn passages(&mut self, passages: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.calls += passages.len();
            if self.invalid {
                bail!("fixture embedding failed");
            }
            Ok(passages.iter().map(|_| vec![1.0, -0.5]).collect())
        }
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
        assert_eq!((next.documents, next.embedded, next.deleted), (1, 1, 1));
        let reader = index.read(MODEL)?.context("no index")?;
        reader.scan(None, |path, vector, row| {
            assert_eq!(path, "design.md");
            assert_eq!(vector, [1.0, -0.5]);
            let source = provenance(row)?;
            assert_eq!(source.line_start, Some(1));
            assert_eq!(source.kind, Kind::Source);
            assert_eq!(row.get::<_, String>(6)?, "Modified source\n");
            Ok(())
        })?;
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
