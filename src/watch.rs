use crate::index::{Index, Refresh};
use crate::model::{self, LocalModel};
use crate::repo::Repo;
use anyhow::{Context, Result, anyhow};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn during_capture(repo: &Repo, capture: impl FnOnce() -> Result<()>) -> Result<()> {
    thread::scope(|scope| {
        let (stop, receiver) = mpsc::channel::<()>();
        let worker = thread::Builder::new()
            .name("nunc-index".into())
            .spawn_scoped(scope, move || watch(repo, receiver))
            .context("start index watcher")?;

        let captured = capture();
        drop(stop);
        let stopped = worker
            .join()
            .map_err(|_| anyhow!("index watcher terminated unexpectedly"));
        captured.and(stopped)
    })
}

fn watch(repo: &Repo, stop: Receiver<()>) {
    let mut model = None;
    let mut last_error = None;
    loop {
        match refresh(repo, &mut model) {
            Ok(Refresh {
                documents,
                embedded,
                deleted,
            }) => {
                if embedded + deleted > 0 || last_error.is_some() {
                    eprintln!(
                        "Indexed {documents} chunks ({embedded} embedded, {deleted} deleted)"
                    );
                }
                last_error = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if last_error.as_ref() != Some(&message) {
                    eprintln!("nunc: indexing pending: {message}");
                    last_error = Some(message);
                }
            }
        }
        if stop.recv_timeout(INTERVAL) != Err(RecvTimeoutError::Timeout) {
            break;
        }
    }
}

fn refresh(repo: &Repo, cached: &mut Option<LocalModel>) -> Result<Refresh> {
    if let Err(error) = model::check_cache(repo) {
        *cached = None;
        return Err(error);
    }
    if cached.is_none() {
        *cached = Some(LocalModel::load(repo)?);
    }
    let model = cached.as_mut().context("no cached ONNX session")?;
    Index::for_repo(repo)?.refresh(model, false)
}
