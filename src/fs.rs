use anyhow::{Context, Result, ensure};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(err) => return Err(err).with_context(|| format!("create {}", path.display())),
    }
    let meta = fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        meta.is_dir() && meta.uid() == effective_uid() && meta.mode() & 0o022 == 0,
        "{} must be a directory owned by you and writable only by you",
        path.display()
    );
    Ok(())
}

pub(crate) fn effective_uid() -> u32 {
    // geteuid has no preconditions or failure return.
    unsafe { libc::geteuid() }
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    atomic_write_mode(path, bytes, replace, 0o600)
}

pub(crate) fn atomic_write_mode(path: &Path, bytes: &[u8], replace: bool, mode: u32) -> Result<()> {
    let parent = path.parent().context("publication path has no parent")?;
    let publish = || -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        if replace {
            tmp.persist(path)?;
        } else {
            tmp.persist_noclobber(path)?;
        }
        sync_dir(parent)
    };
    publish().with_context(|| format!("publish {}", path.display()))
}

pub(crate) fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let read = || -> Result<Vec<u8>> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        bounded_read(&file, limit)
    };
    read().with_context(|| format!("read {}", path.display()))
}

pub(crate) fn bounded_read(file: &File, limit: usize) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "expected a regular file");
    ensure!(metadata.len() <= limit as u64, "file exceeds {limit} bytes");
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "file exceeds {limit} bytes");
    Ok(bytes)
}

pub(crate) fn lockfile(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))?;
    validate_lock(file, path)
}

pub(crate) fn existing_lockfile(path: &Path) -> Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => validate_lock(file, path).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("open lock {}", path.display())),
    }
}

fn validate_lock(file: File, path: &Path) -> Result<File> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == effective_uid() && meta.mode() & 0o077 == 0,
        "unsafe lock file {}",
        path.display()
    );
    Ok(file)
}

pub(crate) fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}
