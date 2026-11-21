use crate::entries::{self, ContextFields, MAX_BODY};
use crate::fifo::{self, Delivery};
use crate::repo::Repo;
use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Read};
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
    /// Show repository and data paths.
    Status,
    /// Check local configuration and prerequisites.
    Doctor,
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
        && ![
            "init", "log", "write", "serve", "reindex", "search", "models", "status", "doctor",
            "help",
        ]
        .contains(&first)
    {
        args.insert(1, OsStr::new("log").into());
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
        Action::Status | Action::Doctor => {
            println!("Repository {} ({})", repo.root.display(), repo.id);
            println!("Entries {}", repo.entries().display());
            println!("Index {}", repo.state()?.join("index.sqlite3").display());
            println!("Model cache {}", repo.model_cache()?.display());
            for source in repo.config()?.sources {
                println!("Source {}", source.display());
            }
        }
    }
    Ok(())
}

fn print_receipt(receipt: entries::Receipt) {
    println!("Saved {} {}", receipt.id, receipt.path.display());
}
