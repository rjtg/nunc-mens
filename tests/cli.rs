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

    let pairs = fixture.json(&["duplicates", &parent, "--threshold", "0.86", "--json"])?;
    let pairs = pairs["matches"].as_array().context("no duplicate list")?;
    assert!(pairs.iter().any(|pair| pair["detection"] == "semantic"
        && pair["scope"] == "chunk"
        && (pair["left"]["id"] == paraphrase || pair["right"]["id"] == paraphrase)));
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
