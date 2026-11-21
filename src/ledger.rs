use crate::entries::{self, ContextFields, Entry, EntryKind};
use crate::fs::{atomic_write, lockfile, missing, private_dir, read_regular, sync_dir};
use crate::repo::{Repo, hash};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io;
use uuid::Uuid;

const NAME: &str = "ledger.json";
const MAX_LEDGER: usize = 128 * 1024 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Ledger {
    summaries: BTreeMap<Uuid, Summary>,
    forgotten: BTreeMap<Uuid, Tombstone>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Summary {
    sources: Vec<Uuid>,
    undone: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Tombstone {
    #[serde(skip_serializing_if = "Option::is_none")]
    git_sha: Option<String>,
}

pub(crate) struct Guard<'a> {
    pub ledger: Ledger,
    repo: &'a Repo,
    _lock: Option<File>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Reference {
    id: Uuid,
    forgotten: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Visibility {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_into: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_into_forgotten: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Reference>,
}

#[derive(Serialize)]
pub(crate) struct Applied {
    pub summary_id: Uuid,
    pub archived_entries: Vec<Uuid>,
    pub status: &'static str,
}

#[derive(Serialize)]
pub(crate) struct Dependency {
    pub summary_id: Uuid,
    pub direct: bool,
}

#[derive(Serialize)]
pub(crate) struct ForgetPlan {
    pub targets: Vec<Uuid>,
    pub already_forgotten: Vec<Uuid>,
    pub dependencies: Vec<Dependency>,
}

impl Ledger {
    pub fn read(repo: &Repo) -> Result<Self> {
        match fs::symlink_metadata(repo.local()) {
            Ok(_) => private_dir(&repo.local())?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).context("inspect durable memory directory"),
        }
        let path = repo.local().join(NAME);
        let bytes = match read_regular(&path, MAX_LEDGER) {
            Ok(bytes) => bytes,
            Err(error) if missing(&error) => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let ledger: Self = serde_json::from_slice(&bytes).context("parse durable memory ledger")?;
        ledger.validate()?;
        Ok(ledger)
    }

    fn validate(&self) -> Result<()> {
        self.archives()?;
        // Topological traversal over every historical edge, including undo and tombstones.
        let mut degrees = BTreeMap::new();
        let mut reverse = BTreeMap::<_, Vec<_>>::new();
        for (&summary, operation) in &self.summaries {
            let mut seen = BTreeSet::new();
            for &source in &operation.sources {
                ensure!(
                    source != summary && seen.insert(source),
                    "invalid consolidation lineage"
                );
                if self.summaries.contains_key(&source) {
                    reverse.entry(source).or_default().push(summary);
                    *degrees.entry(summary).or_insert(0_usize) += 1;
                }
            }
            degrees.entry(summary).or_insert(0);
        }
        let mut ready = degrees
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut visited = 0;
        while let Some(id) = ready.pop() {
            visited += 1;
            for target in reverse.get(&id).into_iter().flatten() {
                let degree = degrees.get_mut(target).context("invalid lineage node")?;
                *degree -= 1;
                if *degree == 0 {
                    ready.push(*target);
                }
            }
        }
        ensure!(
            visited == degrees.len(),
            "consolidation lineage contains a cycle"
        );
        Ok(())
    }

    pub fn archives(&self) -> Result<BTreeMap<Uuid, Uuid>> {
        let mut archives = BTreeMap::new();
        for (&summary, operation) in self.summaries.iter().filter(|(_, op)| !op.undone) {
            for &source in &operation.sources {
                ensure!(
                    archives.insert(source, summary).is_none(),
                    "entry is archived by multiple summaries"
                );
            }
        }
        Ok(archives)
    }

    pub(crate) fn lineage(&self) -> impl Iterator<Item = (Uuid, &[Uuid])> + '_ {
        self.summaries
            .iter()
            .map(|(&id, summary)| (id, summary.sources.as_slice()))
    }

    pub(crate) fn tombstones(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.forgotten.keys().copied()
    }

    pub fn forgotten(&self, id: Uuid) -> bool {
        self.forgotten.contains_key(&id)
    }

    pub fn known_commit(&self, id: Uuid, sha: &str) -> bool {
        self.forgotten(id)
            || self
                .forgotten
                .values()
                .any(|marker| marker.git_sha.as_deref() == Some(sha))
    }

    pub fn pending_summary(&self, entry: &Entry) -> bool {
        entry.metadata.kind == EntryKind::Consolidation
            && !self.summaries.contains_key(&entry.metadata.id)
    }

    pub fn visibility(&self, archives: &BTreeMap<Uuid, Uuid>, id: Uuid) -> Visibility {
        let archived_into = archives.get(&id).copied();
        let summary = self.summaries.get(&id);
        let status = if archived_into.is_some() {
            "archived"
        } else if summary.is_some_and(|summary| summary.undone) {
            "undone"
        } else if summary.is_some() {
            "consolidated"
        } else {
            "active"
        };
        let sources = summary
            .into_iter()
            .flat_map(|summary| summary.sources.iter())
            .map(|&id| Reference {
                id,
                forgotten: self.forgotten(id),
            })
            .collect();
        Visibility {
            status,
            archived_into,
            archived_into_forgotten: archived_into.map(|target| self.forgotten(target)),
            sources,
        }
    }

    pub fn forget_plan(&self, repo: &Repo, ids: &[Uuid]) -> Result<ForgetPlan> {
        unique(ids)?;
        let mut targets = Vec::new();
        let mut already_forgotten = Vec::new();
        for &id in ids {
            if self.forgotten(id) {
                already_forgotten.push(id);
            } else {
                let entry = entries::read(&entries::path(repo, id))
                    .with_context(|| format!("cannot forget entry {id}"))?;
                ensure!(
                    !self.pending_summary(&entry),
                    "entry {id} is an uncommitted summary"
                );
                targets.push(id);
            }
        }
        let selected: BTreeSet<_> = ids.iter().copied().collect();
        let mut reverse = BTreeMap::<_, Vec<_>>::new();
        for (&summary, operation) in &self.summaries {
            for &source in &operation.sources {
                reverse.entry(source).or_default().push(summary);
            }
        }
        let mut queue: VecDeque<_> = targets.iter().copied().map(|id| (id, 0)).collect();
        let mut visited = BTreeSet::new();
        let mut dependencies = BTreeMap::<_, bool>::new();
        while let Some((id, depth)) = queue.pop_front() {
            for &parent in reverse.get(&id).into_iter().flatten() {
                if !selected.contains(&parent) && !self.forgotten(parent) {
                    let direct = dependencies.entry(parent).or_default();
                    *direct |= depth == 0;
                }
                if visited.insert(parent) {
                    queue.push_back((parent, depth + 1));
                }
            }
        }
        Ok(ForgetPlan {
            targets,
            already_forgotten,
            dependencies: dependencies
                .into_iter()
                .map(|(summary_id, direct)| Dependency { summary_id, direct })
                .collect(),
        })
    }
}

fn unique(ids: &[Uuid]) -> Result<()> {
    ensure!(!ids.is_empty(), "select at least one entry");
    ensure!(
        ids.iter().collect::<BTreeSet<_>>().len() == ids.len(),
        "duplicate entry ID"
    );
    Ok(())
}

impl<'a> Guard<'a> {
    pub fn acquire(repo: &'a Repo, write: bool) -> Result<Self> {
        if write {
            repo.prepare()?;
        }
        let lock = match fs::symlink_metadata(repo.local()) {
            Ok(_) => {
                private_dir(&repo.local())?;
                let file = lockfile(&repo.local().join("memory.lock"))?;
                if write {
                    file.lock()?;
                } else {
                    file.lock_shared()?;
                }
                Some(file)
            }
            Err(err) if !write && err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("lock durable memory state"),
        };
        Ok(Self {
            ledger: Ledger::read(repo)?,
            repo,
            _lock: lock,
        })
    }

    fn save(&self) -> Result<()> {
        self.ledger.validate()?;
        let bytes = serde_json::to_vec(&self.ledger)?;
        ensure!(
            bytes.len() <= MAX_LEDGER,
            "durable memory ledger exceeds size limit"
        );
        atomic_write(&self.repo.local().join(NAME), &bytes, true)
    }

    pub fn apply(
        &mut self,
        ids: &[Uuid],
        revisions: &BTreeMap<Uuid, String>,
        body: &str,
    ) -> Result<Applied> {
        unique(ids)?;
        entries::validate_body(body)?;
        ensure!(
            revisions.keys().all(|id| ids.contains(id)),
            "revision supplied for an unselected entry"
        );
        let mut fingerprints = BTreeMap::new();
        let mut inputs = ids.to_vec();
        inputs.sort_unstable();
        for id in &inputs {
            ensure!(!self.ledger.forgotten(*id), "entry {id} was forgotten");
            let entry = entries::read(&entries::path(self.repo, *id))?;
            ensure!(
                !self.ledger.pending_summary(&entry),
                "entry {id} is an uncommitted summary"
            );
            let fingerprint = hash(entry.body.as_bytes());
            ensure!(
                revisions
                    .get(id)
                    .is_none_or(|revision| *revision == fingerprint),
                "stale revision for entry {id}"
            );
            fingerprints.insert(*id, fingerprint);
        }
        let operation = serde_json::to_vec(&(
            "consolidation-v1",
            &self.repo.id,
            &fingerprints,
            hash(body.as_bytes()),
        ))?;
        let summary_id = entries::durable_id(&operation);
        ensure!(
            !inputs.contains(&summary_id),
            "consolidation would contain itself"
        );
        ensure!(
            !self.ledger.forgotten(summary_id),
            "this summary was forgotten"
        );
        if let Some(previous) = self.ledger.summaries.get(&summary_id) {
            ensure!(
                !previous.undone,
                "this consolidation was undone; use new summary text to apply it again"
            );
            let saved = entries::read(&entries::path(self.repo, summary_id))?;
            ensure!(
                saved.metadata.kind == EntryKind::Consolidation && saved.body == body,
                "saved summary {summary_id} no longer matches this operation"
            );
            return Ok(Applied {
                summary_id,
                archived_entries: inputs,
                status: "already-applied",
            });
        }
        let archives = self.ledger.archives()?;
        for id in &inputs {
            ensure!(!archives.contains_key(id), "entry {id} is already archived");
            ensure!(
                self.ledger
                    .summaries
                    .get(id)
                    .is_none_or(|summary| !summary.undone),
                "entry {id} is an undone summary"
            );
        }
        let path = entries::path(self.repo, summary_id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let orphan = entries::read(&path)?;
                ensure!(
                    orphan.metadata.kind == EntryKind::Consolidation && orphan.body == body,
                    "conflicting summary entry {summary_id}"
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut metadata =
                    entries::metadata(self.repo, summary_id, ContextFields::default());
                metadata.kind = EntryKind::Consolidation;
                entries::persist_record(self.repo, body, metadata)?;
            }
            Err(error) => return Err(error).context("inspect pending summary"),
        }
        for (id, expected) in fingerprints {
            let source = entries::read(&entries::path(self.repo, id))?;
            ensure!(
                hash(source.body.as_bytes()) == expected,
                "entry {id} changed during consolidation"
            );
        }
        self.ledger.summaries.insert(
            summary_id,
            Summary {
                sources: inputs.clone(),
                undone: false,
            },
        );
        self.save()?;
        Ok(Applied {
            summary_id,
            archived_entries: inputs,
            status: "applied",
        })
    }

    pub fn undo(&mut self, id: Uuid) -> Result<Applied> {
        ensure!(
            !self.ledger.archives()?.contains_key(&id),
            "undo the enclosing consolidation first"
        );
        let summary = self
            .ledger
            .summaries
            .get_mut(&id)
            .context("unknown consolidation summary ID")?;
        let archived_entries = summary.sources.clone();
        if summary.undone {
            return Ok(Applied {
                summary_id: id,
                archived_entries,
                status: "already-undone",
            });
        }
        summary.undone = true;
        self.save()?;
        Ok(Applied {
            summary_id: id,
            archived_entries,
            status: "undone",
        })
    }

    pub fn forget(&mut self, plan: &ForgetPlan) -> Result<()> {
        for &id in &plan.targets {
            let entry = entries::read(&entries::path(self.repo, id))?;
            self.ledger.forgotten.insert(
                id,
                Tombstone {
                    git_sha: entry.metadata.commit.map(|commit| commit.sha),
                },
            );
        }
        if !plan.targets.is_empty() {
            self.save()?;
        }
        self.cleanup_files()
    }

    pub fn cleanup_files(&self) -> Result<()> {
        let mut deleted = false;
        for &id in self.ledger.forgotten.keys() {
            let path = entries::path(self.repo, id);
            match fs::remove_file(&path) {
                Ok(()) => deleted = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => {
                    return Err(error).with_context(|| format!("remove forgotten entry {id}"));
                }
            }
        }
        if deleted {
            sync_dir(&self.repo.entries())?;
        }
        Ok(())
    }

    pub fn forgotten_ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.ledger.forgotten.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::documents::Snapshot;

    #[test]
    fn interrupted_summary_and_forget_recover_from_durable_intent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = Repo::new(temp.path())?;
        let source = entries::persist(
            &repo,
            "temporary development finding",
            ContextFields::default(),
        )?;
        let unrelated = entries::persist(&repo, "keep this untouched", ContextFields::default())?;
        let mut guard = Guard::acquire(&repo, true)?;
        let ledger_path = repo.local().join(NAME);
        fs::create_dir(&ledger_path)?;
        assert!(
            guard
                .apply(&[source.id], &BTreeMap::new(), "summary after interruption")
                .is_err()
        );
        fs::remove_dir(&ledger_path)?;
        drop(guard);
        assert_eq!(Snapshot::read(&repo)?.documents.len(), 2);
        let mut guard = Guard::acquire(&repo, true)?;
        let applied = guard.apply(&[source.id], &BTreeMap::new(), "summary after interruption")?;
        assert_eq!(fs::read_dir(repo.entries())?.count(), 3);
        assert_eq!(
            guard.ledger.archives()?.get(&source.id),
            Some(&applied.summary_id)
        );

        // Simulate death directly after the tombstone commit, with the file still present.
        guard
            .ledger
            .forgotten
            .insert(source.id, Tombstone { git_sha: None });
        guard.save()?;
        drop(guard);
        assert!(source.path.exists());
        let snapshot = Snapshot::read(&repo)?;
        assert_eq!(snapshot.documents.len(), 2);
        assert!(
            snapshot
                .documents
                .iter()
                .all(|entry| entry.provenance.id != source.id.to_string())
        );
        let guard = Guard::acquire(&repo, true)?;
        guard.cleanup_files()?;
        assert!(!source.path.exists());
        assert!(unrelated.path.exists());
        assert!(entries::path(&repo, applied.summary_id).exists());
        Ok(())
    }
}
