use crate::fs::{atomic_write, lockfile, missing, private_dir, read_regular, sync_dir};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const OLD_IGNORE: &[u8] = b"/entries/\n/input.fifo\n/session.json\n/*.lock\n/.tmp*\n";
const PREVIOUS_IGNORE: &[u8] =
    b"/entries/\n/ledger.json\n/input.fifo\n/session.json\n/*.lock\n/.tmp*\n";
const LOCAL_IGNORE: &[u8] =
    b"/entries/\n/ledger.json\n/input.fifo\n/session.json\n/search-runtime/\n/*.lock\n/.tmp*\n";

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    pub id: String,
    pub git: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub sources: Vec<PathBuf>,
    pub(crate) chunking: crate::chunk::Settings,
    pub(crate) search: crate::search::Settings,
    pub(crate) git: crate::git::Settings,
}

impl Repo {
    pub fn discover() -> Result<Self> {
        let cwd = env::current_dir().context("determine current directory")?;
        if let Some(path) = env::var_os("NUNC_REPO_ROOT") {
            return Self::new(Path::new(&path));
        }
        let git = git_root(&cwd)?;
        Self::at(git.as_deref().unwrap_or(&cwd), git.is_some())
    }

    pub fn new(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("resolve {}", root.display()))?;
        let git = git_root(&root)?.is_some_and(|found| found == root);
        Self::at(&root, git)
    }

    fn at(root: &Path, git: bool) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("resolve {}", root.display()))?;
        ensure!(root.is_dir(), "{} is not a directory", root.display());
        let id = hash(root.as_os_str().as_bytes());
        Ok(Self { root, id, git })
    }

    pub fn local(&self) -> PathBuf {
        self.root.join(".nunc")
    }

    pub fn entries(&self) -> PathBuf {
        self.local().join("entries")
    }

    pub fn state(&self) -> Result<PathBuf> {
        Ok(self
            .external_dir("NUNC_STATE_DIR", "XDG_STATE_HOME", ".local/state", "")?
            .join(&self.id))
    }

    pub fn model_cache(&self) -> Result<PathBuf> {
        self.external_dir("NUNC_MODEL_CACHE_DIR", "XDG_CACHE_HOME", ".cache", "models")
    }

    fn external_dir(&self, var: &str, xdg: &str, fallback: &str, suffix: &str) -> Result<PathBuf> {
        let target = match env::var_os(var) {
            Some(path) => PathBuf::from(path),
            None => {
                let base = env::var_os(xdg)
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute());
                let base = base
                    .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
                    .with_context(|| format!("set {var} or HOME"))?;
                base.join("nunc-mens").join(suffix)
            }
        };
        let target = prospective_path(&target)?;
        ensure!(
            !target.starts_with(&self.root),
            "{var} must resolve outside {}",
            self.root.display()
        );
        Ok(target)
    }

    pub fn prepare(&self) -> Result<()> {
        private_dir(&self.local())?;
        let local = self.local();
        let ignore = local.join(".gitignore");
        let old_default = || {
            read_regular(&ignore, 4096)
                .is_ok_and(|contents| contents == OLD_IGNORE || contents == PREVIOUS_IGNORE)
        };
        if !ignore.try_exists()? || old_default() {
            let _lock = self.config_lock()?;
            if !ignore.try_exists()? {
                atomic_write(&ignore, LOCAL_IGNORE, false)?;
            } else if old_default() {
                atomic_write(&ignore, LOCAL_IGNORE, true)?;
            }
        }
        private_dir(&self.entries())?;
        sync_dir(&local)?;
        sync_dir(&self.root)
    }

    pub fn config(&self) -> Result<Config> {
        match fs::symlink_metadata(self.local()) {
            Ok(_) => private_dir(&self.local())?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => return Err(err).context("inspect repository runtime"),
        }
        let path = self.local().join("config.toml");
        let bytes = match read_regular(&path, 64 * 1024) {
            Ok(bytes) => bytes,
            Err(err) if missing(&err) => return Ok(Config::default()),
            Err(err) => return Err(err),
        };
        let config: Config = toml::from_str(std::str::from_utf8(&bytes)?)
            .with_context(|| format!("parse {}", path.display()))?;
        config.chunking.validate()?;
        config.search.validate()?;
        config.git.validate()?;
        Ok(Config {
            sources: normalize_sources(&config.sources)?,
            chunking: config.chunking,
            search: config.search,
            git: config.git,
        })
    }

    pub fn configure(&self, sources: &[PathBuf], create: bool) -> Result<()> {
        if sources.is_empty() && !create {
            return Ok(());
        }
        self.prepare()?;
        let _lock = self.config_lock()?;
        let path = self.local().join("config.toml");
        if sources.is_empty() && path.try_exists()? {
            self.config()?;
            return Ok(());
        }
        let sources = normalize_sources(sources)?;
        for source in &sources {
            self.validate_source(source, false)?;
        }
        let config = Config {
            sources,
            ..self.config()?
        };
        atomic_write(&path, toml::to_string_pretty(&config)?.as_bytes(), true)
    }

    fn config_lock(&self) -> Result<fs::File> {
        let lock = lockfile(&self.local().join("config.lock"))?;
        lock.lock().context("lock repository config")?;
        Ok(lock)
    }

    pub(crate) fn validate_source(&self, relative: &Path, allow_missing: bool) -> Result<bool> {
        match self.source_location(relative)? {
            SourceLocation::File | SourceLocation::Directory => Ok(true),
            SourceLocation::Missing if allow_missing => Ok(false),
            SourceLocation::Unsupported if allow_missing => Ok(false),
            SourceLocation::Missing => {
                anyhow::bail!("source path does not exist: {}", relative.display())
            }
            SourceLocation::Unsupported => anyhow::bail!(
                "source is a symlink or special file: {}",
                relative.display()
            ),
        }
    }

    pub(crate) fn source_location(&self, relative: &Path) -> Result<SourceLocation> {
        let mut path = self.root.clone();
        let mut location = SourceLocation::Directory;
        for part in relative.components() {
            path.push(part);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(SourceLocation::Missing);
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("inspect source {}", path.display()));
                }
            };
            location = if metadata.is_dir() {
                SourceLocation::Directory
            } else if metadata.is_file() {
                SourceLocation::File
            } else {
                return Ok(SourceLocation::Unsupported);
            };
        }
        Ok(location)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceLocation {
    File,
    Directory,
    Missing,
    Unsupported,
}

pub fn source_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::Normal(part) => normalized.push(part),
            _ => anyhow::bail!(
                "source must be a repository-relative path: {}",
                path.display()
            ),
        }
    }
    ensure!(
        !normalized.as_os_str().is_empty(),
        "select a specific file or subdirectory"
    );
    ensure!(
        normalized.to_str().is_some() && !excluded(&normalized),
        "unsupported source path: {}",
        path.display()
    );
    Ok(normalized)
}

pub(crate) fn excluded(path: &Path) -> bool {
    path.components().any(|part| {
        [".git", ".nunc", "target", "node_modules", ".venv"]
            .iter()
            .any(|name| {
                part.as_os_str()
                    .as_encoded_bytes()
                    .eq_ignore_ascii_case(name.as_bytes())
            })
    })
}

fn normalize_sources(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let paths = paths
        .iter()
        .map(|path| source_path(path))
        .collect::<Result<BTreeSet<_>>>()?;
    Ok(paths.into_iter().collect())
}

fn git_root(cwd: &Path) -> Result<Option<PathBuf>> {
    let output = match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("run git rev-parse"),
    };
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("not a git repository") || error.contains("must be run in a work tree") {
            return Ok(None);
        }
        anyhow::bail!("discover Git worktree: {}", error.trim());
    }
    let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let path = Path::new(OsStr::from_bytes(bytes));
    Ok(Some(path.canonicalize().context("resolve Git worktree")?))
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn prospective_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "expected an absolute data path: {}",
        path.display()
    );
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "data path cannot contain '..': {}",
        path.display()
    );
    match path.canonicalize() {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(error).with_context(|| format!("resolve {}", path.display()));
            }
            let parent = path
                .parent()
                .context("data path has no existing ancestor")?;
            Ok(prospective_path(parent)?
                .join(path.file_name().context("missing data directory name")?))
        }
        Err(error) => Err(error).with_context(|| format!("resolve {}", path.display())),
    }
}
