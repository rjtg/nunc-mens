use crate::entries::{self, ContextFields, Receipt};
use crate::fs::{atomic_write, effective_uid, lockfile, missing, private_dir, read_regular};
use crate::protocol::{Boundary, Parser};
use crate::repo::Repo;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};
use std::ffi::CString;
use std::fs::{self, File, Metadata, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const POLL: Duration = Duration::from_millis(20);
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const FLAGS: i32 = libc::O_NONBLOCK | libc::O_NOFOLLOW;

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Session {
    version: u8,
    id: Uuid,
    boundary: String,
    pid: u32,
    device: u64,
    inode: u64,
}

struct Runtime {
    fifo: PathBuf,
    session: PathBuf,
    owner: PathBuf,
    writer: PathBuf,
}

impl Runtime {
    fn new(repo: &Repo) -> Self {
        let local = repo.local();
        Self {
            fifo: local.join("input.fifo"),
            session: local.join("session.json"),
            owner: local.join("serve.lock"),
            writer: local.join("write.lock"),
        }
    }

    fn remove_artifacts(&self) -> Result<()> {
        remove_owned(&self.session, Metadata::is_file)?;
        remove_owned(&self.fifo, |meta| meta.file_type().is_fifo())
    }

    fn session(&self) -> Result<Option<Session>> {
        let bytes = match read_regular(&self.session, 4096) {
            Ok(bytes) => bytes,
            Err(err) if missing(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        let session: Session =
            serde_json::from_slice(&bytes).context("parse FIFO session metadata")?;
        ensure!(session.version == 1, "unsupported FIFO session version");
        Ok(Some(session))
    }
}

struct Running<'a>(&'a Runtime);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.remove_artifacts() {
            eprintln!("nunc: cleanup FIFO session: {error:#}");
        }
    }
}

fn remove_owned(path: &Path, expected: impl FnOnce(&Metadata) -> bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                expected(&meta) && meta.uid() == effective_uid() && meta.mode() & 0o077 == 0,
                "unexpected runtime file at {}; inspect it before restarting",
                path.display()
            );
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("inspect {}", path.display())),
    }
}

fn create_fifo(path: &Path) -> Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    // name is a valid, NUL-terminated pathname for the duration of the call.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == -1 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("create FIFO {}", path.display()));
    }
    Ok(())
}

fn fifo_metadata(file: &File) -> Result<Metadata> {
    let meta = file.metadata()?;
    ensure!(
        meta.file_type().is_fifo() && meta.uid() == effective_uid() && meta.mode() & 0o077 == 0,
        "FIFO must be owned by you and have owner-only permissions"
    );
    Ok(meta)
}

pub fn serve(repo: &Repo, boundary: Option<String>, sources: &[PathBuf]) -> Result<()> {
    let boundary = Boundary::new(boundary)?;
    repo.prepare()?;
    let runtime = Runtime::new(repo);
    let owner = lockfile(&runtime.owner)?;
    match owner.try_lock() {
        Ok(()) => (),
        Err(TryLockError::WouldBlock) => bail!("a server already owns {}", repo.local().display()),
        Err(TryLockError::Error(err)) => return Err(err).context("lock FIFO server ownership"),
    }
    runtime.remove_artifacts()?;
    repo.configure(sources, false)?;
    repo.config()?;
    let stop = Arc::new(AtomicBool::new(false));
    let _signals = Signals::new(&stop)?;
    create_fifo(&runtime.fifo)?;
    let _running = Running(&runtime);
    let mut pipe = File::options()
        .read(true)
        .custom_flags(FLAGS)
        .open(&runtime.fifo)
        .context("open FIFO reader")?;
    let meta = fifo_metadata(&pipe)?;
    let session = Session {
        version: 1,
        id: Uuid::new_v4(),
        boundary: boundary.value().to_owned(),
        pid: std::process::id(),
        device: meta.dev(),
        inode: meta.ino(),
    };
    atomic_write(
        &runtime.session,
        &serde_json::to_vec_pretty(&session)?,
        false,
    )?;
    println!(
        "FIFO {}\nBoundary {}",
        runtime.fifo.display(),
        boundary.value()
    );
    io::stdout().flush().context("publish server endpoint")?;
    read_frames(&mut pipe, &stop, &boundary, repo)
}

struct Signals(Vec<signal_hook::SigId>);

impl Signals {
    fn new(stop: &Arc<AtomicBool>) -> Result<Self> {
        let mut signals = Self(Vec::new());
        for signal in [SIGINT, SIGTERM] {
            signals.0.push(
                flag::register(signal, Arc::clone(stop)).context("register shutdown signal")?,
            );
        }
        Ok(signals)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in &self.0 {
            signal_hook::low_level::unregister(*id);
        }
    }
}

fn read_frames(pipe: &mut File, stop: &AtomicBool, boundary: &Boundary, repo: &Repo) -> Result<()> {
    let mut parser = Parser::new(boundary);
    let mut bytes = [0; 16 * 1024];
    loop {
        match pipe.read(&mut bytes) {
            Ok(0) => {
                if parser.end_stream() {
                    eprintln!("nunc: discarded incomplete FIFO entry at end of stream");
                }
            }
            Ok(count) => {
                parser.push(&bytes[..count], |record| {
                    match record
                        .and_then(|body| entries::persist(repo, &body, ContextFields::default()))
                    {
                        Ok(receipt) => println!("Saved {} {}", receipt.id, receipt.path.display()),
                        Err(error) => eprintln!("nunc: FIFO capture failed: {error:#}"),
                    }
                });
                if !stop.load(Ordering::Relaxed) {
                    continue;
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(err) => return Err(err).context("read FIFO"),
        }
        if stop.load(Ordering::Relaxed) {
            if parser.end_stream() {
                eprintln!("nunc: discarded incomplete FIFO entry during shutdown");
            }
            return Ok(());
        }
        thread::sleep(POLL);
    }
}

pub enum Delivery {
    Queued,
    Saved(Receipt),
}

pub fn write(repo: &Repo, body: &str) -> Result<Delivery> {
    entries::validate_body(body)?;
    match fs::symlink_metadata(repo.local()) {
        Ok(_) => private_dir(&repo.local())?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return direct(repo, body),
        Err(err) => return Err(err).context("inspect FIFO runtime directory"),
    }
    let runtime = Runtime::new(repo);
    if deliver(&runtime, body)? {
        return Ok(Delivery::Queued);
    }
    direct(repo, body)
}

fn direct(repo: &Repo, body: &str) -> Result<Delivery> {
    Ok(Delivery::Saved(entries::persist(
        repo,
        body,
        ContextFields::default(),
    )?))
}

fn deliver(runtime: &Runtime, body: &str) -> Result<bool> {
    let deadline = Instant::now() + SEND_TIMEOUT;
    let writer = lockfile(&runtime.writer)?;
    loop {
        match writer.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(POLL),
            Err(TryLockError::WouldBlock) => {
                bail!("FIFO writers are busy; use nunc log for direct capture")
            }
            Err(TryLockError::Error(err)) => return Err(err).context("lock FIFO writer"),
        }
    }
    let owner = lockfile(&runtime.owner)?;
    match owner.try_lock_shared() {
        Ok(()) => return Ok(false),
        Err(TryLockError::WouldBlock) => (),
        Err(TryLockError::Error(err)) => return Err(err).context("check FIFO ownership"),
    }
    let Some(session) = runtime.session()? else {
        return Ok(false);
    };
    let mut pipe = match File::options()
        .write(true)
        .custom_flags(FLAGS)
        .open(&runtime.fifo)
    {
        Ok(pipe) => pipe,
        Err(err)
            if err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENXIO) =>
        {
            return Ok(false);
        }
        Err(err) => {
            return Err(err).with_context(|| format!("open FIFO {}", runtime.fifo.display()));
        }
    };
    let meta = fifo_metadata(&pipe)?;
    if (meta.dev(), meta.ino()) != (session.device, session.inode)
        || runtime.session()?.as_ref() != Some(&session)
    {
        return Ok(false);
    }
    let frame = Boundary::new(Some(session.boundary))?.frame(body)?;
    send_frame(&mut pipe, &frame, Instant::now() + SEND_TIMEOUT)
}

fn send_frame(pipe: &mut File, frame: &[u8], deadline: Instant) -> Result<bool> {
    let mut sent = 0;
    while sent < frame.len() {
        match pipe.write(&frame[sent..]) {
            Ok(count) if count > 0 => sent += count,
            Ok(_) => bail!("FIFO write accepted {sent} bytes before stopping"),
            Err(err) if err.kind() == io::ErrorKind::Interrupted && Instant::now() < deadline => (),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(POLL)
            }
            Err(err) if sent == 0 && err.kind() == io::ErrorKind::BrokenPipe => return Ok(false),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "FIFO delivery failed after {sent} bytes; use nunc log for direct capture"
                    )
                });
            }
        }
    }
    Ok(true)
}
