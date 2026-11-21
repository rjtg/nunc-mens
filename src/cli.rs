use crate::documents::{Kind, Snapshot};
use crate::entries::{self, ContextFields, MAX_BODY};
use crate::fifo::{self, Delivery};
use crate::index::Index;
use crate::model::{self, LocalModel, MODEL};
use crate::repo::Repo;
use crate::search;
use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "nunc", version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Initialize local storage and optionally register source files or directories.
    Init {
        #[arg(long)]
        source: Vec<PathBuf>,
    },
    /// Save a memory directly to a durable Markdown file.
    Log {
        #[command(flatten)]
        input: Capture,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Serve the repository FIFO in the foreground.
    Serve {
        #[arg(long)]
        boundary: Option<String>,
        #[arg(long)]
        source: Vec<PathBuf>,
    },
    /// Capture through the FIFO when available, or save directly.
    Write(Capture),
    /// Refresh the semantic index from Markdown and registered sources.
    Reindex {
        /// Build a fresh database and publish it after successful inference.
        #[arg(long)]
        full: bool,
        #[arg(long)]
        source: Vec<PathBuf>,
    },
    /// Search the local index by semantic similarity.
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long)]
        json: bool,
        #[arg(long = "type")]
        kind: Option<Kind>,
        #[arg(long)]
        source: Vec<PathBuf>,
    },
    /// Manage the local embedding model.
    Models {
        #[command(subcommand)]
        command: ModelAction,
    },
    /// Show repository and data paths.
    Status,
    /// Check local configuration and prerequisites.
    Doctor,
}

#[derive(Subcommand)]
enum ModelAction {
    /// Explicitly download and validate the pinned ONNX model.
    Download,
}

#[derive(Args)]
struct Capture {
    #[arg(long, conflicts_with = "text")]
    stdin: bool,
    #[arg(required_unless_present = "stdin", num_args = 1..)]
    text: Vec<String>,
}

impl Capture {
    fn body(self) -> Result<String> {
        if !self.stdin {
            return Ok(self.text.join(" "));
        }
        let stdin = io::stdin();
        ensure!(!stdin.is_terminal(), "pipe text into --stdin");
        let mut bytes = Vec::new();
        stdin
            .lock()
            .take(MAX_BODY as u64 + 1)
            .read_to_end(&mut bytes)
            .context("read entry from stdin")?;
        ensure!(bytes.len() <= MAX_BODY, "entry exceeds {MAX_BODY} bytes");
        String::from_utf8(bytes).context("entry must be UTF-8")
    }
}

pub fn run() -> ExitCode {
    let mut args: Vec<_> = std::env::args_os().collect();
    if let Some(first) = args.get(1).and_then(|arg| arg.to_str())
        && !first.starts_with('-')
        && first != "help"
        && Cli::command().find_subcommand(first).is_none()
    {
        args.insert(1, "log".into());
    }
    let cli = Cli::parse_from(args);
    match execute(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nunc: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(action: Action) -> Result<()> {
    let repo = Repo::discover()?;
    match action {
        Action::Init { source } => {
            repo.configure(&source, true)?;
            println!("Initialized {}", repo.local().display());
        }
        Action::Log {
            input,
            tag,
            repo: label,
        } => {
            print_receipt(entries::persist(
                &repo,
                &input.body()?,
                ContextFields {
                    repo: label,
                    tags: tag,
                },
            )?);
        }
        Action::Serve { boundary, source } => fifo::serve(&repo, boundary, &source)?,
        Action::Write(input) => match fifo::write(&repo, &input.body()?)? {
            Delivery::Queued => println!("Queued for FIFO delivery"),
            Delivery::Saved(receipt) => print_receipt(receipt),
        },
        Action::Reindex { full, source } => {
            repo.configure(&source, false)?;
            let mut model = LocalModel::load(&repo)?;
            let report = Index::for_repo(&repo)?.refresh(&mut model, full)?;
            println!(
                "Indexed {} documents ({} embedded, {} deleted)",
                report.documents, report.embedded, report.deleted
            );
        }
        Action::Search {
            query,
            limit,
            json,
            kind,
            source,
        } => {
            let found = search::search(&repo, &query.join(" "), limit as usize, kind, &source)?;
            let mut out = io::stdout().lock();
            if json {
                serde_json::to_writer(&mut out, &found)?;
                writeln!(out)?;
            } else {
                for warning in found.warnings {
                    eprintln!("nunc: {warning}");
                }
                if found.results.is_empty() {
                    writeln!(out, "No results")?;
                }
                for hit in found.results {
                    let p = hit.source;
                    write!(
                        out,
                        "{}. {:.4} {} {}",
                        hit.rank,
                        hit.score,
                        p.kind.as_str(),
                        p.path
                    )?;
                    if let (Some(first), Some(last)) = (p.line_start, p.line_end) {
                        write!(out, ":{first}-{last}")?;
                    }
                    if let Some(timestamp) = p.timestamp {
                        write!(out, " {timestamp}")?;
                    }
                    writeln!(out, " [{}]\n{}\n", p.id, hit.text)?;
                }
            }
        }
        Action::Models {
            command: ModelAction::Download,
        } => {
            println!("Model available at {}", model::download(&repo)?.display());
        }
        Action::Status | Action::Doctor => status(&repo, matches!(action, Action::Doctor))?,
    }
    Ok(())
}

fn print_receipt(receipt: entries::Receipt) {
    println!("Saved {} {}", receipt.id, receipt.path.display());
}

fn status(repo: &Repo, detail: bool) -> Result<()> {
    let config = repo.config()?;
    let index = Index::for_repo(repo)?;
    println!("Repository {} ({})", repo.root.display(), repo.id);
    println!("Entries {}", repo.entries().display());
    println!("Index {}", index.path().display());
    println!("Model cache {}", repo.model_cache()?.display());
    for source in config.sources {
        println!("Source {}", source.display());
    }
    println!(
        "Model {} @ {} ({} dimensions)",
        MODEL.id, MODEL.revision, MODEL.dimensions
    );
    match model::check_cache(repo) {
        Ok(()) => println!("Model files: cached"),
        Err(error) => println!("Model files: {error}"),
    }
    match index.read(MODEL)? {
        Some(reader) if detail => {
            let snapshot = Snapshot::read(repo)?;
            let description = if reader.digest == snapshot.digest {
                "current"
            } else {
                "needs refresh"
            };
            println!(
                "Index state: {description} ({} current documents)",
                snapshot.documents.len()
            );
        }
        Some(_) => println!("Index state: present"),
        None => println!("Index state: missing (run nunc reindex after model setup)"),
    }
    Ok(())
}
