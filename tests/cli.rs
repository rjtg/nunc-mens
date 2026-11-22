use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    cwd: PathBuf,
    state: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path().join("repo");
        let cwd = repo.join("nested");
        fs::create_dir_all(&cwd)?;
        checked(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .output()?,
        )?;
        Ok(Self {
            state: dir.path().join("state"),
            cache: dir.path().join("models"),
            _dir: dir,
            repo,
            cwd,
        })
    }

    fn nunc(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nunc"));
        cmd.args(args)
            .current_dir(&self.cwd)
            .env_remove("NUNC_REPO_ROOT")
            .env("NUNC_STATE_DIR", &self.state)
            .env("NUNC_MODEL_CACHE_DIR", &self.cache);
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        checked(self.nunc(args).output()?)
    }

    fn local(&self) -> PathBuf {
        self.repo.join(".nunc")
    }

    fn entries(&self) -> Result<Vec<String>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(self.local().join("entries"))? {
            let path = item?.path();
            // The FIFO server can still be staging a note in this directory.
            if path.extension().is_some_and(|ext| ext == "md") {
                entries.push(fs::read_to_string(path)?);
            }
        }
        Ok(entries)
    }
}

fn checked(output: Output) -> Result<Output> {
    ensure!(
        output.status.success(),
        "command failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn eventually(mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready()? {
        ensure!(Instant::now() < deadline, "condition did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

struct Server(Child);

impl Server {
    fn start(fixture: &Fixture, args: &[&str]) -> Result<Self> {
        let mut cmd = fixture.nunc(&["serve"]);
        let server = Self(
            cmd.args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        eventually(|| Ok(fixture.local().join("session.json").is_file()))?;
        Ok(server)
    }

    fn stop(&mut self) -> Result<()> {
        // The child is still owned by this test. A graceful shutdown must reap it.
        ensure!(
            unsafe { libc::kill(self.0.id().try_into()?, libc::SIGTERM) } == 0,
            "signal server"
        );
        eventually(|| match self.0.try_wait()? {
            Some(status) => {
                ensure!(status.success(), "server exited with {status}");
                Ok(true)
            }
            None => Ok(false),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn body(entry: &str) -> Result<&str> {
    entry
        .strip_prefix("---\n")
        .and_then(|entry| entry.split_once("\n---\n"))
        .map(|(_, body)| body)
        .context("missing front matter")
}

#[test]
fn captures_from_nested_paths_without_a_server() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["init"])?;
    fixture.run(&["write", "offline", "capture"])?;
    fixture.run(&["shorthand", "capture"])?;
    assert!(fixture.local().join("config.toml").exists());
    assert!(!fixture.cwd.join(".nunc").exists());
    let mut entries = fixture.entries()?;
    entries.sort_by_key(|entry| body(entry).unwrap_or_default().to_owned());
    assert_eq!(body(&entries[0])?, "offline capture");
    assert_eq!(body(&entries[1])?, "shorthand capture");
    Ok(())
}

#[test]
fn fifo_preserves_raw_and_client_records_under_concurrency() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut server = Server::start(&fixture, &["--boundary", "test-boundary"])?;
    let duplicate = fixture.nunc(&["serve"]).output()?;
    assert!(!duplicate.status.success());
    {
        let mut fifo = raw_writer(&fixture.local().join("input.fifo"))?;
        fifo.write_all(b"raw\r\n\n```\ncode\n```\n--test-")?;
        fifo.write_all(b"boundary\r\nsecond raw\n--test-boundary\n")?;
    }
    {
        // Keep a writer open so there is no EOF between an incomplete raw note
        // and the next cooperative client frame.
        let mut fifo = raw_writer(&fixture.local().join("input.fifo"))?;
        fifo.write_all(b"abandoned record\n--test-boundary")?;
        fixture.run(&["write", "recovered client note"])?;
    }
    let collision = fixture
        .nunc(&["write", "--", "collision\n--test-boundary"])
        .output()?;
    assert!(!collision.status.success());
    let mut clients = Vec::new();
    let mut expected = vec![
        "raw\r\n\n```\ncode\n```\n".to_owned(),
        "second raw\n".to_owned(),
        "recovered client note".to_owned(),
    ];
    for id in 0..6 {
        let msg = format!("client {id}\n\n{}", "x".repeat(21_000));
        let child = fixture
            .nunc(&["write", &msg])
            .stdout(Stdio::null())
            .spawn()?;
        clients.push(child);
        expected.push(msg);
    }
    for mut child in clients {
        assert!(child.wait()?.success());
    }
    eventually(|| Ok(fixture.entries()?.len() == expected.len()))?;
    let entries = fixture.entries()?;
    let mut actual = entries
        .iter()
        .map(|entry| Ok(body(entry)?.to_owned()))
        .collect::<Result<Vec<_>>>()?;
    expected.sort();
    actual.sort();
    assert_eq!(expected, actual);
    server.stop()?;
    assert!(!fixture.local().join("session.json").exists());
    assert!(!fixture.local().join("input.fifo").exists());
    Ok(())
}

fn raw_writer(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::File::options()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?)
}

#[test]
fn restarts_after_forced_exit_and_discards_incomplete_stream() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut server = Server::start(&fixture, &[])?;
    let state_path = fixture.local().join("session.json");
    let before: Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    {
        let mut fifo = raw_writer(&fixture.local().join("input.fifo"))?;
        fifo.write_all(b"never committed\n")?;
    }
    server.0.kill()?;
    server.0.wait()?;
    fixture.run(&["write", "stale session fallback"])?;
    let mut restarted = Server::start(&fixture, &[])?;
    eventually(|| {
        let bytes = match fs::read(&state_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        let after: Value = serde_json::from_slice(&bytes)?;
        Ok(before["boundary"] != after["boundary"])
    })?;
    fixture.run(&["write", "fresh session"])?;
    eventually(|| Ok(fixture.entries()?.len() == 2))?;
    assert!(
        fixture
            .entries()?
            .iter()
            .all(|entry| !entry.contains("never committed"))
    );
    restarted.stop()?;
    Ok(())
}

#[test]
fn missing_model_is_actionable_while_capture_still_works() -> Result<()> {
    let fixture = Fixture::new()?;
    for args in [
        &["search", "--json", "earlier design"][..],
        &["reindex"][..],
    ] {
        let output = fixture.nunc(args).output()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("model unavailable"));
        assert!(stderr.contains("nunc models download"));
    }
    assert!(!fixture.cache.exists());
    fixture.run(&["log", "a durable entry before embeddings"])?;
    assert_eq!(fixture.entries()?.len(), 1);
    Ok(())
}

fn real_model_cache() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("NUNC_TEST_MODEL_CACHE").context(
            "set NUNC_TEST_MODEL_CACHE to an absolute cache prepared with nunc models download",
        )?,
    ))
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_fastembed_semantic_search_and_json_provenance() -> Result<()> {
    let fixture = Fixture::new()?;
    let cache = real_model_cache()?;
    let nunc = |args: &[&str]| -> Result<Output> {
        checked(
            fixture
                .nunc(args)
                .env("NUNC_MODEL_CACHE_DIR", &cache)
                .output()?,
        )
    };
    let note = "Staging authentication failed because the deployment machine's clock drifted and its signed tokens expired.";
    nunc(&["log", note])?;
    nunc(&[
        "log",
        "Raspberry bushes thrive in sunny locations with acidic soil.",
    ])?;
    fs::create_dir(fixture.repo.join("docs"))?;
    fs::write(
        fixture.repo.join("docs/policies.md"),
        "Authentication policy\nSynchronize clocks with NTP.\n",
    )?;
    nunc(&["reindex", "--source", "docs"])?;
    let result: Value = serde_json::from_slice(
        &nunc(&[
            "search",
            "--json",
            "--type",
            "memory",
            "--limit",
            "1",
            "Warum wurde die Anmeldung in der Testumgebung abgelehnt?",
        ])?
        .stdout,
    )?;
    assert_eq!(result["mode_used"], "semantic");
    assert_eq!(result["warnings"].as_array().map(Vec::len), Some(0));
    let hit = &result["results"][0];
    assert_eq!(hit["text"], note);
    assert_eq!(hit["kind"], "memory");
    let id = hit["id"].as_str().context("missing entry id")?;
    assert_eq!(hit["path"], format!(".nunc/entries/{id}.md"));
    assert_eq!(hit["rank"], 1);
    assert!(hit["timestamp"].is_string());
    let source: Value = serde_json::from_slice(
        &nunc(&[
            "search",
            "--json",
            "--source",
            "docs",
            "time synchronization",
        ])?
        .stdout,
    )?;
    assert_eq!(source["results"][0]["path"], "docs/policies.md");
    assert_eq!(source["results"][0]["line_start"], 1);
    assert_eq!(source["results"][0]["line_end"], 2);
    Ok(())
}

#[test]
fn no_fifo_reader_falls_back_and_runtime_symlinks_are_rejected() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["init"])?;
    let fifo = fixture.local().join("input.fifo");
    checked(
        Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&fifo)
            .output()?,
    )?;
    let owner = fixture.local().join("serve.lock");
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let lock = fs::File::options()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(owner)?;
    lock.lock()?;
    let meta = fs::metadata(&fifo)?;
    fs::write(
        fixture.local().join("session.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 2, "id": uuid::Uuid::new_v4(), "pid": std::process::id(),
            "boundary": "fake", "device": meta.dev(), "inode": meta.ino()
        }))?,
    )?;
    let receipt = fixture.run(&["write", "no reader"])?;
    assert!(String::from_utf8(receipt.stdout)?.starts_with("Saved "));
    drop(lock);

    fs::remove_file(fixture.local().join("session.json"))?;
    fs::remove_file(&fifo)?;
    let target = fixture.repo.join("untouched");
    fs::write(&target, "existing content")?;
    std::os::unix::fs::symlink(&target, &fifo)?;
    let output = fixture.nunc(&["serve"]).output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("unexpected runtime file"));
    assert_eq!(fs::read_to_string(target)?, "existing content");
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_watcher_reconciles_changes_deletions_and_restart() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    fs::create_dir(fixture.repo.join("docs"))?;
    fs::create_dir(fixture.repo.join("src"))?;
    let doc = fixture.repo.join("docs/rotation.md");
    let deleted = fixture.repo.join("src/old.rs");
    fs::write(&doc, "Keys rotate weekly.\n")?;
    fs::write(
        &deleted,
        "// Old procedure: disable access during key rollover.\n",
    )?;
    let mut server = Server::start(&fixture, &["--source", "docs", "--source", "src"])?;
    fixture.run(&[
        "write",
        "During rollover, clients keep the preceding token in memory for one minute.",
    ])?;

    let index_path = || -> Result<Option<PathBuf>> {
        let states = match fs::read_dir(&fixture.state) {
            Ok(states) => states,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        for state in states {
            let path = state?.path().join("index.sqlite3");
            if path.is_file() {
                return Ok(Some(path));
            }
        }
        Ok(None)
    };
    eventually(|| {
        let Some(path) = index_path()? else {
            return Ok(false);
        };
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Ok(conn.query_row("SELECT count(*) FROM documents", [], |row| {
            row.get::<_, usize>(0)
        })? == 3)
    })?;

    fs::write(
        doc,
        "Keys rotate weekly.\nThe previous token stays valid for sixty seconds.\n",
    )?;
    fs::remove_file(deleted)?;
    eventually(|| {
        let Some(path) = index_path()? else {
            return Ok(false);
        };
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let hits: usize = conn.query_row(
            "SELECT count(*) FROM documents WHERE (path = 'docs/rotation.md' AND line_end = 2)
             OR path = 'src/old.rs'",
            [],
            |row| row.get(0),
        )?;
        let count: usize =
            conn.query_row("SELECT count(*) FROM documents", [], |row| row.get(0))?;
        Ok(hits == 1 && count == 2)
    })?;

    let search = || -> Result<Value> {
        Ok(serde_json::from_slice(
            &fixture
                .run(&["search", "--json", "credentials grace period"])?
                .stdout,
        )?)
    };
    let before = search()?;
    assert_eq!(before["warnings"], serde_json::json!([]));
    let rows = before["results"].as_array().context("missing hits")?;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row["kind"] == "source" && row["line_end"] == 2)
    );
    assert!(rows.iter().any(|row| row["kind"] == "memory"));
    server.stop()?;
    let mut server = Server::start(&fixture, &[])?;
    fixture.run(&["reindex", "--full"])?;
    assert_eq!(search()?, before);
    server.stop()?;
    Ok(())
}

impl Fixture {
    fn json(&self, args: &[&str]) -> Result<Value> {
        Ok(serde_json::from_slice(&self.run(args)?.stdout)?)
    }

    fn log_id(&self, text: &str) -> Result<String> {
        String::from_utf8(self.run(&["log", text])?.stdout)?
            .split_whitespace()
            .nth(1)
            .map(str::to_owned)
            .context("missing receipt ID")
    }

    fn git(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo)
            .args([
                "-c",
                "user.name=Test Agent",
                "-c",
                "user.email=agent@example.test",
            ])
            .args(args)
            .env("NUNC_STATE_DIR", &self.state)
            .env("NUNC_MODEL_CACHE_DIR", &self.cache);
        checked(cmd.output()?)
    }

    fn commit(&self, body: &str, message: &str) -> Result<String> {
        fs::write(self.repo.join("note.txt"), body)?;
        self.git(&["add", "note.txt"])?;
        self.git(&["commit", "-qm", message])?;
        Ok(String::from_utf8(self.git(&["rev-parse", "HEAD"])?.stdout)?
            .trim()
            .to_owned())
    }

    fn entry_bytes(&self, id: &str) -> Result<Vec<u8>> {
        Ok(fs::read(
            self.local().join("entries").join(format!("{id}.md")),
        )?)
    }

    fn event_time(&self, id: &str, timestamp: &str) -> Result<()> {
        let text = String::from_utf8(self.entry_bytes(id)?)?;
        let (header, body) = text
            .strip_prefix("---\n")
            .and_then(|text| text.split_once("\n---\n"))
            .context("missing fixture front matter")?;
        let mut metadata: Value = serde_json::from_str(header)?;
        metadata["timestamp"] = timestamp.into();
        fs::write(
            self.local().join("entries").join(format!("{id}.md")),
            format!("---\n{metadata}\n---\n{body}"),
        )?;
        Ok(())
    }

    fn summary(&self, ids: &str, body: &str) -> Result<String> {
        let response = self.json(&[
            "consolidate",
            "apply",
            "--entries",
            ids,
            "--summary",
            body,
            "--json",
        ])?;
        response["summary_id"]
            .as_str()
            .map(str::to_owned)
            .context("missing summary ID")
    }
}

#[test]
fn timeline_and_search_use_local_calendar_days_without_a_model() -> Result<()> {
    let fixture = Fixture::new()?;
    let event = |label, at| -> Result<String> {
        let id = fixture.log_id(label)?;
        fixture.event_time(&id, at)?;
        Ok(id)
    };
    event("The prior day", "2026-03-08T04:59:59Z")?;
    let early = event("Local midnight", "2026-03-08T05:00:00Z")?;
    let mut ties = vec![
        event("Identical clock time A", "2026-03-08T12:00:00Z")?,
        event("Identical clock time B", "2026-03-08T13:00:00+01:00")?,
    ];
    ties.sort();
    let late = event("Late after spring-forward", "2026-03-09T03:59:59Z")?;
    event("Next local midnight", "2026-03-09T04:00:00Z")?;
    let fall_first = event("First 01:30", "2026-11-01T01:30:00-04:00")?;
    let fall_second = event("Second 01:30", "2026-11-01T01:30:00-05:00")?;
    let fall_late = event("The 25th hour", "2026-11-02T04:30:00Z")?;
    let local = |args: &[&str]| -> Result<Output> {
        checked(fixture.nunc(args).env("TZ", "America/New_York").output()?)
    };
    let ids = |output: Output| -> Result<Vec<String>> {
        Ok(String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect())
    };
    let mut spring = vec![early];
    spring.extend(ties);
    spring.push(late);
    assert_eq!(
        ids(local(&[
            "search",
            "--from",
            "2026-03-08",
            "--to",
            "2026-03-08",
            "--ids"
        ])?)?,
        spring
    );
    assert_eq!(
        ids(local(&["timeline", "--date", "2026-11-01", "--ids"])?)?,
        [fall_first, fall_second, fall_late]
    );
    let group = [
        "timeline",
        "--date",
        "2026-03-08",
        "--group-by",
        "day",
        "--json",
    ];
    let output = local(&group)?;
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, local(&group)?.stdout);
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["days"]["2026-03-08"], serde_json::json!(spring));
    assert_eq!(report["results"][0]["title"], "Local midnight");
    assert!(
        report["results"][0]["path"]
            .as_str()
            .is_some_and(|p| p.starts_with(".nunc/entries/"))
    );

    let yesterday = chrono::Utc::now()
        .date_naive()
        .pred_opt()
        .context("fixture date")?;
    let yesterday_id = event(
        "What I worked on yesterday",
        &format!("{yesterday}T12:00:00Z"),
    )?;
    let output = checked(
        fixture
            .nunc(&["search", "--yesterday", "--ids"])
            .env("TZ", "UTC")
            .output()?,
    )?;
    assert!(ids(output)?.contains(&yesterday_id));
    let fresh = fixture.log_id("A freshly recorded activity")?;
    assert_eq!(
        fixture.json(&["search", "--since", "10m", "--json"])?["results"][0]["id"],
        fresh
    );
    fs::create_dir(fixture.repo.join("docs"))?;
    fs::write(fixture.repo.join("docs/untimed.md"), "No event timestamp.")?;
    fixture.run(&["init", "--source", "docs"])?;
    assert_eq!(
        fixture.json(&["search", "--since", "1d", "--source", "docs", "--json"])?["results"],
        serde_json::json!([])
    );
    assert!(!fixture.state.exists());
    assert!(!fixture.cache.exists());
    Ok(())
}

#[test]
fn timeline_respects_commit_time_neighbors_and_nested_visibility() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(fixture.repo.join("worker.txt"), "worker")?;
    fixture.git(&["add", "worker.txt"])?;
    checked(
        Command::new("git")
            .current_dir(&fixture.repo)
            .args([
                "-c",
                "user.name=Agent",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "Historical worker change",
            ])
            .env("GIT_AUTHOR_DATE", "2024-07-04T01:00:00Z")
            .env("GIT_COMMITTER_DATE", "2024-07-04T02:00:00Z")
            .output()?,
    )?;
    fixture.run(&["git", "ingest", "HEAD"])?;
    let git_only = fixture.json(&["timeline", "--entry-kind", "git-commit", "--json"])?;
    let git = &git_only["results"][0];
    assert_eq!(git["timestamp"], "2024-07-04T02:00:00+00:00");
    assert_eq!(git["entry_kind"], "git-commit");
    assert_eq!(git["title"], "Historical worker change");
    assert_eq!(git["git"]["sha"].as_str().map(str::len), Some(40));
    let nearby = fixture.log_id("Manual evidence about the same worker.")?;
    fixture.event_time(&nearby, "2024-07-04T01:30:00Z")?;
    let from = fixture.json(&[
        "timeline",
        "--around",
        git["id"].as_str().context("missing commit ID")?,
        "--window",
        "30m",
        "--json",
    ])?;
    assert_eq!(from["temporal_context"]["activities"][0]["id"], nearby);
    assert_eq!(from["anchor"]["path"], git["path"]);
    assert_eq!(from["anchor"]["git"]["sha"], git["git"]["sha"]);
    assert_eq!(
        from["temporal_context"]["activities"][0]["anchors"][0]["delta_millis"],
        -1_800_000
    );

    let a = fixture.log_id("First incident evidence.")?;
    let b = fixture.log_id("Second incident evidence.")?;
    let c = fixture.log_id("Additional incident evidence.")?;
    let inner = fixture.summary(&format!("{a},{b}"), "Inner incident summary.")?;
    let outer = fixture.summary(&format!("{inner},{c}"), "Outer incident summary.")?;
    let list = |raw: bool| -> Result<Vec<String>> {
        let mut args = vec!["timeline", "--since", "10m", "--ids"];
        if raw {
            args.push("--raw");
        }
        Ok(String::from_utf8(fixture.run(&args)?.stdout)?
            .lines()
            .map(str::to_owned)
            .collect())
    };
    assert_eq!(list(false)?, std::slice::from_ref(&outer));
    assert_eq!(list(true)?.len(), 5);
    fixture.run(&["forget", &b, "--yes"])?;
    assert!(!list(true)?.contains(&b));
    fixture.run(&["consolidate", "undo", &outer])?;
    fixture.run(&["consolidate", "undo", &inner])?;
    assert_eq!(list(false)?, [a, c]);
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_temporal_search_separates_ranked_anchors_from_overlapping_windows() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let event = |label, at| -> Result<String> {
        let id = fixture.log_id(label)?;
        fixture.event_time(&id, at)?;
        Ok(id)
    };
    let earlier = event(
        "SQLite lease guards the writer transaction.",
        "2026-02-02T12:00:00Z",
    )?;
    let later = event(
        "Durable publication requires a SQLite writer lease.",
        "2026-02-02T12:30:00Z",
    )?;
    let neighbor = event(
        "Review certificate expiry with the deployment team.",
        "2026-02-02T11:00:00Z",
    )?;
    let archived = event("Old incident recap.", "2026-02-02T11:05:00Z")?;
    fixture.run(&["archive", &archived])?;
    fs::create_dir(fixture.repo.join("docs"))?;
    fs::write(
        fixture.repo.join("docs/worker.md"),
        "# SQLite writer lease and transaction",
    )?;
    fixture.run(&["reindex", "--source", "docs"])?;
    let args = [
        "search",
        "SQLite writer locking",
        "--from",
        "2026-02-02T12:00:00Z",
        "--to",
        "2026-02-02T13:00:00Z",
        "--related-by",
        "time",
        "--limit",
        "2",
        "--window",
        "90m",
        "--json",
    ];
    let report = fixture.json(&args)?;
    let anchors: std::collections::BTreeSet<_> = report["results"]
        .as_array()
        .context("missing semantic results")?
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .collect();
    assert_eq!(anchors, [&*earlier, &*later].into_iter().collect());
    let context = &report["temporal_context"];
    assert_eq!(context["window_seconds"], 5400);
    let activities = context["activities"]
        .as_array()
        .context("missing activities")?;
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0]["id"], neighbor);
    let relations: std::collections::BTreeMap<_, _> = activities[0]["anchors"]
        .as_array()
        .context("missing activity relations")?
        .iter()
        .filter_map(|relation| {
            Some((
                relation["anchor_id"].as_str()?,
                relation["delta_millis"].as_i64()?,
            ))
        })
        .collect();
    assert_eq!(
        relations,
        [(&*earlier, -3_600_000), (&*later, -5_400_000)]
            .into_iter()
            .collect()
    );
    let before = fixture.json(&["timeline", "--around", &earlier, "--raw", "--json"])?;
    assert!(
        before["temporal_context"]["activities"]
            .as_array()
            .context("missing raw activities")?
            .iter()
            .any(|activity| activity["id"] == archived && activity["status"] == "archived")
    );
    fixture.run(&["reindex", "--full"])?;
    assert_eq!(
        before,
        fixture.json(&["timeline", "--around", &earlier, "--raw", "--json"])?
    );
    Ok(())
}

#[test]
fn topic_listing_handles_unindexed_events_and_ledger_visibility() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.log_id("Reason for deferred topic assignment.")?;
    let second = fixture.log_id("Another recorded decision.")?;
    fixture.run(&["archive", &second])?;
    let args = ["topics", "--since", "1h", "--json"];
    let report = fixture.json(&args)?;
    assert_eq!(report["topics"], serde_json::json!([]));
    assert_eq!(report["ungrouped"][0]["id"], first);
    assert_eq!(report["ungrouped"][0]["reason"], "missing-embedding");
    assert!(
        !report["warnings"]
            .as_array()
            .context("missing warnings")?
            .is_empty()
    );
    assert_eq!(
        report,
        fixture.json(&["timeline", "--since", "1h", "--group-by", "topic", "--json"])?
    );
    let raw = fixture.json(&["topics", "--since", "1h", "--raw", "--json"])?;
    assert_eq!(raw["ungrouped"].as_array().map(Vec::len), Some(2));
    assert_eq!(raw["ungrouped"][1]["status"], "archived");
    let ids = fixture.run(&[
        "topics", "--around", &first, "--window", "1h", "--raw", "--ids",
    ])?;
    assert_eq!(ids.stdout, format!("{second}\n").as_bytes());
    assert!(ids.stderr.is_empty());
    fixture.run(&["forget", &second, "--yes"])?;
    assert_eq!(
        fixture.json(&["topics", "--since", "1h", "--raw", "--json"])?["ungrouped"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[test]
fn keyword_search_builds_and_updates_without_a_model() -> Result<()> {
    let fixture = Fixture::new()?;
    let note = fixture.log_id("# SQLite writer\n\nWriterLock reports SQLITE_BUSY. Pass --summary-stdin.\n\n## Cleanup\n\nRetain a backup.")?;
    let other = fixture.log_id("The summary stdin option reads from a pipe.")?;
    let missing = fixture
        .nunc(&["search", "--mode", "hybrid", "WriterLock", "--json"])
        .output()?;
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8(missing.stderr)?.contains("model unavailable"));
    fs::create_dir_all(fixture.repo.join("docs/design"))?;
    fs::write(
        fixture.repo.join("docs/design/sqlite.md"),
        "Atomic database publication.",
    )?;
    let commit = fixture.commit("state updated", "SQLite transaction checkpoint")?;
    fixture.run(&["git", "ingest", "HEAD"])?;
    fixture.run(&["reindex", "--keyword-only", "--source", "docs"])?;
    let search = |query: &str| {
        fixture.json(&[
            "search",
            "--mode",
            "keyword",
            "--explain",
            "--json",
            "--",
            query,
        ])
    };
    for query in ["WriterLock", "SQLITE_BUSY", "--summary-stdin", &note] {
        let result = search(query)?;
        assert_eq!(result["mode_used"], "keyword");
        assert_eq!(result["pipeline"]["keyword"], "applied");
        assert_eq!(result["pipeline"]["semantic"], "skipped");
        assert!(
            result["pipeline"]["scored_chunks"]
                .as_u64()
                .is_some_and(|count| count > 0)
        );
        assert_eq!(result["results"][0]["id"], note);
        assert_eq!(result["results"][0]["explanation"]["exact_match"], true);
        assert!(
            result["results"][0]["explanation"]
                .get("similarity")
                .is_none()
        );
    }
    assert!(
        search("WriterLock")?["results"][0]["explanation"]["bm25"]
            .as_f64()
            .is_some_and(|bm25| bm25 < 0.0)
    );
    for path in ["docs/design/sqlite.md", "design/sqlite.md"] {
        assert_eq!(search(path)?["results"][0]["path"], "docs/design/sqlite.md");
    }
    assert_eq!(search("esign/sqlite.md")?["results"], serde_json::json!([]));
    for query in [&commit[..12], &commit] {
        assert_eq!(search(query)?["results"][0]["git"]["sha"], commit);
    }
    assert_eq!(search("\"\" \"\"")?["results"], serde_json::json!([]));
    search("WriterLock\" OR (source:**)")?;
    let format = fixture.run(&[
        "search",
        "--mode",
        "keyword",
        "SQLITE_BUSY",
        "--format",
        "markdown",
        "--max-tokens",
        "200",
    ])?;
    assert!(String::from_utf8(format.stdout)?.contains("SQLITE_BUSY"));
    fixture.run(&["archive", &note])?;
    let hidden = search("SQLITE_BUSY")?;
    assert_eq!(hidden["results"], serde_json::json!([]));
    assert_eq!(hidden["pipeline"]["excluded_archived_or_undone_chunks"], 1);
    let raw = fixture.json(&[
        "search",
        "--mode",
        "keyword",
        "SQLITE_BUSY",
        "--raw",
        "--json",
    ])?;
    assert_eq!(raw["results"][0]["id"], note);
    fixture.run(&["unarchive", &note])?;
    assert_eq!(search("SQLITE_BUSY")?["results"][0]["id"], note);
    fixture.run(&["forget", &note, "--yes"])?;
    assert_eq!(search("SQLITE_BUSY")?["results"], serde_json::json!([]));
    let surviving = search("summary stdin")?;
    assert_eq!(surviving["results"][0]["id"], other);
    let summary = fixture.summary(&other, "Stdin summary input contract.")?;
    fixture.run(&["reindex", "--keyword-only"])?;
    assert_eq!(search("summary stdin")?["results"][0]["id"], summary);
    fixture.run(&["consolidate", "undo", &summary])?;
    assert_eq!(search("summary stdin")?["results"][0]["id"], other);
    fs::remove_file(fixture.repo.join("docs/design/sqlite.md"))?;
    fixture.run(&["reindex", "--keyword-only"])?;
    assert_eq!(
        search("database publication")?["results"],
        serde_json::json!([])
    );
    let doctor = fixture.json(&["doctor", "--deep", "--json"])?;
    assert!(doctor["findings"].get("EMBEDDINGS_PENDING").is_some());
    assert!(doctor["findings"].get("INVALID_EMBEDDING").is_none());
    assert!(!fixture.cache.exists());
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn keyword_index_upgrades_to_shared_semantic_chunks() -> Result<()> {
    let fixture = Fixture::new()?;
    let note = fixture.log_id("# Transactions\n\nWriterLock guards SQLite commit publication.")?;
    let other = fixture.log_id("Concurrent database commits must serialize.")?;
    fixture.run(&["reindex", "--keyword-only"])?;
    let cache = real_model_cache()?;
    let run = |args: &[&str]| {
        fixture
            .nunc(args)
            .env("NUNC_MODEL_CACHE_DIR", &cache)
            .output()
    };
    let pending = run(&["search", "WriterLock"])?;
    assert!(!pending.status.success());
    assert!(String::from_utf8(pending.stderr)?.contains("nunc reindex"));
    checked(run(&["reindex"])?)?;
    fixture.run(&["reindex", "--keyword-only"])?;
    let lexical = fixture.json(&["search", "--mode", "keyword", "WriterLock", "--json"])?;
    let semantic: Value = serde_json::from_slice(
        &checked(run(&["search", "serializing database commits", "--json"])?)?.stdout,
    )?;
    assert_eq!(lexical["results"][0]["id"], note);
    let semantic_note = semantic["results"]
        .as_array()
        .and_then(|results| results.iter().find(|hit| hit["id"] == note))
        .context("semantic search omitted the keyword hit")?;
    assert!(lexical["results"][0]["matched_chunk"]["id"].is_string());
    assert_eq!(
        lexical["results"][0]["matched_chunk"]["id"],
        semantic_note["matched_chunk"]["id"]
    );
    let hybrid = |extra: &[&str]| -> Result<Value> {
        let mut args = vec![
            "search",
            "--mode",
            "hybrid",
            "WriterLock",
            "--explain",
            "--json",
        ];
        args.extend_from_slice(extra);
        Ok(serde_json::from_slice(&checked(run(&args)?)?.stdout)?)
    };
    let mixed = hybrid(&[])?;
    assert_eq!(mixed["mode_used"], "hybrid");
    assert_eq!(mixed["pipeline"]["fusion"], "applied");
    assert_eq!(mixed["pipeline"]["fusion_counts"]["keyword_candidates"], 1);
    assert_eq!(mixed["results"][0]["id"], note);
    let explanation = &mixed["results"][0]["explanation"];
    let fusion = &explanation["fusion"];
    assert_eq!(fusion["keyword_rank"], 1);
    let native_sum = fusion["keyword_contribution"]
        .as_f64()
        .context("missing BM25 contribution")?
        + fusion["semantic_contribution"]
            .as_f64()
            .context("missing cosine contribution")?;
    let score = explanation["final_score"]
        .as_f64()
        .context("missing score")?;
    assert!((score - native_sum).abs() < 1e-12);
    assert_eq!(mixed["results"][1]["id"], other);
    assert!(mixed["results"][1]["explanation"].get("bm25").is_none());
    assert_eq!(mixed, hybrid(&[])?);
    let budgeted = hybrid(&[
        "--source",
        ".nunc/entries",
        "--limit",
        "1",
        "--max-tokens",
        "1",
    ])?;
    assert_eq!(budgeted["results"], serde_json::json!([]));
    assert_eq!(budgeted["pipeline"]["omitted_by_budget"], 1);
    assert_eq!(budgeted["pipeline"]["beyond_limit"], 1);
    let empty = hybrid(&["--type", "source"])?;
    assert_eq!(empty["results"], serde_json::json!([]));
    assert_eq!(empty["pipeline"]["fusion_counts"]["semantic_candidates"], 0);
    assert_eq!(empty["pipeline"]["fusion_counts"]["keyword_candidates"], 0);
    fixture.run(&["archive", &note])?;
    let hidden = hybrid(&[])?;
    assert_eq!(hidden["results"][0]["id"], other);
    assert_eq!(hidden["pipeline"]["excluded_archived_or_undone_chunks"], 1);
    assert_eq!(hybrid(&["--raw"])?["results"][0]["id"], note);
    fixture.run(&["forget", &note, "--yes"])?;
    checked(run(&["reindex", "--full"])?)?;
    assert_eq!(
        hybrid(&["--raw"])?["results"].as_array().map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[test]
fn socket_search_preserves_formats_visibility_and_rebuilds() -> Result<()> {
    let fixture = Fixture::new()?;
    let note = fixture.log_id("WriterLock guards commit publication")?;
    fixture.run(&["reindex", "--keyword-only"])?;
    let unavailable = fixture
        .nunc(&["search", "--server", "WriterLock"])
        .output()?;
    assert!(!unavailable.status.success());
    assert!(String::from_utf8(unavailable.stderr)?.contains("nunc serve --search-socket"));
    let mut server = Server::start(&fixture, &["--search-socket"])?;
    let socket = fixture.local().join("search-runtime/search.sock");
    assert_eq!(fs::metadata(&socket)?.permissions().mode() & 0o777, 0o600);
    for format in [
        &["--json", "--explain"][..],
        &["--ids"],
        &["--format", "markdown", "--explain"],
        &["--context", "parent", "--max-tokens", "800"],
    ] {
        let mut args = vec!["search", "--mode", "keyword", "WriterLock"];
        args.extend_from_slice(format);
        let ordinary = fixture.run(&args)?;
        args.push("--server");
        let remote = fixture.run(&args)?;
        assert_eq!(ordinary.stdout, remote.stdout);
        assert_eq!(ordinary.stderr, remote.stderr);
    }
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(
            fixture
                .nunc(&[
                    "search",
                    "--server",
                    "--mode",
                    "keyword",
                    "WriterLock",
                    "--json",
                ])
                .stdout(Stdio::piped())
                .spawn()?,
        );
    }
    fixture.run(&["write", "WriterLock also protects the new checkpoint"])?;
    eventually(|| Ok(fixture.entries()?.len() == 2))?;
    fixture.run(&["reindex", "--keyword-only", "--full"])?;
    for client in clients {
        let output: Value = serde_json::from_slice(&checked(client.wait_with_output()?)?.stdout)?;
        assert_eq!(output["mode_used"], "keyword");
    }
    fixture.run(&["archive", &note])?;
    let search = |extra: &[&str]| {
        let mut args = vec![
            "search",
            "--server",
            "--mode",
            "keyword",
            "WriterLock",
            "--json",
        ];
        args.extend_from_slice(extra);
        fixture.json(&args)
    };
    assert_eq!(search(&[])?["results"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        search(&["--raw"])?["results"].as_array().map(Vec::len),
        Some(2)
    );
    fixture.run(&["forget", &note, "--yes"])?;
    assert_eq!(
        search(&["--raw"])?["results"].as_array().map(Vec::len),
        Some(1)
    );
    server.stop()?;
    assert!(!socket.exists());
    Ok(())
}

#[test]
fn socket_rejects_wrong_commands_versions_and_frames_and_recovers_after_restart() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.log_id("WriterLock guards commit publication")?;
    fixture.run(&["reindex", "--keyword-only"])?;
    let mut server = Server::start(&fixture, &["--search-socket"])?;
    let socket = fixture.local().join("search-runtime/search.sock");
    let check = |payload: Option<Value>, expected: &str| -> Result<()> {
        let mut stream = UnixStream::connect(&socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut status = [0];
        stream.read_exact(&mut status)?;
        assert_eq!(status, [2]);
        if let Some(payload) = payload {
            let payload = serde_json::to_vec(&payload)?;
            stream.write_all(&u32::try_from(payload.len())?.to_be_bytes())?;
            stream.write_all(&payload)?;
        } else {
            stream.write_all(&u32::MAX.to_be_bytes())?;
        }
        stream.read_exact(&mut status)?;
        assert_eq!(status, [1]);
        let mut length = [0; 4];
        stream.read_exact(&mut length)?;
        let mut body = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut body)?;
        assert!(String::from_utf8(body)?.contains(expected));
        Ok(())
    };
    check(None, "size limit")?;
    check(
        Some(serde_json::json!({"version": 2, "args": []})),
        "protocol version",
    )?;
    let args = ["nunc", "log", "unexpected"].map(str::as_bytes);
    check(
        Some(serde_json::json!({"version": 1, "args": args})),
        "search invocations",
    )?;
    assert_eq!(fixture.entries()?.len(), 1);
    let stalled = UnixStream::connect(&socket)?;
    server.stop()?;
    drop(stalled);
    let mut server = Server::start(&fixture, &["--search-socket"])?;
    server.0.kill()?;
    server.0.wait()?;
    assert!(socket.exists());
    let mut restarted = Server::start(&fixture, &["--search-socket"])?;
    let args = [
        "search",
        "--server",
        "--mode",
        "keyword",
        "WriterLock",
        "--json",
    ];
    eventually(|| Ok(fixture.nunc(&args).output()?.status.success()))?;
    restarted.stop()?;
    assert!(!socket.exists());
    fs::write(&socket, "unrelated file")?;
    let rejected = fixture.nunc(&["serve", "--search-socket"]).output()?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8(rejected.stderr)?.contains("must be a Unix socket"));
    assert_eq!(fs::read_to_string(&socket)?, "unrelated file");
    Ok(())
}

#[test]
fn search_socket_bounds_the_waiting_queue() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut server = Server::start(&fixture, &["--search-socket"])?;
    let socket = fixture.local().join("search-runtime/search.sock");
    let mut active = UnixStream::connect(&socket)?;
    active.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut status = [0];
    active.read_exact(&mut status)?;
    assert_eq!(status, [2]);
    let queued = (0..8)
        .map(|_| UnixStream::connect(&socket))
        .collect::<std::io::Result<Vec<_>>>()?;
    let busy = fixture.nunc(&["search", "--server", "query"]).output()?;
    assert!(!busy.status.success());
    assert!(String::from_utf8(busy.stderr)?.contains("server is busy"));
    drop(queued);
    drop(active);
    server.stop()?;
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn socket_search_matches_standalone_with_real_embeddings() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let id = fixture.log_id("WriterLock serializes commit publication in SQLite.")?;
    fixture.run(&["reindex"])?;
    let mut server = Server::start(&fixture, &["--search-socket"])?;
    for mode in ["semantic", "hybrid"] {
        let standalone = fixture.run(&["search", "--mode", mode, "WriterLock", "--json"])?;
        for _ in 0..2 {
            let remote =
                fixture.run(&["search", "--server", "--mode", mode, "WriterLock", "--json"])?;
            assert_eq!(remote.stdout, standalone.stdout);
            assert_eq!(
                serde_json::from_slice::<Value>(&remote.stdout)?["results"][0]["id"],
                id
            );
        }
    }
    let search = |raw: bool| {
        let mut args = vec!["search", "--server", "--json", "WriterLock"];
        if raw {
            args.push("--raw");
        }
        fixture.json(&args)
    };
    fixture.run(&["archive", &id])?;
    assert_eq!(search(false)?["results"], serde_json::json!([]));
    assert_eq!(search(true)?["results"][0]["id"], id);
    fixture.run(&["forget", &id, "--yes"])?;
    fixture.run(&["reindex", "--full"])?;
    assert_eq!(search(true)?["results"], serde_json::json!([]));
    server.stop()?;
    Ok(())
}

#[test]
fn search_profiling_preserves_stdout_and_requires_opt_in() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.log_id("WriterLock serializes concurrent commits.")?;
    fixture.run(&["reindex", "--keyword-only"])?;
    let command = ["search", "--mode", "keyword", "WriterLock", "--json"];
    let ordinary = checked(
        fixture
            .nunc(&command)
            .env_remove("NUNC_SEARCH_PROFILE")
            .output()?,
    )?;
    let profiled = checked(
        fixture
            .nunc(&command)
            .env("NUNC_SEARCH_PROFILE", "1")
            .output()?,
    )?;
    assert_eq!(ordinary.stdout, profiled.stdout);
    assert!(ordinary.stderr.is_empty());
    let stderr = String::from_utf8(profiled.stderr)?;
    if cfg!(feature = "search-profile") {
        let report: Value = serde_json::from_str(
            stderr
                .strip_prefix("nunc-profile ")
                .context("missing stderr profile")?,
        )?;
        for stage in ["cli", "keyword_scan", "source_filter", "context", "format"] {
            assert!(report[stage].is_u64());
        }
        assert!(report.get("model_load").is_none());
    } else {
        assert!(stderr.is_empty());
    }
    Ok(())
}

#[test]
fn similar_to_rejects_invalid_references_and_text_conflicts_without_inference() -> Result<()> {
    let fixture = Fixture::new()?;
    let conflict = fixture
        .nunc(&["search", "--similar-to", "docs/design.md", "query"])
        .output()?;
    assert_eq!(conflict.status.code(), Some(2));
    assert!(conflict.stdout.is_empty());
    for (reference, expected) in [
        ("../private.md", "repository-relative"),
        ("docs/unknown.md", "outside the registered sources"),
    ] {
        let output = fixture
            .nunc(&["search", "--similar-to", reference])
            .output()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.contains(expected));
    }
    assert!(!fixture.state.exists());
    assert!(!fixture.cache.exists());
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_search_by_example_reuses_indexed_passages_and_filters_distinct_parents() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let body = "# SQLite writer lock\n\nHold a SQLite mutex during writer transactions.\n\n## Publication\n\nSerialize commits to protect durable publication.";
    let reference = fixture.log_id(body)?;
    let exact = fixture.log_id(body)?;
    let archived = fixture.log_id(body)?;
    fixture.run(&["archive", &archived])?;
    let docs = fixture.repo.join("docs");
    fs::create_dir(&docs)?;
    fs::write(docs.join("example.md"), body)?;
    fs::write(
        docs.join("rules.md"),
        "# SQLite transaction ordering\n\nUse a writer mutex for atomic commit.\n\n## Recovery\n\nRelease the SQLite write lock after publication.",
    )?;
    fs::write(
        docs.join("garden.md"),
        "# Garden\n\nWater marigolds daily and add compost.",
    )?;
    fixture.run(&["reindex", "--source", "docs"])?;
    fixture.cache = fixture.state.join("empty-model");
    let args = [
        "search",
        "--similar-to",
        &reference,
        "--limit",
        "10",
        "--context",
        "parent",
        "--json",
    ];
    let report = fixture.json(&args)?;
    assert_eq!(report, fixture.json(&args)?);
    assert_eq!(report["reference"]["id"], reference);
    assert!(
        report["reference"]["passage_count"]
            .as_u64()
            .is_some_and(|n| n > 1)
    );
    let hits = report["results"].as_array().context("missing hits")?;
    let parents: Vec<_> = hits
        .iter()
        .map(|hit| &hit["matched_chunk"]["parent_id"])
        .collect();
    assert_eq!(parents.len(), 4);
    assert!(parents.iter().any(|id| **id == exact));
    assert!(
        parents
            .iter()
            .all(|id| **id != reference && **id != archived)
    );
    assert_eq!(
        parents
            .iter()
            .filter_map(|id| id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        parents.len()
    );
    let position = |path| {
        hits.iter()
            .position(|hit| hit["path"] == path)
            .context("missing source")
    };
    assert!(position("docs/rules.md")? < position("docs/garden.md")?);
    let ids = fixture.run(&[
        "search",
        "--similar-to",
        &reference,
        "--ids",
        "--limit",
        "10",
    ])?;
    assert_eq!(
        String::from_utf8(ids.stdout)?.lines().collect::<Vec<_>>(),
        parents
            .iter()
            .filter_map(|id| id.as_str())
            .collect::<Vec<_>>()
    );

    let file = fixture.json(&[
        "search",
        "--similar-to",
        "./docs/example.md",
        "--source-file",
        "docs/rules.md",
        "--max-tokens",
        "600",
        "--json",
    ])?;
    assert_eq!(file["reference"]["path"], "docs/example.md");
    assert_eq!(file["reference"]["kind"], "source");
    assert_eq!(file["results"][0]["path"], "docs/rules.md");
    assert_eq!(file["results"].as_array().map(Vec::len), Some(1));
    let markdown = fixture.run(&[
        "search",
        "--similar-to",
        "docs/example.md",
        "--source",
        "docs",
        "--format",
        "markdown",
        "--max-tokens",
        "300",
    ])?;
    assert!(markdown.stdout.len() <= 1200);
    let document = String::from_utf8(markdown.stdout)?;
    assert!(document.contains("docs/rules.md"));
    assert!(!document.contains("docs/example.md"));
    let unindexed = fixture.log_id("A newly saved design without a vector.")?;
    fs::write(docs.join("example.md"), "Changed after indexing.")?;
    for value in [&unindexed, "docs/example.md"] {
        let output = fixture.nunc(&["search", "--similar-to", value]).output()?;
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)?.contains("no current indexed embeddings"));
    }
    let hidden = fixture
        .nunc(&["search", "--similar-to", &archived])
        .output()?;
    assert!(!hidden.status.success());
    assert!(String::from_utf8(hidden.stderr)?.contains("use --raw"));
    assert_eq!(
        fixture.json(&["search", "--similar-to", &archived, "--raw", "--json"])?["reference"]["id"],
        archived
    );
    fixture.run(&["forget", &reference, "--yes"])?;
    let forgotten = fixture
        .nunc(&["search", "--similar-to", &reference, "--raw"])
        .output()?;
    assert!(!forgotten.status.success());
    assert!(String::from_utf8(forgotten.stderr)?.contains("was forgotten"));
    assert!(!fixture.cache.exists());
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_explain_reports_only_examined_chunks_and_candidate_limits() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let anchor = fixture
        .log_id("# SQLite mutex\n\nSerialize writes.\n\n## Recovery\n\nInspect the journal.")?;
    let target = fixture
        .log_id("# Writer lock\n\nSerialize publication.\n\n## Exit\n\nRelease the mutex.")?;
    let other = fixture.log_id("Testing database write coordination.")?;
    let old = fixture.log_id("An older writer note.")?;
    let archived = fixture.log_id("An archived writer note.")?;
    let forgotten = fixture.log_id("Discarded fixture body that should stay private.")?;
    for id in [&anchor, &target, &other, &archived, &forgotten] {
        fixture.event_time(id, "2026-01-06T00:00:00Z")?;
    }
    fixture.event_time(&old, "2026-01-04T00:00:00Z")?;
    fs::create_dir_all(fixture.repo.join("docs/empty"))?;
    fs::write(
        fixture.repo.join("docs/a.md"),
        "How the mutex protects writes.",
    )?;
    fs::write(
        fixture.repo.join("docs/b.md"),
        "How tests validate the reader.",
    )?;
    fixture.run(&["archive", &archived])?;
    fixture.run(&["reindex", "--source", "docs"])?;
    let index = fs::read_dir(&fixture.state)?
        .next()
        .context("missing state")??
        .path()
        .join("index.sqlite3");
    let stale = fs::read(&index)?;
    fixture.run(&["forget", &forgotten, "--yes"])?;
    // Restore only the disposable pre-forget index in this isolated fixture.
    fs::write(index, stale)?;
    let flags = [
        "search",
        "--similar-to",
        &anchor,
        "--type",
        "memory",
        "--from",
        "2026-01-05T00:00:00Z",
        "--limit",
        "1",
        "--json",
    ];
    let plain = fixture.json(&flags)?;
    assert!(plain.get("pipeline").is_none());
    let explained = fixture.json(&[&flags[..], &["--explain"]].concat())?;
    let pipeline = &explained["pipeline"];
    assert_eq!(pipeline["mode"], "semantic");
    assert_eq!(pipeline["exact"], true);
    assert_eq!(pipeline["index_freshness"], "stale");
    assert_eq!(pipeline["chunks_after_type_scope"], 8);
    assert_eq!(pipeline["excluded_reference_chunks"], 2);
    assert_eq!(pipeline["excluded_archived_or_undone_chunks"], 1);
    assert_eq!(pipeline["excluded_forgotten_chunks"], 1);
    assert_eq!(pipeline["excluded_temporal_chunks"], 1);
    assert_eq!(pipeline["scored_chunks"], 3);
    assert_eq!(pipeline["deduplicated_chunks"], 1);
    assert_eq!(pipeline["ranking_unit"], "parent");
    assert_eq!(pipeline["ranked"], 2);
    assert_eq!(pipeline["beyond_limit"], 1);
    assert_eq!(pipeline["returned"], 1);
    let explained = serde_json::to_string(&explained)?;
    assert!(!explained.contains(&forgotten));
    assert!(!explained.contains("Discarded fixture body"));
    let budget = fixture.json(&[&flags[..], &["--explain", "--max-tokens", "1"]].concat())?;
    assert_eq!(budget["pipeline"]["omitted_by_budget"], 1);
    assert_eq!(budget["pipeline"]["returned"], 0);

    for (filter, path, passed) in [
        ("--source-file", "docs/a.md", 1),
        ("--source", "docs/empty", 0),
    ] {
        let report = fixture.json(&[
            "search",
            "--similar-to",
            &anchor,
            filter,
            path,
            "--json",
            "--explain",
        ])?;
        assert_eq!(report["pipeline"]["chunks_after_type_scope"], 2);
        assert_eq!(report["pipeline"]["chunks_after_source_filter"], passed);
        assert_eq!(report["pipeline"]["scored_chunks"], passed);
        assert_eq!(report["pipeline"]["returned"], passed);
    }
    let markdown = fixture.run(&[
        "search",
        "--similar-to",
        &anchor,
        "--source",
        "docs",
        "--format",
        "markdown",
        "--explain",
    ])?;
    assert!(String::from_utf8(markdown.stdout)?.starts_with("## Result 1"));
    assert!(String::from_utf8(markdown.stderr)?.contains("Exact chunk counts"));
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_topics_group_distinct_workstreams_using_only_stored_vectors() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let note = |body, min| -> Result<String> {
        let id = fixture.log_id(body)?;
        fixture.event_time(&id, &format!("2026-02-02T12:{min:02}:00Z"))?;
        Ok(id)
    };
    let sql = [
        note(
            "# SQLite writer serialization\n\nUse a mutex to serialize SQLite transactions and lock the writer during database publication.",
            0,
        )?,
        note(
            "# SQLite write transactions\n\nLock the SQLite writer with a mutex to serialize database transactions and publication.",
            5,
        )?,
    ];
    let tls = [
        note(
            "# Renewal of TLS certificates\n\nRotate expired TLS certificates and refresh the issuer chain after certificate renewal.",
            10,
        )?,
        note(
            "# TLS certificate renewal\n\nRenew expired TLS certificates and update the certificate issuer chain.",
            15,
        )?,
    ];
    let anchor = note("Document the retry protocol for a Unix FIFO reader.", 20)?;
    fixture.run(&["reindex"])?;
    let stale = note("A new note without vectors yet.", 25)?;
    // Grouping can still run with the installed model unavailable after indexing.
    fixture.cache = fixture.state.join("absent-model");
    let args = [
        "topics",
        "--around",
        &anchor,
        "--window",
        "30m",
        "--threshold",
        "0.88",
        "--json",
    ];
    let result = fixture.json(&args)?;
    assert_eq!(result, fixture.json(&args)?);
    assert_eq!(result["scope"]["anchor"]["id"], anchor);
    let groups = result["topics"].as_array().context("missing groups")?;
    assert_eq!(groups.len(), 2, "{result}");
    for (group, expected) in groups.iter().zip([sql, tls]) {
        let ids: Vec<_> = group["members"]
            .as_array()
            .context("missing members")?
            .iter()
            .filter_map(|event| event["id"].as_str())
            .collect();
        assert_eq!(ids, expected);
        assert!(
            group["members"]
                .as_array()
                .context("missing members")?
                .iter()
                .any(|event| event["title"] == group["label"]
                    && event["id"] == group["representative_id"])
        );
        assert!(
            group["minimum_similarity"]
                .as_f64()
                .is_some_and(|score| score >= 0.88)
        );
    }
    assert_eq!(result["ungrouped"][0]["id"], stale);
    assert_eq!(result["ungrouped"][0]["reason"], "missing-embedding");
    assert!(!fixture.cache.exists());
    Ok(())
}

#[test]
fn duplicate_check_works_without_cached_embeddings() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.log_id("Writer lock controls interleaving.")?;
    let second = fixture.log_id("Writer  lock\ncontrols interleaving. ")?;
    fixture.log_id("Writer lock changes owner after restart.")?;
    let result = fixture.json(&["duplicates", &first, "--json"])?;
    assert_eq!(result["matches"].as_array().map(Vec::len), Some(1));
    let found = &result["matches"][0];
    assert_eq!(found["detection"], "exact");
    assert_eq!(found["scope"], "entry");
    assert!(
        ["left", "right"]
            .iter()
            .any(|side| found[*side]["id"] == second)
    );
    assert!(
        !result["warnings"]
            .as_array()
            .context("missing duplicate warnings")?
            .is_empty()
    );
    let written = fixture.run(&[
        "write",
        "--check-duplicates",
        "Writer lock controls interleaving.",
    ])?;
    assert!(String::from_utf8(written.stderr)?.contains("exact entry"));
    assert_eq!(fixture.entries()?.len(), 4);
    Ok(())
}

#[test]
fn duplicate_groups_export_only_verified_active_non_lineage_ids() -> Result<()> {
    let fixture = Fixture::new()?;
    let body = "Approval window is sixty seconds.";
    let keep = fixture.log_id(body)?;
    let candidate = fixture.log_id(body)?;
    let lineage = fixture.log_id(body)?;
    let archived = fixture.log_id(body)?;
    let forgotten = fixture.log_id(body)?;
    let whitespace_variant = fixture.log_id("Approval window is\nsixty seconds.")?;
    let entries = fixture.local().join("entries");
    for (day, id) in [
        &keep,
        &candidate,
        &lineage,
        &archived,
        &forgotten,
        &whitespace_variant,
    ]
    .iter()
    .enumerate()
    {
        let path = entries.join(format!("{id}.md"));
        let content = fs::read_to_string(&path)?;
        let (header, markdown) = content
            .strip_prefix("---\n")
            .context("front matter")?
            .split_once("\n---\n")
            .context("metadata terminator")?;
        let mut metadata: Value = serde_json::from_str(header)?;
        metadata["timestamp"] = format!("2026-01-0{}T12:00:00+00:00", day + 1).into();
        fs::write(&path, format!("---\n{metadata}\n---\n{markdown}"))?;
    }
    let sibling = fixture.log_id("Migration decisions for middleware.")?;
    let summary = fixture.summary(
        &format!("{lineage},{sibling}"),
        "External summary about migration.",
    )?;
    fixture.run(&["consolidate", "undo", &summary])?;
    fixture.run(&["archive", &archived])?;
    fixture.run(&["forget", &forgotten, "--yes"])?;

    let report = fixture.json(&["duplicates", "--exact", "--json"])?;
    let group = &report["groups"][0];
    assert_eq!(group["reference_id"], keep);
    assert_eq!(
        group["suggested_forget_ids"],
        serde_json::json!([candidate])
    );
    assert_eq!(group["members"].as_array().map(Vec::len), Some(3));
    assert_eq!(group["members"][0]["id"], candidate);
    assert_eq!(group["members"][0]["byte_identical"], true);
    assert_eq!(group["members"][1]["id"], lineage);
    assert_eq!(group["members"][2]["byte_identical"], false);
    let human = String::from_utf8(fixture.run(&["duplicates", "--exact"])?.stdout)?;
    assert!(human.contains(&format!("KEEP {keep}")));
    assert!(human.contains(&format!("to {keep}) {candidate}")));
    let raw = fixture.json(&["duplicates", "--exact", "--raw", "--json"])?;
    let members = raw["groups"][0]["members"]
        .as_array()
        .context("missing members")?;
    assert!(members.iter().any(|member| member["id"] == archived));
    assert!(members.iter().all(|member| member["id"] != forgotten));
    assert_eq!(raw["suggested_forget_ids"], serde_json::json!([candidate]));
    let exported = fixture.run(&["duplicates", "--exact", "--forget-ids"])?;
    assert_eq!(exported.stdout, format!("{candidate}\n").as_bytes());
    assert!(exported.stderr.is_empty());
    let preview = fixture.json(&["forget", &candidate, "--dry-run", "--json"])?;
    assert_eq!(preview["targets"], serde_json::json!([candidate]));
    assert!(entries.join(format!("{candidate}.md")).is_file());
    for flags in [
        &["duplicates", "--forget-ids"][..],
        &["duplicates", "--exact", "--raw", "--forget-ids"][..],
    ] {
        let rejected = fixture.nunc(flags).output()?;
        assert_eq!(rejected.status.code(), Some(2));
        assert!(rejected.stdout.is_empty());
    }
    Ok(())
}

#[test]
fn git_history_and_hook_share_idempotency_and_respect_existing_hooks() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.commit("data", "Serialize writers")?;
    let second = fixture.commit("new data", "Keep model assets local")?;
    let dry = fixture.run(&["git", "import", "--all", "--limit", "1", "--dry-run"])?;
    assert!(String::from_utf8(dry.stdout)?.contains(&format!("{second} would import")));
    assert!(!fixture.local().exists());
    assert!(
        fixture
            .nunc(&[
                "git",
                "import",
                "--all",
                "--range",
                &format!("{first}..{second}")
            ])
            .output()?
            .status
            .code()
            == Some(2)
    );
    fixture.run(&["git", "import", "--range", &format!("{first}..{second}")])?;
    assert_eq!(fixture.entries()?.len(), 1);
    fixture.run(&["git", "import", "--all"])?;
    fixture.run(&["git", "sync"])?;
    fixture.run(&["git", "ingest", &first])?;
    fixture.run(&["git", "import", "--since", "2999-01-01"])?;
    assert_eq!(fixture.entries()?.len(), 2);
    let metadata = fixture
        .entries()?
        .into_iter()
        .map(|entry| {
            let metadata = entry
                .strip_prefix("---\n")
                .and_then(|s| s.split_once("\n---\n"))
                .context("missing front matter")?
                .0;
            Ok(serde_json::from_str::<Value>(metadata)?)
        })
        .collect::<Result<Vec<_>>>()?;
    assert!(metadata.iter().any(|entry| entry["kind"] == "git-commit"
        && entry["commit"]["sha"] == second
        && entry["commit"]["paths"][0] == "note.txt"
        && entry["commit"]["committed_at"] == entry["timestamp"]));

    fs::create_dir(fixture.repo.join("hooks"))?;
    fixture.git(&["config", "core.hooksPath", "hooks"])?;
    let hook = fixture.repo.join("hooks/post-commit");
    fs::write(&hook, "#!/bin/sh\nexit 0\n")?;
    assert!(
        !fixture
            .nunc(&["git", "hook", "install"])
            .output()?
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&hook)?, "#!/bin/sh\nexit 0\n");
    fs::remove_file(&hook)?;
    fixture.run(&["git", "hook", "install"])?;
    assert!(
        String::from_utf8(fixture.run(&["git", "hook", "status"])?.stdout)?.contains("installed")
    );
    fixture.commit("third", "Move capture off the embedding path")?;
    fixture.git(&["commit", "--amend", "-qm", "Amended capture design"])?;
    assert_eq!(fixture.entries()?.len(), 4);
    fixture.run(&["git", "sync"])?;
    assert_eq!(fixture.entries()?.len(), 4);
    fixture.run(&["git", "hook", "uninstall"])?;
    assert!(!hook.exists());
    Ok(())
}

#[test]
fn consolidation_validates_revisions_handles_all_inputs_and_forgets_only_targets() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.log_id("Original FIFO lock design")?;
    let second = fixture.log_id("A concurrent writer test")?;
    let ids = format!("{original},{second}");
    let input_file = fixture.repo.join("summary.md");
    fs::write(&input_file, "File summary\r\n\nPreserve all input.")?;
    let file_name = input_file.to_str().context("invalid fixture filename")?;
    let invalid = fixture
        .nunc(&[
            "consolidate",
            "apply",
            "--entries",
            &ids,
            "--summary",
            "s",
            "--summary-stdin",
        ])
        .output()?;
    assert_eq!(invalid.status.code(), Some(2));
    let stale = fixture
        .nunc(&[
            "consolidate",
            "apply",
            "--entries",
            &ids,
            "--summary",
            "s",
            "--revision",
            &format!("{original}=stale"),
        ])
        .output()?;
    assert!(!stale.status.success());
    assert_eq!(fixture.entries()?.len(), 2);
    let first = fixture.json(&[
        "consolidate",
        "apply",
        "--entries",
        &ids,
        "--summary-from-file",
        file_name,
        "--json",
    ])?;
    let first_id = first["summary_id"]
        .as_str()
        .context("missing file summary ID")?;
    assert_eq!(first["status"], "applied");
    assert!(
        fixture
            .entry_bytes(first_id)?
            .ends_with(b"File summary\r\n\nPreserve all input.")
    );
    let retry = fixture.json(&[
        "consolidate",
        "apply",
        "--entries",
        &ids,
        "--summary-from-file",
        file_name,
        "--json",
    ])?;
    assert_eq!(retry["summary_id"], first_id);
    assert_eq!(retry["status"], "already-applied");

    let mut child = fixture
        .nunc(&[
            "consolidate",
            "apply",
            "--entries",
            first_id,
            "--summary-stdin",
            "--json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("missing stdin")?
        .write_all(b"Stdin summary")?;
    let nested: Value = serde_json::from_slice(&checked(child.wait_with_output()?)?.stdout)?;
    let nested_id = nested["summary_id"]
        .as_str()
        .context("missing nested summary ID")?;
    assert!(
        !fixture
            .nunc(&["consolidate", "undo", first_id])
            .output()?
            .status
            .success()
    );

    let before = fixture.entry_bytes(nested_id)?;
    let dry = fixture.json(&["forget", &original, "--dry-run", "--json"])?;
    let dependencies = dry["dependencies"]
        .as_array()
        .context("missing dependencies")?;
    assert!(
        dependencies
            .iter()
            .any(|dep| dep["summary_id"] == first_id && dep["direct"] == true)
    );
    assert!(
        dependencies
            .iter()
            .any(|dep| dep["summary_id"] == nested_id && dep["direct"] == false)
    );
    assert_eq!(fixture.entries()?.len(), 4);
    assert!(
        !fixture
            .nunc(&["forget", &original])
            .stdin(Stdio::null())
            .output()?
            .status
            .success()
    );
    let outcome = fixture.json(&["forget", &original, "--yes", "--json"])?;
    assert_eq!(outcome["targets"][0], original);
    assert_eq!(fixture.entry_bytes(nested_id)?, before);
    assert!(fixture.entry_bytes(&original).is_err());
    assert!(fixture.entry_bytes(&second).is_ok());
    assert_eq!(
        fixture.json(&["forget", &original, "--yes", "--json"])?["already_forgotten"][0],
        original
    );
    assert!(
        !fixture
            .nunc(&["forget", "00000000-0000-0000-0000-000000000010", "--yes"])
            .output()?
            .status
            .success()
    );
    fixture.run(&["consolidate", "undo", nested_id])?;
    fixture.run(&["consolidate", "undo", first_id])?;
    let inline = fixture.summary(&second, "Replacement inline summary")?;
    assert!(
        fixture
            .entry_bytes(&inline)?
            .ends_with(b"Replacement inline summary")
    );
    let outcome = fixture.json(&["forget", first_id, &inline, "--yes", "--json"])?;
    assert_eq!(outcome["targets"].as_array().map(Vec::len), Some(2));
    assert!(fixture.entry_bytes(first_id).is_err());
    assert!(fixture.entry_bytes(&inline).is_err());
    assert!(fixture.entry_bytes(nested_id).is_ok());
    assert!(fixture.entry_bytes(&second).is_ok());
    Ok(())
}

#[test]
fn forgotten_git_commit_cannot_be_restored_by_sync_import_or_hook_command() -> Result<()> {
    let fixture = Fixture::new()?;
    let _sha = fixture.commit("fixture", "Never reuse a removed lease record")?;
    fixture.run(&["git", "import", "--all"])?;
    let entry = fs::read_dir(fixture.local().join("entries"))?
        .next()
        .context("missing Git note")??
        .path();
    let id = entry
        .file_stem()
        .and_then(|name| name.to_str())
        .context("missing ID")?;
    fixture.run(&["forget", id, "--yes"])?;
    for args in [
        &["git", "import", "--all"][..],
        &["git", "sync"][..],
        &["git", "ingest", "HEAD"][..],
    ] {
        let result = fixture.run(args)?;
        assert!(String::from_utf8(result.stdout)?.contains("1 forgotten"));
    }
    fixture.run(&["git", "hook", "install"])?;
    let hook = Command::new(fixture.repo.join(".git/hooks/post-commit"))
        .current_dir(&fixture.repo)
        .env("NUNC_STATE_DIR", &fixture.state)
        .env("NUNC_MODEL_CACHE_DIR", &fixture.cache)
        .output()?;
    assert!(hook.status.success());
    assert!(String::from_utf8(hook.stderr)?.contains("1 forgotten"));
    assert!(fixture.entries()?.is_empty());
    assert!(!fs::read_to_string(fixture.local().join("ledger.json"))?.contains("Never reuse"));
    Ok(())
}

#[test]
fn overlapping_consolidations_serialize() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.log_id("A shared candidate")?;
    let spawn = |text: &str| -> Result<Child> {
        Ok(fixture
            .nunc(&[
                "consolidate",
                "apply",
                "--entries",
                &id,
                "--summary",
                text,
                "--json",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    };
    let a = spawn("First concurrent summary")?;
    let b = spawn("Second concurrent summary")?;
    let (a, b) = (a.wait_with_output()?, b.wait_with_output()?);
    assert_ne!(a.status.success(), b.status.success());
    assert_eq!(fixture.entries()?.len(), 2);
    assert!(fixture.entry_bytes(&id).is_ok());
    let failed = if a.status.success() { b } else { a };
    assert!(String::from_utf8(failed.stderr)?.contains("already archived"));
    Ok(())
}

#[test]
fn git_merge_reports_changes_relative_to_first_parent() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.commit("base", "Base commit")?;
    let main = String::from_utf8(fixture.git(&["branch", "--show-current"])?.stdout)?;
    fixture.git(&["checkout", "-qb", "side"])?;
    fs::write(fixture.repo.join("side.md"), "Added on side")?;
    fixture.git(&["add", "side.md"])?;
    fixture.git(&["commit", "-qm", "Side commit"])?;
    fixture.git(&["checkout", "-q", main.trim()])?;
    fixture.commit("main updated", "Main change")?;
    fixture.git(&["merge", "--no-ff", "side", "-qm", "Merge decisions"])?;
    fixture.run(&["git", "import", "--all"])?;
    let records = fixture.entries()?;
    assert_eq!(records.len(), 4);
    let merge = records
        .iter()
        .find(|entry| entry.contains("Merge decisions"))
        .context("missing merge memory")?;
    let metadata: Value = serde_json::from_str(
        merge
            .split_once("\n---\n")
            .context("missing merge metadata")?
            .0
            .trim_start_matches("---\n"),
    )?;
    assert_eq!(metadata["commit"]["paths"], serde_json::json!(["side.md"]));
    Ok(())
}

#[test]
fn context_budget_rejects_zero_and_ids_mode_before_loading_the_model() -> Result<()> {
    let fixture = Fixture::new()?;
    for args in [
        &["search", "writer", "--max-tokens", "0"][..],
        &["search", "writer", "--max-tokens", "100", "--ids"][..],
    ] {
        let output = fixture.nunc(args).output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

#[test]
fn markdown_bundle_rejects_conflicting_output_flags() -> Result<()> {
    let fixture = Fixture::new()?;
    for option in ["--json", "--ids"] {
        let output = fixture
            .nunc(&["search", "lease", "--format", "markdown", option])
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_markdown_bundle_preserves_fences_budgets_and_visibility() -> Result<()> {
    use pulldown_cmark::{Event, Parser, Tag};
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let body = "# Café lease\n\nA writer lease protects publication.\n\n## Recovery\n\n```rust\nreopenLease();\n```\n\n~~~text\nOnly the current lease holder may write.\n~~~\n";
    let visible = fixture.log_id(body)?;
    let archived = fixture.log_id("An archived decision on writer lease recovery.")?;
    let forgotten = fixture.log_id("A forgotten note about writer lease recovery.")?;
    fixture.run(&["archive", &archived])?;
    fixture.run(&["reindex"])?;
    fixture.run(&["forget", &forgotten, "--yes"])?;
    let args = [
        "search",
        "writer lease publication",
        "--format",
        "markdown",
        "--context",
        "parent",
        "--limit",
        "10",
    ];
    let stale = fixture.run(&args)?;
    assert!(String::from_utf8(stale.stderr)?.contains("Index needs refresh"));
    fixture.run(&["reindex"])?;
    let result = fixture.run(&args)?;
    assert!(result.stderr.is_empty());
    assert_eq!(result.stdout, stale.stdout);
    let stdout = String::from_utf8(result.stdout)?;
    assert!(stdout.starts_with("## Result 1\n"));
    assert!(stdout.contains(body));
    assert_eq!(stdout.matches(&visible).count(), 2); // Entry filename and parent ID.
    assert!(!stdout.contains(&archived));
    assert!(!stdout.contains(&forgotten));
    let events: Vec<_> = Parser::new(&stdout).collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Start(Tag::Heading { .. })))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Start(Tag::CodeBlock(_))))
            .count(),
        1
    );
    let raw = fixture.run(&[&args[..], &["--raw", "--explain", "--max-tokens", "500"]].concat())?;
    assert!(String::from_utf8(raw.stderr)?.contains("context budget used"));
    assert!(raw.stdout.len() <= 500 * 4);
    let raw = String::from_utf8(raw.stdout)?;
    assert!(raw.contains(&visible));
    assert!(raw.contains(&archived));
    assert!(raw.contains("Status ` archived `"));
    assert!(!raw.contains(&forgotten));
    assert!(raw.contains("Returned lines"));
    assert!(raw.contains("Model revision"));
    let tiny = fixture.run(&[&args[..], &["--max-tokens", "1"]].concat())?;
    assert!(tiny.stdout.is_empty());
    assert!(String::from_utf8(tiny.stderr)?.contains("1 omitted"));
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_search_budget_keeps_distinct_attributed_context() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let docs = fixture.repo.join("docs");
    fs::create_dir(&docs)?;
    let long = format!(
        "# Runtime decisions\n\n## Café locking\n\nThe SQLite writer mutex guards durable publication ⛄.\n\n{}",
        "A separate lead tells the reader to review older operational notes.\n\n".repeat(100)
    );
    let brief = "# Transactions\n\nThe SQLite writer mutex coordinates process access.";
    fs::write(docs.join("long.md"), &long)?;
    fs::write(docs.join("brief.md"), brief)?;
    let hidden = fixture.log_id("Archived SQLite mutex publication note.")?;
    fixture.run(&["archive", &hidden])?;
    fixture.run(&["reindex", "--source", "docs"])?;
    let args = [
        "search",
        "SQLite writer mutex guards publication",
        "--source",
        "docs",
        "--context",
        "parent",
        "--max-tokens",
        "900",
        "--explain",
        "--limit",
        "2",
        "--json",
    ];
    let report = fixture.json(&args)?;
    assert_eq!(report, fixture.json(&args)?);
    let hits = report["results"]
        .as_array()
        .context("missing budgeted hits")?;
    assert_eq!(hits.len(), 2);
    let mut found_long = false;
    for hit in hits {
        let is_long = hit["path"] == "docs/long.md";
        let parent = if is_long { &long } else { brief };
        let window = &hit["context_window"];
        let number = |key: &str| -> Result<usize> {
            Ok(window[key]
                .as_u64()
                .with_context(|| format!("missing {key}"))?
                .try_into()?)
        };
        let start = number("byte_start")?;
        let end = number("byte_end")?;
        assert_eq!(hit["text"], &parent[start..end]);
        assert_eq!(
            number("line_start")?,
            parent[..start]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1
        );
        assert!(hit["matched_chunk"]["id"].is_string());
        assert!(hit["explanation"]["similarity"].is_number());
        if is_long {
            found_long = true;
            assert_eq!(window["truncated"], true);
            assert_eq!(
                hit["matched_chunk"]["heading_path"],
                serde_json::json!(["Runtime decisions", "Café locking"])
            );
            assert!(
                hit["text"]
                    .as_str()
                    .context("missing context")?
                    .contains("durable publication ⛄")
            );
        }
    }
    assert!(found_long);
    assert_eq!(report["budget"]["max_tokens"], 900);
    assert_eq!(report["budget"]["omitted_candidates"], 0);
    assert!(
        report["budget"]["estimated_tokens"]
            .as_u64()
            .is_some_and(|used| used <= 900)
    );
    let tiny = fixture.json(&[
        "search",
        "SQLite writer",
        "--source",
        "docs",
        "--max-tokens",
        "1",
        "--json",
    ])?;
    assert_eq!(tiny["results"], serde_json::json!([]));
    assert_eq!(tiny["budget"]["omitted_candidates"], 2);
    let raw = fixture.json(&[
        "search",
        "SQLite mutex",
        "--type",
        "memory",
        "--raw",
        "--max-tokens",
        "1000",
        "--json",
    ])?;
    assert_eq!(raw["results"][0]["id"], hidden);
    let ordinary = fixture.json(&[
        "search",
        "SQLite mutex",
        "--type",
        "memory",
        "--max-tokens",
        "1000",
        "--json",
    ])?;
    assert_eq!(ordinary["results"], serde_json::json!([]));
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn real_context_explanations_duplicates_and_lifecycle() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let long = format!(
        "# Architecture\n\n{}\n\n## Writer serialization\n\nCooperative FIFO writers acquire an advisory interprocess lock before sending a frame.\n\n```sh\nnunc write \"hello\"\n```\n",
        "Garden soil can retain moisture. ".repeat(180)
    );
    let parent = fixture.log_id(&long)?;
    let paraphrase = fixture
        .log_id("Senders serialize writes to the named pipe using an advisory process lock.")?;
    let distinct = fixture.log_id(
        "Revised design: the capture process accepts only one sender. There is no writer lock.",
    )?;
    fixture.run(&["reindex"])?;
    let query = "How do different processes take turns sending messages through the pipe?";
    let hits = fixture.json(&[
        "search",
        "--context",
        "section",
        "--recency-boost=0.05",
        "--explain",
        "--json",
        "--limit",
        "10",
        query,
    ])?;
    let hit = hits["results"]
        .as_array()
        .context("no search results")?
        .iter()
        .find(|v| {
            v["id"] == parent && v["matched_chunk"]["heading_path"][1] == "Writer serialization"
        })
        .context("missing long entry chunk")?;
    assert!(
        hit["matched_chunk"]["line_start"]
            .as_u64()
            .is_some_and(|line| line > 3)
    );
    assert!(
        hit["text"]
            .as_str()
            .is_some_and(|text| text.contains("nunc write") && !text.contains("Garden"))
    );
    let explanation = &hit["explanation"];
    let number = |name: &str| {
        explanation[name]
            .as_f64()
            .with_context(|| format!("missing {name}"))
    };
    assert!(
        (number("similarity")? + number("recency_contribution")? - number("final_score")?).abs()
            < 1e-12
    );
    assert!((0.0..=0.05).contains(&number("recency_contribution")?));
    let legacy = fixture.json(&[
        "search", "--json", "--sort", "recent", "--limit", "1", query,
    ])?;
    assert_eq!(legacy["results"][0]["id"], distinct);

    let duplicates = fixture.json(&["duplicates", &parent, "--threshold", "0.86", "--json"])?;
    let pairs = duplicates["matches"]
        .as_array()
        .context("no duplicate list")?;
    assert!(pairs.iter().any(|pair| pair["detection"] == "semantic"
        && pair["scope"] == "chunk"
        && (pair["left"]["id"] == paraphrase || pair["right"]["id"] == paraphrase)));
    let group = duplicates["groups"]
        .as_array()
        .context("missing duplicate groups")?
        .iter()
        .find(|group| group["detection"] == "semantic")
        .context("missing semantic group")?;
    assert_eq!(group["reference_id"], parent);
    assert_eq!(group["suggested_forget_ids"], serde_json::json!([]));
    assert!(
        group["members"]
            .as_array()
            .context("missing group members")?
            .iter()
            .any(|member| member["id"] == paraphrase
                && member["scope"] == "chunk"
                && member["reference_chunk"]["parent_id"] == parent
                && member["chunk"]["parent_id"] == paraphrase)
    );
    // Similarity is heuristic. A more restrictive cutoff must exclude the distinct revision.
    let strict = fixture.json(&["duplicates", &parent, "--threshold", "0.96", "--json"])?;
    assert!(
        strict["matches"]
            .as_array()
            .context("no strict list")?
            .iter()
            .all(|pair| pair["left"]["id"] != distinct && pair["right"]["id"] != distinct)
    );

    let candidates = fixture.json(&[
        "consolidate",
        "candidates",
        query,
        "--limit",
        "10",
        "--json",
    ])?;
    let selected = candidates["results"]
        .as_array()
        .context("no candidates")?
        .iter()
        .find(|v| v["id"] == parent)
        .context("missing full candidate")?;
    assert_eq!(selected["text"], long);
    let revision = selected["matched_chunk"]["parent_hash"]
        .as_str()
        .context("no hash")?;
    let summary = fixture.json(&[
        "consolidate",
        "apply",
        "--entries",
        &format!("{parent},{paraphrase}"),
        "--summary",
        "Advisory locks serialize FIFO writers across processes.",
        "--revision",
        &format!("{parent}={revision}"),
        "--json",
    ])?;
    let id = summary["summary_id"].as_str().context("no summary ID")?;
    let results = |raw: bool| -> Result<Vec<Value>> {
        let mut args = vec!["search", "--json", "--limit", "100"];
        if raw {
            args.push("--raw");
        }
        args.push(query);
        Ok(fixture.json(&args)?["results"]
            .as_array()
            .context("no results list")?
            .clone())
    };
    assert!(
        results(false)?
            .iter()
            .all(|hit| hit["id"] != parent && hit["id"] != paraphrase)
    );
    assert!(results(true)?.iter().any(|hit| hit["id"] == parent
        && hit["status"] == "archived"
        && hit["archived_into"] == id));
    fixture.run(&["reindex", "--full"])?;
    assert!(
        results(false)?
            .iter()
            .any(|hit| hit["id"] == id && hit["status"] == "consolidated")
    );
    let bytes = fixture.entry_bytes(id)?;
    fixture.run(&["forget", &paraphrase, "--yes"])?;
    fixture.run(&["reindex", "--full"])?;
    let raw = results(true)?;
    assert!(raw.iter().all(|hit| hit["id"] != paraphrase));
    assert!(raw.iter().any(|hit| hit["id"] == id
        && hit["sources"].as_array().is_some_and(|refs| {
            refs.iter()
                .any(|r| r["id"] == paraphrase && r["forgotten"] == true)
        })));
    assert_eq!(fixture.entry_bytes(id)?, bytes);
    fixture.run(&["consolidate", "undo", id])?;
    assert!(results(false)?.iter().all(|hit| hit["id"] != id));
    assert!(results(false)?.iter().any(|hit| hit["id"] == parent));
    assert!(
        results(true)?
            .iter()
            .any(|hit| hit["id"] == id && hit["status"] == "undone")
    );

    // Simulate interruption just after the durable forget intent was published.
    let interrupted =
        fixture.log_id("Disposable record left behind after durable forget intent")?;
    fixture.run(&["reindex"])?;
    let ledger = fixture.local().join("ledger.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&ledger)?)?;
    data["forgotten"][&interrupted] = serde_json::json!({});
    fs::write(&ledger, serde_json::to_vec(&data)?)?;
    assert!(fixture.entry_bytes(&interrupted).is_ok());
    assert!(results(true)?.iter().all(|hit| hit["id"] != interrupted));
    fixture.run(&["reindex"])?;
    assert!(fixture.entry_bytes(&interrupted).is_err());
    Ok(())
}

#[test]
fn git_capture_bounds_diff_content_and_redacts_obvious_credentials() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(
        fixture.repo.join(".env"),
        "PRIVATE_EXAMPLE=secret-content\n",
    )?;
    fs::write(
        fixture.repo.join("service.txt"),
        "Normal context line\npassword=unsafe-fixture-value\n",
    )?;
    fixture.git(&["add", ".env", "service.txt"])?;
    fixture.git(&[
        "commit",
        "-qm",
        "Update context",
        "-m",
        "Authorization: unsafe-fixture-token",
    ])?;
    fixture.run(&["init"])?;
    fs::write(
        fixture.local().join("config.toml"),
        "[git]\ndiff_context_bytes = 512\n",
    )?;
    fixture.run(&["git", "ingest", "HEAD"])?;
    let notes = fixture.entries()?;
    assert_eq!(notes.len(), 1);
    let note = &notes[0];
    assert!(note.contains("Normal context line"));
    assert!(note.contains("[redacted]"));
    assert!(!note.contains(".env"));
    assert!(!note.contains("secret-content"));
    assert!(!note.contains("unsafe-fixture"));
    Ok(())
}

#[test]
fn concurrent_forget_is_idempotent_and_preserves_unrelated_entries() -> Result<()> {
    let fixture = Fixture::new()?;
    let target = fixture.log_id("Explicit concurrent forget target")?;
    let keep = fixture.log_id("Unrelated note")?;
    let spawn = || -> Result<Child> {
        Ok(fixture
            .nunc(&["forget", &target, "--yes", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    };
    let children = [spawn()?, spawn()?];
    let mut deleted = 0;
    let mut idempotent = 0;
    for child in children {
        let report: Value = serde_json::from_slice(&checked(child.wait_with_output()?)?.stdout)?;
        deleted += report["targets"]
            .as_array()
            .context("missing forget targets")?
            .len();
        idempotent += report["already_forgotten"]
            .as_array()
            .context("missing idempotency report")?
            .len();
    }
    assert_eq!((deleted, idempotent), (1, 1));
    assert!(fixture.entry_bytes(&target).is_err());
    assert!(fixture.entry_bytes(&keep).is_ok());
    Ok(())
}

#[test]
fn upgrades_default_privacy_ignore_without_overwriting_custom_rules() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["init"])?;
    let ignore = fixture.local().join(".gitignore");
    fs::write(
        &ignore,
        "/entries/\n/input.fifo\n/session.json\n/*.lock\n/.tmp*\n",
    )?;
    fixture.log_id("Upgrade old ignore rules")?;
    assert!(
        fs::read_to_string(&ignore)?
            .lines()
            .any(|line| line == "/ledger.json")
    );
    fs::write(&ignore, "custom-pattern\n")?;
    fixture.log_id("Preserve custom rules")?;
    assert_eq!(fs::read_to_string(&ignore)?, "custom-pattern\n");
    Ok(())
}

#[test]
fn index_health_dry_run_on_fresh_worktree_creates_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let report = fixture.json(&["repair", "--dry-run", "--json"])?;
    assert_eq!(report["index_state"], "missing");
    assert_eq!(report["findings"]["INDEX_MISSING"]["count"], 1);
    assert!(!fixture.local().exists());
    assert!(!fixture.state.exists());
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn audit_and_repair_disposable_index_while_preserving_durable_visibility() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let archived = fixture.log_id("Archived memory on project coordination.")?;
    let forgotten = fixture.log_id("Forgotten memory on project coordination.")?;
    let summary = fixture.summary(&archived, "Summary of project coordination.")?;
    let active = fixture.log_id("Active memory on project coordination.")?;
    fs::write(
        fixture.repo.join("known.md"),
        "# Coordination\n\nLocal sources for the coordination design.",
    )?;
    fixture.run(&["reindex", "--source", "known.md"])?;
    let healthy = fixture.json(&["doctor", "--deep", "--json"])?;
    assert_eq!(healthy["action"], "none");
    assert_eq!(healthy["findings"], serde_json::json!({}));

    let index = fs::read_dir(&fixture.state)?
        .next()
        .context("missing state")??
        .path()
        .join("index.sqlite3");
    let conn = rusqlite::Connection::open(&index)?;
    let mut record = conn.prepare("SELECT id FROM documents WHERE parent_id=?1")?;
    let active_chunk: String = record.query_row([&active], |row| row.get(0))?;
    drop(record);
    conn.execute(
        "UPDATE documents SET vector=X'01' WHERE parent_id=?1",
        [&archived],
    )?;
    conn.execute("DELETE FROM documents WHERE id=?1", [&active_chunk])?;
    conn.execute(
        "UPDATE documents SET line_end=999 WHERE parent_id=?1",
        [&summary],
    )?;
    drop(conn);
    fs::remove_file(fixture.repo.join("known.md"))?;
    // Simulate interruption after a tombstone but before unlinking the Markdown
    // or pruning its indexed vector.
    let ledger = fixture.local().join("ledger.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&ledger)?)?;
    data["forgotten"][&forgotten] = serde_json::json!({});
    fs::write(&ledger, serde_json::to_vec(&data)?)?;
    let durable = fs::read(&ledger)?;
    let archived_bytes = fixture.entry_bytes(&archived)?;
    let forgotten_bytes = fixture.entry_bytes(&forgotten)?;
    let index_bytes = fs::read(&index)?;
    let dry = fixture.json(&["repair", "--dry-run", "--json"])?;
    for code in [
        "INVALID_EMBEDDING",
        "MISSING_CHUNK",
        "STALE_CHUNK",
        "ORPHAN_PARENT",
        "FORGOTTEN_INDEX_ROWS",
        "FORGET_FILE_PENDING",
    ] {
        assert!(
            dry["findings"][code]["count"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "missing {code}"
        );
    }
    assert_eq!(fs::read(&index)?, index_bytes);

    let cache = std::mem::replace(&mut fixture.cache, fixture._dir.path().join("empty-cache"));
    let offline = fixture.json(&["repair", "--json"])?;
    assert_eq!(offline["action"], "removed_stale_rows_embeddings_pending");
    let conn = rusqlite::Connection::open(&index)?;
    for id in [&forgotten, &archived, &summary] {
        let count: usize = conn.query_row(
            "SELECT count(*) FROM documents WHERE parent_id=?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
    }
    drop(conn);
    fixture.cache = cache;

    let mut writer = fixture
        .nunc(&["log", "Concurrent durable memory"])
        .stdout(Stdio::null())
        .spawn()?;
    let rebuilt = fixture.json(&["repair", "--json"])?;
    assert!(writer.wait()?.success());
    assert_eq!(rebuilt["action"], "rebuilt_index");
    assert_eq!(fs::read(&ledger)?, durable);
    assert_eq!(fixture.entry_bytes(&archived)?, archived_bytes);
    assert_eq!(fixture.entry_bytes(&forgotten)?, forgotten_bytes);
    let final_report = fixture.json(&["repair", "--json"])?;
    // A direct log may race with the first snapshot and get picked up by this run.
    assert!(matches!(
        final_report["action"].as_str(),
        Some("none" | "rebuilt_index")
    ));
    assert_eq!(fixture.json(&["repair", "--json"])?["action"], "none");
    let normal = fixture.json(&["search", "--json", "--limit", "30", "project coordination"])?;
    let raw = fixture.json(&[
        "search",
        "--raw",
        "--json",
        "--limit",
        "30",
        "project coordination",
    ])?;
    assert!(
        normal["results"]
            .as_array()
            .context("results")?
            .iter()
            .all(|hit| hit["id"] != archived && hit["id"] != forgotten)
    );
    let all = raw["results"].as_array().context("raw results")?;
    assert!(
        all.iter()
            .any(|hit| hit["id"] == archived && hit["status"] == "archived")
    );
    assert!(all.iter().all(|hit| hit["id"] != forgotten));

    fs::write(&index, b"corrupted SQLite file")?;
    assert_eq!(
        fixture.json(&["doctor", "--deep", "--json"])?["findings"]["INDEX_UNREADABLE"]["count"],
        1
    );
    assert_eq!(
        fixture.json(&["repair", "--json"])?["action"],
        "rebuilt_index"
    );
    Ok(())
}

#[test]
fn manual_archive_is_atomic_idempotent_and_preserves_consolidation_lineage() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.log_id("An original comparable decision")?;
    let second = fixture.log_id("An original comparable decision")?;
    let bytes = fixture.entry_bytes(&first)?;
    let ledger = fixture.local().join("ledger.json");
    let preview = fixture.json(&["archive", &first, &second, "--dry-run", "--json"])?;
    assert_eq!(preview["changed"], serde_json::json!([first, second]));
    assert!(!ledger.exists());
    let report = fixture.json(&["archive", &first, &second, "--json"])?;
    assert_eq!(report["changed"], preview["changed"]);
    assert_eq!(fixture.entry_bytes(&first)?, bytes);
    let durable = fs::read(&ledger)?;
    let retry = fixture.json(&["archive", &first, "--json"])?;
    assert_eq!(retry["unchanged"], serde_json::json!([first]));
    assert_eq!(fs::read(&ledger)?, durable);
    assert_eq!(
        fixture.json(&["duplicates", "--exact", "--json"])?["matches"],
        serde_json::json!([])
    );
    let raw = fixture.json(&["duplicates", "--exact", "--raw", "--json"])?;
    assert_eq!(raw["matches"].as_array().map(Vec::len), Some(1));
    assert!(
        !fixture
            .nunc(&["archive", &first, "00000000-0000-0000-0000-000000000001"])
            .output()?
            .status
            .success()
    );
    assert_eq!(fs::read(&ledger)?, durable);

    fixture.run(&["unarchive", &first])?;
    let joined = fixture.summary(&first, "First consolidated layer")?;
    let outer = fixture.summary(&joined, "Second consolidated layer")?;
    let conflict = fixture.nunc(&["unarchive", &second, &first]).output()?;
    assert!(!conflict.status.success());
    assert!(String::from_utf8(conflict.stderr)?.contains("consolidate undo"));
    assert_eq!(
        fixture.json(&["archive", &second, "--json"])?["unchanged"],
        serde_json::json!([second])
    );
    fixture.run(&["consolidate", "undo", &outer])?;
    fixture.run(&["archive", &joined])?;
    fixture.run(&["consolidate", "undo", &joined])?;
    assert!(
        !fixture
            .nunc(&["unarchive", &joined])
            .output()?
            .status
            .success()
    );

    fixture.run(&["forget", &second, "--yes"])?;
    assert!(
        !fixture
            .nunc(&["unarchive", &second])
            .output()?
            .status
            .success()
    );
    assert_eq!(fixture.entry_bytes(&first)?, bytes);
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn manual_archive_changes_search_visibility_without_reembedding() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let id = fixture.log_id("An archived decision about exclusive memory locks.")?;
    fixture.run(&["reindex"])?;
    fixture.run(&["archive", &id])?;
    let text = "memory locks";
    let normal = fixture.json(&["search", "--json", text])?;
    assert_eq!(normal["results"], serde_json::json!([]));
    let raw = fixture.json(&["search", "--raw", "--json", text])?;
    assert_eq!(raw["results"][0]["id"], id);
    assert_eq!(raw["results"][0]["archive_reason"], "manual");
    fixture.run(&["reindex", "--full"])?;
    assert_eq!(
        fixture.json(&["search", "--json", text])?["results"],
        serde_json::json!([])
    );
    fixture.run(&["unarchive", &id])?;
    assert_eq!(
        fixture.json(&["search", "--json", text])?["results"][0]["id"],
        id
    );
    fixture.run(&["archive", &id])?;
    fixture.run(&["forget", &id, "--yes"])?;
    assert_eq!(
        fixture.json(&["search", "--raw", "--json", text])?["results"],
        serde_json::json!([])
    );
    Ok(())
}

#[test]
fn concurrent_standalone_clients_return_unique_durable_receipts() -> Result<()> {
    let fixture = Fixture::new()?;
    let children = (0..12)
        .map(|ordinal| {
            let note = format!("independent writer {ordinal}\n\nbody without a final newline");
            let child = fixture
                .nunc(&["write", &note])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            Ok((note, child))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut ids = std::collections::BTreeSet::new();
    for (note, child) in children {
        let receipt = String::from_utf8(checked(child.wait_with_output()?)?.stdout)?;
        ensure!(receipt.starts_with("Saved "), "expected durable receipt");
        let id = receipt
            .split_whitespace()
            .nth(1)
            .context("missing receipt ID")?;
        assert!(ids.insert(id.to_owned()));
        let saved = String::from_utf8(fixture.entry_bytes(id)?)?;
        assert_eq!(body(&saved)?, note);
    }
    assert_eq!(ids.len(), 12);
    assert_eq!(fixture.entries()?.len(), 12);
    Ok(())
}

#[test]
fn linked_worktrees_have_distinct_memory_and_index_paths() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.commit("tracked source", "Create worktree fixture")?;
    let other = fixture._dir.path().join("linked");
    fixture.git(&[
        "worktree",
        "add",
        "--detach",
        other.to_str().context("worktree path")?,
    ])?;
    fixture.log_id("Memory private to the original worktree")?;
    assert!(!other.join(".nunc").exists());
    let original = String::from_utf8(fixture.run(&["status"])?.stdout)?;
    let linked = String::from_utf8(
        checked(fixture.nunc(&["status"]).current_dir(&other).output()?)?.stdout,
    )?;
    let index = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("Index "))
            .map(str::to_owned)
    };
    assert!(index(&original).is_some());
    assert!(index(&linked).is_some());
    assert_ne!(index(&original), index(&linked));
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn standalone_read_after_write_and_concurrent_visibility_follow_the_contract() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let archived = fixture.log_id("A timing decision to archive")?;
    let forgotten = fixture.log_id("A timing decision to forget")?;
    fixture.run(&["reindex"])?;
    let new = fixture.log_id("A new acknowledged timing decision")?;
    let search = |raw: bool| {
        let mut args = vec!["search", "--json", "--limit", "50"];
        if raw {
            args.push("--raw");
        }
        args.push("timing decision");
        fixture.json(&args)
    };
    let contains = |result: &Value, id: &str| -> bool {
        result["results"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["id"] == id))
    };
    let stale = search(false)?;
    assert!(!contains(&stale, &new));
    assert!(
        !stale["warnings"]
            .as_array()
            .context("missing warnings")?
            .is_empty()
    );

    // Block the reader at the index lock, then observe that it holds the
    // shared memory lock before starting the competing writers.
    let state = fs::read_dir(&fixture.state)?
        .next()
        .context("missing index state")??
        .path();
    let index_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("index.lock"))?;
    index_lock.lock()?;
    let reader = fixture
        .nunc(&["search", "--json", "--limit", "50", "timing decision"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let memory_probe = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.local().join("memory.lock"))?;
    eventually(|| match memory_probe.try_lock() {
        Ok(()) => {
            memory_probe.unlock()?;
            Ok(false)
        }
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    })?;
    let requests: &[&[&str]] = &[
        &["reindex", "--full"],
        &["archive", &archived],
        &["forget", &forgotten, "--yes"],
    ];
    let children = requests
        .iter()
        .map(|args| {
            fixture
                .nunc(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    assert!(fixture.entry_bytes(&forgotten).is_ok());
    drop(index_lock);
    let during: Value = serde_json::from_slice(&checked(reader.wait_with_output()?)?.stdout)?;
    assert!(contains(&during, &archived));
    assert!(contains(&during, &forgotten));
    for child in children {
        checked(child.wait_with_output()?)?;
    }

    // An explicit successful sync after the durable receipt is the visibility
    // barrier. Archive and tombstones remain authoritative across all orders.
    fixture.run(&["reindex"])?;
    let normal = search(false)?;
    assert!(contains(&normal, &new));
    assert!(!contains(&normal, &archived));
    assert!(!contains(&normal, &forgotten));
    let raw = search(true)?;
    assert!(contains(&raw, &archived));
    assert!(!contains(&raw, &forgotten));
    assert!(fixture.entry_bytes(&forgotten).is_err());
    Ok(())
}

#[test]
fn invalid_source_filters_fail_before_model_initialization() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::create_dir(fixture.repo.join("design"))?;
    fs::create_dir(fixture.repo.join("design-old"))?;
    std::os::unix::fs::symlink("../design-old", fixture.repo.join("design/link"))?;
    fixture.run(&["init", "--source", "design"])?;
    for (flag, path, expected) in [
        ("--source", "design-old", "outside the registered sources"),
        ("--source", "../outside", "repository-relative"),
        ("--source", "design/link", "symlink or special file"),
        ("--source-file", "design", "requires a file"),
    ] {
        let failure = fixture.nunc(&["search", "decision", flag, path]).output()?;
        assert!(!failure.status.success());
        assert!(failure.stdout.is_empty());
        assert!(String::from_utf8(failure.stderr)?.contains(expected));
    }
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn exact_source_filters_union_and_apply_before_ranking() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    for (name, body) in [
        (
            "docs/decisions/db.md",
            "# SQLite\n\nKeep the ledger outside the vector index.",
        ),
        (
            "docs/decisions/sub/storage.md",
            "# Storage\n\nThe SQLite index is disposable.\n\n## Rebuild\n\nRebuild from Markdown when needed.",
        ),
        (
            "docs/design-old/README.md",
            "# Design\n\nSQLite concurrency SQLite concurrency.",
        ),
        (
            "docs/design/proposal.md",
            "# Design\n\nRender initialization output in the CLI.",
        ),
        (
            "docs/rfcs/sqlite.md",
            "# Draft SQLite RFC\n\nLock readers before replacing the database.",
        ),
        (
            "docs/plans/delivery.md",
            "# Plans\n\nDeliver the worktree repair command.",
        ),
        (
            "docs/incidents/failure.md",
            "# Incident\n\nA damaged SQLite database can be rebuilt.",
        ),
        (
            "docs/research/notes.md",
            "# Research\n\nAn evaluation of SQLite locking.",
        ),
    ] {
        let path = fixture.repo.join(name);
        fs::create_dir_all(path.parent().context("source parent")?)?;
        fs::write(path, body)?;
    }
    fs::create_dir(fixture.repo.join("docs/empty"))?;
    let memory = fixture.log_id("A memory about SQLite concurrency")?;
    fixture.run(&["reindex", "--source", "docs"])?;
    let query = |flags: &[&str]| -> Result<Value> {
        let mut args = vec!["search", "--json", "SQLite concurrency"];
        args.extend_from_slice(flags);
        fixture.json(&args)
    };
    let solo = query(&["--source-file", "./docs/design/proposal.md", "--limit", "1"])?;
    assert_eq!(solo["results"][0]["path"], "docs/design/proposal.md");
    assert_eq!(solo["results"][0]["matched_chunk"]["line_start"], 1);

    let union = query(&[
        "--source",
        "./docs/decisions/",
        "--source-file",
        "docs/rfcs/sqlite.md",
        "--source-file",
        "docs/incidents/failure.md",
        "--limit",
        "20",
    ])?;
    let paths: std::collections::BTreeSet<_> = union["results"]
        .as_array()
        .context("results")?
        .iter()
        .filter_map(|hit| hit["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "docs/decisions/db.md",
            "docs/decisions/sub/storage.md",
            "docs/incidents/failure.md",
            "docs/rfcs/sqlite.md"
        ]
        .into()
    );
    assert!(
        union["results"]
            .as_array()
            .context("results")?
            .iter()
            .filter(|hit| hit["path"] == "docs/decisions/sub/storage.md")
            .count()
            > 1
    );
    assert_eq!(
        query(&["--source", "docs/empty"])?["results"],
        serde_json::json!([])
    );
    let mixed = query(&[
        "--source",
        ".nunc/entries/",
        "--source-file",
        "docs/plans/delivery.md",
    ])?;
    let hits = mixed["results"].as_array().context("mixed results")?;
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|hit| hit["id"] == memory));

    fs::remove_file(fixture.repo.join("docs/research/notes.md"))?;
    let obsolete = query(&["--source-file", "docs/research/notes.md"])?;
    assert!(
        !obsolete["warnings"]
            .as_array()
            .context("freshness warning")?
            .is_empty()
    );
    assert_eq!(obsolete["results"][0]["path"], "docs/research/notes.md");
    fixture.run(&["reindex"])?;
    let failure = fixture
        .nunc(&[
            "search",
            "decision",
            "--json",
            "--source-file",
            "docs/research/notes.md",
        ])
        .output()?;
    assert!(failure.stdout.is_empty());
    assert!(!failure.status.success());
    assert!(
        String::from_utf8(failure.stderr)?.contains("does not exist and has no indexed matches")
    );

    fixture.run(&["reindex", "--source", "docs/rfcs/sqlite.md"])?;
    let direct = query(&["--source-file", "docs/rfcs/sqlite.md"])?;
    assert_eq!(direct["results"][0]["path"], "docs/rfcs/sqlite.md");
    Ok(())
}

#[test]
fn search_ids_rejects_incompatible_output_and_keeps_errors_off_stdout() -> Result<()> {
    let fixture = Fixture::new()?;
    for option in ["--json", "--explain"] {
        let output = fixture
            .nunc(&["search", "memory", "--ids", option])
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    let output = fixture.nunc(&["search", "memory", "--ids"]).output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("model unavailable"));
    Ok(())
}

#[test]
fn diff_compares_verbatim_markdown_without_model_or_index() -> Result<()> {
    let fixture = Fixture::new()?;
    let old_body = "# Café\r\n\n```rust\nlet limit = 5;\n```\ntrailing CR\r";
    let long_line = "界".repeat(30_000);
    let new_body = format!("# Café\r\n\n```rust\nlet limit = 7;\n```\n{long_line}\n");
    let old = fixture.log_id(old_body)?;
    let same = fixture.log_id(old_body)?;
    let new = fixture.log_id(&new_body)?;
    for id in [&old, &same] {
        let equal = fixture.run(&["diff", &old, id])?;
        assert!(equal.stdout.is_empty());
        assert!(equal.stderr.is_empty());
    }

    let path = fixture.local().join("entries").join(format!("{old}.md"));
    let original = fs::read(&path)?;
    fixture.run(&["archive", &old])?;
    let diff = fixture.nunc(&["diff", &old, &new]).output()?;
    assert_eq!(diff.status.code(), Some(1));
    assert!(diff.stderr.is_empty());
    assert_eq!(
        String::from_utf8(diff.stdout)?,
        format!(
            "--- a/{old}.md\n+++ b/{new}.md\n@@ -1,6 +1,6 @@\n # Café\r\n \n ```rust\n-let limit = 5;\n+let limit = 7;\n ```\n-trailing CR\r\n\\ No newline at end of file\n+{long_line}\n"
        )
    );
    assert_eq!(fs::read(&path)?, original);
    assert!(!fixture.state.exists());
    assert!(!fixture.cache.exists());
    Ok(())
}

#[test]
fn diff_reports_invalid_missing_empty_and_forgotten_entries_without_output() -> Result<()> {
    let fixture = Fixture::new()?;
    let valid = fixture.log_id("A readable note")?;
    let forgotten = fixture.log_id("Forgotten body must stay hidden")?;
    let dir = fixture.local().join("entries");
    let path = dir.join(format!("{forgotten}.md"));
    let original = fs::read(&path)?;
    fixture.run(&["forget", &forgotten, "--yes"])?;
    // An interrupted cleanup or a restored file cannot defeat the tombstone.
    fs::write(&path, original)?;
    let empty = uuid::Uuid::new_v4().to_string();
    fs::write(dir.join(format!("{empty}.md")), "")?;
    let missing = uuid::Uuid::new_v4().to_string();
    for (id, message) in [
        ("invalid", "invalid value"),
        (&missing, "cannot inspect memory"),
        (&empty, "missing front matter"),
        (&forgotten, "was forgotten"),
    ] {
        let output = fixture.nunc(&["diff", &valid, id]).output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr)?;
        assert!(error.contains(message), "{error}");
        assert!(!error.contains("Forgotten body"));
    }
    Ok(())
}

#[test]
#[ignore = "requires the explicitly prepared ONNX model cache"]
fn search_ids_orders_unique_parent_ids_and_preserves_visibility() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.cache = real_model_cache()?;
    let multi = fixture.log_id("# SQLite\n\nSQLite writer locks.\n\n## Locks\n\nSQLite guard ordering.\n\n## Safety\n\nSQLite reindex and lock ordering.")?;
    let short = fixture.log_id("A brief note about SQLite concurrency.")?;
    fixture.commit("Git-derived lock information", "SQLite lock history")?;
    fixture.run(&["git", "ingest", "HEAD"])?;
    fs::write(
        fixture.repo.join("spec.md"),
        "# SQLite\n\nIndexed source.\n\n## Writer\n\nWriter mutex.",
    )?;
    fixture.run(&["reindex", "--source", "spec.md"])?;

    let json = fixture.json(&[
        "search",
        "SQLite locks",
        "--type",
        "memory",
        "--json",
        "--limit",
        "30",
    ])?;
    let hits = json["results"].as_array().context("missing hits")?;
    assert!(hits.iter().filter(|hit| hit["id"] == multi).count() > 1);
    let mut seen = std::collections::BTreeSet::new();
    let expected: Vec<_> = hits
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .filter(|id| seen.insert(*id))
        .collect();
    assert_eq!(expected.len(), 3);
    assert!(hits.iter().any(|hit| hit["entry_kind"] == "git-commit"));
    let output = fixture.run(&[
        "search",
        "SQLite locks",
        "--type",
        "memory",
        "--ids",
        "--limit",
        "2",
    ])?;
    assert_eq!(
        output.stdout,
        format!("{}\n{}\n", expected[0], expected[1]).as_bytes()
    );

    let sources = fixture.run(&[
        "search",
        "SQLite locks",
        "--source-file",
        "spec.md",
        "--ids",
    ])?;
    let source = String::from_utf8(sources.stdout)?;
    assert_eq!(source.lines().count(), 1);
    assert!(source.starts_with("source:"));

    fixture.run(&["archive", &multi])?;
    fixture.run(&["forget", &short, "--yes"])?;
    let normal = String::from_utf8(
        fixture
            .run(&["search", "SQLite locks", "--type", "memory", "--ids"])?
            .stdout,
    )?;
    assert!(!normal.contains(&multi));
    assert!(!normal.contains(&short));
    let raw = String::from_utf8(
        fixture
            .run(&[
                "search",
                "SQLite locks",
                "--type",
                "memory",
                "--ids",
                "--raw",
            ])?
            .stdout,
    )?;
    assert!(raw.lines().any(|id| id == multi));
    assert!(!raw.contains(&short));
    fs::create_dir(fixture.repo.join("empty"))?;
    fixture.run(&["reindex", "--source", "empty"])?;
    let empty = fixture.run(&["search", "SQLite locks", "--source", "empty", "--ids"])?;
    assert!(empty.stdout.is_empty());
    Ok(())
}
