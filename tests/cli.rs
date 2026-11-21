use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::fs;
use std::io::Write;
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
        fs::read_dir(self.local().join("entries"))?
            .map(|item| Ok(fs::read_to_string(item?.path())?))
            .collect()
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
            "unselected",
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
