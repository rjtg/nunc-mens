use crate::entries::{self, Commit, ContextFields, EntryKind};
use crate::fs::{atomic_write_mode, read_regular, sync_dir};
use crate::ledger::{Guard, Ledger};
use crate::repo::Repo;
use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

const META_LIMIT: usize = 96 * 1024;
const DIFF_LIMIT: usize = 256 * 1024;
const HOOK_LIMIT: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub message_bytes: usize,
    pub max_paths: usize,
    pub diff_context_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            message_bytes: 16 * 1024,
            max_paths: 64,
            diff_context_bytes: 0,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64 * 1024).contains(&self.message_bytes),
            "git.message_bytes must be between 1 and 65536"
        );
        ensure!(self.max_paths <= 64, "git.max_paths must be at most 64");
        ensure!(
            self.diff_context_bytes <= 16 * 1024,
            "git.diff_context_bytes must be at most 16384"
        );
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Selection {
    pub all: bool,
    pub limit: Option<u32>,
    pub since: Option<NaiveDate>,
    pub range: Option<String>,
    pub dry_run: bool,
}

#[derive(Default)]
pub(crate) struct Counts {
    pub selected: usize,
    pub saved: usize,
    pub existing: usize,
    pub forgotten: usize,
}

enum Ingested {
    Saved,
    Existing,
    Forgotten,
}

fn git(repo: &Repo) -> Result<Command> {
    ensure!(repo.git, "this command requires a Git worktree");
    let mut command = Command::new("git");
    command
        .current_dir(&repo.root)
        .args(["--no-pager", "--literal-pathspecs"]);
    Ok(command)
}

fn bounded(mut command: Command, limit: usize) -> Result<(Vec<u8>, bool)> {
    let program = command
        .get_args()
        .nth(2)
        .unwrap_or_else(|| OsStr::new("operation"))
        .to_string_lossy()
        .into_owned();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start Git {program}"))?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .context("missing Git output")?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    let clipped = bytes.len() > limit;
    if clipped || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    read?;
    ensure!(
        status.success() || clipped,
        "Git {program} failed with {status}"
    );
    bytes.truncate(limit);
    Ok((bytes, clipped))
}

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn resolve(repo: &Repo, rev: &str) -> Result<String> {
    ensure!(
        !rev.is_empty() && !rev.starts_with('-'),
        "expected a Git commit revision"
    );
    let mut cmd = git(repo)?;
    cmd.args(["rev-parse", "--verify", "--end-of-options"])
        .arg(format!("{rev}^{{commit}}"));
    let (bytes, _) = bounded(cmd, 1024).with_context(|| format!("resolve Git commit {rev}"))?;
    let sha = std::str::from_utf8(&bytes)?.trim();
    ensure!(valid_sha(sha), "Git did not produce a full commit hash");
    Ok(sha.to_owned())
}

pub(crate) fn ingest(repo: &Repo, rev: &str) -> Result<Counts> {
    let sha = resolve(repo, rev)?;
    let settings = repo.config()?.git;
    let mut counts = Counts::default();
    count(repo, &sha, &settings, false, &mut counts)?;
    Ok(counts)
}

pub(crate) fn import(repo: &Repo, selection: Selection) -> Result<Counts> {
    let settings = repo.config()?.git;
    ensure!(
        selection.range.is_none() || !selection.all,
        "--range and --all are mutually exclusive"
    );
    let mut cmd = git(repo)?;
    cmd.args(["rev-list", "--topo-order"]);
    if let Some(limit) = selection.limit {
        cmd.arg(format!("--max-count={limit}"));
    }
    if let Some(since) = selection.since {
        cmd.arg(format!("--since-as-filter={since}T00:00:00Z"));
    }
    if let Some(range) = selection.range {
        let (left, right) = range
            .split_once("..")
            .context("range must have form REV..REV")?;
        ensure!(
            !right.starts_with('.') && !right.contains(".."),
            "range must have form REV..REV"
        );
        cmd.arg(format!(
            "{}..{}",
            resolve(repo, left)?,
            resolve(repo, right)?
        ));
    } else if selection.all {
        cmd.arg("--all");
    } else {
        // An unborn HEAD has nothing to sync or import.
        let mut head = git(repo)?;
        head.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        if !head
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success()
        {
            return Ok(Counts::default());
        }
        cmd.arg("HEAD");
    }
    cmd.arg("--");
    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .context("stream Git history")?;
    let run = || -> Result<Counts> {
        let lines = BufReader::new(
            child
                .stdout
                .as_mut()
                .context("missing Git history output")?,
        )
        .lines();
        let mut counts = Counts::default();
        for line in lines {
            let sha = line?;
            ensure!(valid_sha(&sha), "invalid full hash in Git history");
            count(repo, &sha, &settings, selection.dry_run, &mut counts)
                .with_context(|| format!("ingest Git commit {sha}"))?;
        }
        Ok(counts)
    };
    let mut run = run;
    let result = run();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let counts = result?;
    ensure!(
        status.success(),
        "Git history traversal failed with {status}"
    );
    Ok(counts)
}

fn count(
    repo: &Repo,
    sha: &str,
    settings: &Settings,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<()> {
    counts.selected += 1;
    let id = entries::durable_id(format!("git-commit-v1\0{}\0{sha}", repo.id).as_bytes());
    let result = if dry_run {
        let ledger = Ledger::read(repo)?;
        let result = current(repo, &ledger, id, sha)?.unwrap_or(Ingested::Saved);
        let label = match result {
            Ingested::Saved => "would import",
            Ingested::Existing => "existing",
            Ingested::Forgotten => "forgotten",
        };
        println!("{sha} {label}");
        result
    } else {
        let guard = Guard::acquire(repo, true)?;
        if let Some(result) = current(repo, &guard.ledger, id, sha)? {
            result
        } else {
            let (commit, body) = capture(repo, sha, settings)?;
            let mut metadata = entries::metadata(repo, id, ContextFields::default());
            metadata.kind = EntryKind::GitCommit;
            metadata.timestamp = commit.committed_at;
            metadata.commit = Some(commit);
            entries::persist_record(repo, &body, metadata)?;
            Ingested::Saved
        }
    };
    match result {
        Ingested::Saved => counts.saved += 1,
        Ingested::Existing => counts.existing += 1,
        Ingested::Forgotten => counts.forgotten += 1,
    }
    Ok(())
}

fn current(repo: &Repo, ledger: &Ledger, id: Uuid, sha: &str) -> Result<Option<Ingested>> {
    if ledger.known_commit(id, sha) {
        return Ok(Some(Ingested::Forgotten));
    }
    let path = entries::path(repo, id);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let entry = entries::read(&path)?;
            ensure!(
                entry
                    .metadata
                    .commit
                    .is_some_and(|commit| commit.sha == sha),
                "Git memory ID conflict for {id}"
            );
            Ok(Some(Ingested::Existing))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("inspect Git memory"),
    }
}

fn capture(repo: &Repo, sha: &str, settings: &Settings) -> Result<(Commit, String)> {
    let mut cmd = git(repo)?;
    cmd.args([
        "show",
        "-s",
        "--no-notes",
        "--format=%H%x00%P%x00%aI%x00%cI%x00%an%x00%ae%x00%B",
        sha,
        "--",
    ]);
    let (bytes, clipped) = bounded(cmd, META_LIMIT)?;
    let mut parts = bytes
        .splitn(7, |byte| *byte == 0)
        .map(String::from_utf8_lossy);
    ensure!(
        parts.next().as_deref() == Some(sha),
        "unexpected commit hash in Git metadata"
    );
    let parents = parts.next().context("missing Git parents")?;
    let parent = parents.split_whitespace().next();
    let authored_at = parts.next().context("missing Git author date")?.parse()?;
    let committed_at = parts.next().context("missing Git commit date")?.parse()?;
    let author = clean(parts.next().context("missing Git author")?.as_ref(), 200);
    let author_email = clean(parts.next().context("missing Git email")?.as_ref(), 200);
    let message_raw = parts.next().context("missing Git message")?;
    let message = clean(&message_raw, settings.message_bytes);
    let (paths, additions, deletions, diff_clipped) = stats(repo, sha, parent, settings)?;
    let mut truncated = clipped || diff_clipped || message_raw.len() > settings.message_bytes;
    let mut doc = format!(
        "# Git commit {sha}\n\n{message}\n\n## Changes\n\n{additions} additions, {deletions} deletions across captured paths.\n"
    );
    for path in &paths {
        doc.push_str(&format!("- `{path}`\n"));
    }
    if settings.diff_context_bytes > 0 && !paths.is_empty() {
        let mut cmd = diff(repo, sha, parent)?;
        cmd.args(["-p", "-U1", "--"]).args(&paths);
        let (bytes, clipped) = bounded(cmd, settings.diff_context_bytes)?;
        truncated |= clipped;
        let patch = clean(
            &String::from_utf8_lossy(&bytes),
            settings.diff_context_bytes,
        );
        if !patch.trim().is_empty() {
            doc.push_str("\n## Diff context\n\n");
            for line in patch.lines() {
                doc.push_str(&format!("    {line}\n"));
            }
        }
    }
    Ok((
        Commit {
            sha: sha.into(),
            author,
            author_email,
            authored_at,
            committed_at,
            paths,
            additions,
            deletions,
            truncated,
        },
        doc,
    ))
}

fn diff(repo: &Repo, sha: &str, parent: Option<&str>) -> Result<Command> {
    let mut cmd = git(repo)?;
    if let Some(parent) = parent {
        ensure!(valid_sha(parent), "invalid first parent in Git metadata");
        cmd.args(["diff", parent, sha]);
    } else {
        cmd.args(["diff-tree", "--root", "--no-commit-id", "-r", sha]);
    }
    cmd.args([
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-renames",
    ]);
    Ok(cmd)
}

fn stats(
    repo: &Repo,
    sha: &str,
    parent: Option<&str>,
    settings: &Settings,
) -> Result<(Vec<String>, u64, u64, bool)> {
    let mut cmd = diff(repo, sha, parent)?;
    cmd.args(["--numstat", "-z", "--"]);
    let (bytes, clipped) = bounded(cmd, DIFF_LIMIT)?;
    let mut paths = Vec::new();
    let (mut additions, mut deletions) = (0_u64, 0_u64);
    let mut truncated = clipped;
    let mut paths_bytes = 0;
    for record in bytes.split_inclusive(|byte| *byte == 0) {
        let Some(line) = record
            .strip_suffix(&[0])
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        else {
            continue;
        };
        let mut fields = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if sensitive_path(path)
            || path.len() > 180
            || path.chars().any(char::is_control)
            || path.contains('`')
        {
            continue;
        }
        paths_bytes += serde_json::to_string(path)?.len() + 1;
        if paths.len() == settings.max_paths || paths_bytes > 10 * 1024 {
            truncated = true;
            break;
        }
        paths.push(path.to_owned());
        additions = additions.saturating_add(a.parse().unwrap_or(0));
        deletions = deletions.saturating_add(d.parse().unwrap_or(0));
    }
    Ok((paths, additions, deletions, truncated))
}

fn sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.split('/').any(|part| {
        part == ".env"
            || part.starts_with(".env.")
            || [
                ".nunc",
                ".aws",
                ".ssh",
                "credentials",
                "secrets",
                "id_rsa",
                "id_ed25519",
            ]
            .contains(&part)
            || [".pem", ".key", ".p12"]
                .iter()
                .any(|suffix| part.ends_with(suffix))
    })
}

fn clean(text: &str, limit: usize) -> String {
    let mut capped = limit.min(text.len());
    while !text.is_char_boundary(capped) {
        capped -= 1;
    }
    let mut out = String::new();
    for line in text[..capped].lines() {
        let lower = line.to_ascii_lowercase();
        if [
            "password",
            "passwd",
            "api_key",
            "api-key",
            "access_token",
            "private key",
            "authorization:",
            "bearer ",
            "secret=",
            "secret:",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            out.push_str("[redacted]");
        } else {
            out.extend(line.chars().filter(|&c| !c.is_control() || c == '\t'));
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

pub(crate) fn hook_path(repo: &Repo) -> Result<PathBuf> {
    let mut cmd = git(repo)?;
    cmd.args([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "hooks/post-commit",
    ]);
    let (bytes, clipped) = bounded(cmd, HOOK_LIMIT)?;
    ensure!(!clipped, "Git hook path exceeds limit");
    let path = PathBuf::from(OsStr::from_bytes(
        bytes.strip_suffix(b"\n").unwrap_or(&bytes),
    ));
    ensure!(path.is_absolute(), "Git returned a relative hook path");
    Ok(path)
}

fn hook_script(repo: &Repo) -> Result<Vec<u8>> {
    let exe = std::env::current_exe()?;
    let exe = exe
        .to_str()
        .context("nunc executable path must be UTF-8 for the hook")?;
    let quoted = format!("'{}'", exe.replace('\'', "'\\''"));
    let body = format!(
        "unset NUNC_REPO_ROOT\nif ! {quoted} git ingest HEAD >&2; then\n    printf '%s\\n' 'nunc post-commit capture failed; run nunc git sync to retry' >&2\nfi\nexit 0\n"
    );
    let digest = crate::repo::hash(body.as_bytes());
    Ok(format!(
        "#!/bin/sh\n# nunc-mens post-commit {} {digest}\n{body}",
        repo.id
    )
    .into_bytes())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hook {
    Absent,
    Managed,
    Existing,
}

impl Hook {
    pub fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Managed => "installed",
            Self::Existing => "existing hook (unmanaged)",
        }
    }
}

pub(crate) fn hook_status(repo: &Repo, path: &Path) -> Result<Hook> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Hook::Absent),
        Err(error) => return Err(error).context("inspect post-commit hook"),
    };
    if !meta.is_file() || meta.len() > HOOK_LIMIT as u64 {
        return Ok(Hook::Existing);
    }
    let bytes = read_regular(path, HOOK_LIMIT)?;
    let bytes = bytes.strip_prefix(b"#!/bin/sh\n").unwrap_or_default();
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Ok(Hook::Existing);
    };
    let prefix = format!("# nunc-mens post-commit {} ", repo.id);
    let digest = crate::repo::hash(&bytes[newline + 1..]);
    let header = format!("{prefix}{digest}");
    Ok(if header.as_bytes() == &bytes[..newline] {
        Hook::Managed
    } else {
        Hook::Existing
    })
}

pub(crate) fn change_hook(repo: &Repo, install: bool) -> Result<PathBuf> {
    let path = hook_path(repo)?;
    let parent = path.parent().context("missing Git hook directory")?;
    let _guard = Guard::acquire(repo, true)?;
    ensure!(
        parent.exists(),
        "create the Git hook directory {} or configure a repository-local core.hooksPath first",
        parent.display()
    );
    let actual = parent.canonicalize()?;
    ensure!(
        actual.starts_with(&repo.root),
        "shared hook location {}; configure custom chaining with nunc git ingest HEAD",
        actual.display()
    );
    match (install, hook_status(repo, &path)?) {
        (_, Hook::Existing) => bail!(
            "preserving existing hook at {}; add nunc git ingest HEAD to it explicitly",
            path.display()
        ),
        (true, Hook::Absent) => atomic_write_mode(&path, &hook_script(repo)?, false, 0o700)?,
        (false, Hook::Managed) => {
            fs::remove_file(&path)?;
            sync_dir(parent)?;
        }
        _ => (),
    }
    Ok(path)
}
