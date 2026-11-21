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
    let collision = fixture
        .nunc(&["write", "--", "collision\n--test-boundary"])
        .output()?;
    assert!(!collision.status.success());
    let mut clients = Vec::new();
    let mut expected = vec![
        "raw\r\n\n```\ncode\n```\n".to_owned(),
        "second raw\n".to_owned(),
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
