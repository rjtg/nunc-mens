use crate::index::{Index, Refresh};
use crate::model::SharedModel;
use crate::repo::Repo;
use anyhow::{Context, Result, anyhow};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn during_capture(
    repo: &Repo,
    model: &SharedModel,
    capture: impl FnOnce() -> Result<()>,
) -> Result<()> {
    thread::scope(|scope| {
        let (stop, receiver) = mpsc::channel::<()>();
        let worker = thread::Builder::new()
            .name("nunc-index".into())
            .spawn_scoped(scope, move || watch(repo, model, receiver))
            .context("start index watcher")?;

        let captured = capture();
        drop(stop);
        let stopped = worker
            .join()
            .map_err(|_| anyhow!("index watcher terminated unexpectedly"));
        captured.and(stopped)
    })
}

fn watch(repo: &Repo, model: &SharedModel, stop: Receiver<()>) {
    let mut last_error = None;
    loop {
        match model.with(repo, |model| Index::for_repo(repo)?.refresh(model, false)) {
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
