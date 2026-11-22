//! One versioned search request per connection on a private Unix socket.
use crate::cli;
use crate::fs::{effective_uid, private_dir};
use crate::index::ReadCache;
use crate::model::SharedModel;
use crate::repo::Repo;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

const VERSION: u8 = 1;
const REQUEST_LIMIT: usize = 1024 * 1024;
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const QUEUE: usize = 8;
const TICK: Duration = Duration::from_millis(25);
const IO_TICK: Duration = Duration::from_millis(150);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
const SUCCESS: u8 = 0;
const FAILURE: u8 = 1;
const READY: u8 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    args: Vec<Vec<u8>>,
}

fn directory(repo: &Repo) -> PathBuf {
    repo.local().join("search-runtime")
}

fn validate_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == effective_uid() && meta.mode() & 0o077 == 0,
        "search runtime directory must be private and owned by you"
    );
    Ok(())
}

fn socket_metadata(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.file_type().is_socket() && meta.uid() == effective_uid(),
        "search endpoint must be a Unix socket owned by you"
    );
    Ok(meta)
}

pub(crate) struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
}

impl Endpoint {
    pub(crate) fn bind(repo: &Repo) -> Result<Self> {
        let dir = directory(repo);
        private_dir(&dir)?;
        validate_directory(&dir)?;
        let path = dir.join("search.sock");
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                socket_metadata(&path)?;
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind search socket {}", path.display()))?;
        let meta = socket_metadata(&path)?;
        let endpoint = Self {
            listener,
            path,
            identity: (meta.dev(), meta.ino()),
        };
        fs::set_permissions(&endpoint.path, fs::Permissions::from_mode(0o600))?;
        endpoint.listener.set_nonblocking(true)?;
        Ok(endpoint)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn during_capture(
        &self,
        repo: &Repo,
        model: &SharedModel,
        stop: &AtomicBool,
        capture: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(QUEUE);
            let intake = scope.spawn(move || {
                let result = self.accept(send, stop);
                if result.is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
                result
            });
            let worker = scope.spawn(move || {
                let mut cache = ReadCache::default();
                while !stop.load(Ordering::Relaxed) {
                    match receive.recv_timeout(TICK) {
                        Ok(stream) => {
                            let _ = handle(stream, repo, model, &mut cache, stop);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            let capture = capture();
            stop.store(true, Ordering::Relaxed);
            let intake = intake
                .join()
                .map_err(|_| anyhow::anyhow!("search socket listener panicked"))?;
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("search socket worker panicked"))?;
            capture.and(intake)
        })
    }

    fn accept(&self, send: SyncSender<UnixStream>, stop: &AtomicBool) -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false)?;
                    match send.try_send(stream) {
                        Ok(()) => {}
                        Err(TrySendError::Full(busy)) => {
                            stream = busy;
                            stream.set_nonblocking(true)?;
                            let _ = write_error(&mut stream, "search server is busy; retry");
                        }
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("accept search socket client"),
            }
        }
        Ok(())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if socket_metadata(&self.path).is_ok_and(|meta| (meta.dev(), meta.ino()) == self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn request(
    repo: &Repo,
    argv: &[OsString],
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    let dir = directory(repo);
    let path = dir.join("search.sock");
    let unavailable = || {
        format!(
            "search server unavailable at {}; start nunc serve --search-socket",
            path.display()
        )
    };
    validate_directory(&dir).with_context(unavailable)?;
    ensure!(
        socket_metadata(&path).with_context(unavailable)?.mode() & 0o077 == 0,
        "search socket permissions must be owner-only"
    );
    let request = serde_json::to_vec(&Request {
        version: VERSION,
        args: std::iter::once(b"nunc".to_vec())
            .chain(argv.iter().skip(1).map(|arg| arg.as_bytes().to_vec()))
            .collect(),
    })?;
    ensure!(
        request.len() <= REQUEST_LIMIT,
        "search request exceeds socket limit"
    );
    let mut stream = UnixStream::connect(&path).with_context(unavailable)?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut status = [0];
    read_exact(&mut stream, &mut status, deadline, None)?;
    if status[0] == FAILURE {
        return read_failure(&mut stream, deadline);
    }
    ensure!(
        status[0] == READY,
        "invalid search socket admission response"
    );
    write_frame(&mut DeadlineWriter::new(&mut stream), &request)?;
    read_exact(&mut stream, &mut status, deadline, None)?;
    match status[0] {
        SUCCESS => {
            let stdout = read_frame(&mut stream, OUTPUT_LIMIT, deadline, None)?;
            let stderr = read_frame(&mut stream, DIAGNOSTIC_LIMIT, deadline, None)?;
            out.write_all(&stdout)?;
            err.write_all(&stderr)?;
        }
        FAILURE => return read_failure(&mut stream, deadline),
        _ => anyhow::bail!("invalid search socket response"),
    }
    Ok(())
}

fn read_failure(stream: &mut UnixStream, deadline: Instant) -> Result<()> {
    let error = read_frame(stream, DIAGNOSTIC_LIMIT, deadline, None)?;
    anyhow::bail!("{}", String::from_utf8_lossy(&error))
}

fn handle(
    mut stream: UnixStream,
    repo: &Repo,
    model: &SharedModel,
    cache: &mut ReadCache,
    stop: &AtomicBool,
) -> Result<()> {
    DeadlineWriter::new(&mut stream).write_all(&[READY])?;
    let result = (|| -> Result<(Buffer, Buffer)> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let payload = read_frame(&mut stream, REQUEST_LIMIT, deadline, Some(stop))?;
        let request: Request =
            serde_json::from_slice(&payload).context("invalid search request")?;
        ensure!(
            request.version == VERSION,
            "unsupported search protocol version"
        );
        let expires = Instant::now() + QUERY_TIMEOUT;
        let checkpoint = || -> Result<()> {
            ensure!(Instant::now() < expires, "search request timed out");
            ensure!(
                !stop.load(Ordering::Relaxed) && connected(&stream),
                "search request cancelled"
            );
            Ok(())
        };
        checkpoint()?;
        let argv = request.args.into_iter().map(OsString::from_vec).collect();
        let mut output = Buffer::new(OUTPUT_LIMIT);
        let mut diagnostics = Buffer::new(DIAGNOSTIC_LIMIT);
        cli::serve_search(
            repo,
            argv,
            model,
            cache,
            &mut output,
            &mut diagnostics,
            &checkpoint,
        )?;
        Ok((output, diagnostics))
    })();
    let mut writer = DeadlineWriter::new(&mut stream);
    match result {
        Ok((stdout, stderr)) => {
            writer.write_all(&[SUCCESS])?;
            write_frame(&mut writer, &stdout.data)?;
            write_frame(&mut writer, &stderr.data)?;
        }
        Err(error) => write_error(&mut writer, &format!("{error:#}"))?,
    }
    Ok(())
}

fn connected(stream: &UnixStream) -> bool {
    let mut probe = 0u8;
    // Peek without consuming request bytes or blocking the worker.
    let read = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut probe as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    read > 0
        || (read < 0
            && matches!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ))
}

fn read_exact(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            return Err(io::Error::other("search server is shutting down"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search socket read timed out",
            ));
        }
        stream.set_read_timeout(Some(remaining.min(IO_TICK)))?;
        match stream.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete search socket frame",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_frame(
    stream: &mut UnixStream,
    limit: usize,
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    read_exact(stream, &mut size, deadline, stop)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= limit, "search socket frame exceeds size limit");
    let mut bytes = vec![0; size];
    read_exact(stream, &mut bytes, deadline, stop)?;
    Ok(bytes)
}

fn write_frame(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let size = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    out.write_all(&size.to_be_bytes())?;
    out.write_all(bytes)
}

fn write_error(out: &mut impl Write, error: &str) -> io::Result<()> {
    let bytes = error.as_bytes();
    out.write_all(&[FAILURE])?;
    write_frame(out, &bytes[..bytes.len().min(DIAGNOSTIC_LIMIT)])
}

struct DeadlineWriter<'a> {
    stream: &'a mut UnixStream,
    expires: Instant,
}

impl<'a> DeadlineWriter<'a> {
    fn new(stream: &'a mut UnixStream) -> Self {
        Self {
            stream,
            expires: Instant::now() + REQUEST_TIMEOUT,
        }
    }
}

impl Write for DeadlineWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.expires.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search socket write timed out",
            ));
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Buffer {
    data: Vec<u8>,
    limit: usize,
}

impl Buffer {
    fn new(limit: usize) -> Self {
        Self {
            data: Vec::new(),
            limit,
        }
    }
}

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.data.len() {
            return Err(io::Error::other(
                "search socket output exceeds size limit; use --limit or --max-tokens",
            ));
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
