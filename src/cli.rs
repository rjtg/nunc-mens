use crate::documents::{Kind, Snapshot};
use crate::duplicates::{self, DEFAULT_THRESHOLD};
use crate::entries::{self, ContextFields, MAX_BODY};
use crate::fifo::{self, Delivery};
use crate::git;
use crate::index::Index;
use crate::ledger::{self, Guard, Ledger};
use crate::model::{self, LocalModel, MODEL};
use crate::repo::Repo;
use crate::search::{self, ContextMode, Sort};
use crate::timeline::{self, DatedEvent, Event, EventKind, NeighborReport, TimeFilter};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Local, NaiveDate, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use uuid::Uuid;

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
    Write {
        #[command(flatten)]
        input: Capture,
        #[arg(long)]
        check_duplicates: bool,
        #[arg(long, requires = "check_duplicates")]
        threshold: Option<f64>,
    },
    /// Compare normalized content and semantic embeddings without changing entries.
    Duplicates {
        id: Option<String>,
        #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
        threshold: f64,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long)]
        exact: bool,
        /// Print verified exact duplicate IDs for explicit cleanup.
        #[arg(long, requires = "exact", conflicts_with_all = ["json", "raw"])]
        forget_ids: bool,
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        json: bool,
    },
    /// Compare two Markdown memories. Exit 1 if they differ.
    Diff { old: Uuid, new: Uuid },
    /// Refresh the semantic index from Markdown and registered sources.
    Reindex {
        /// Build a fresh database and publish it after successful inference.
        #[arg(long)]
        full: bool,
        #[arg(long)]
        source: Vec<PathBuf>,
    },
    /// Search by semantic similarity or list recorded activities by date.
    Search {
        #[arg(required_unless_present_any = ["yesterday", "date", "since", "from", "to"], num_args = 1..)]
        query: Vec<String>,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long)]
        json: bool,
        /// Print ranked parent IDs, one per line.
        #[arg(long, conflicts_with_all = ["json", "explain"])]
        ids: bool,
        /// Render one source-attributed Markdown document.
        #[arg(long, value_enum, conflicts_with_all = ["json", "ids"])]
        format: Option<SearchFormat>,
        /// Cap estimated context tokens, including per-result provenance.
        #[arg(long, conflicts_with = "ids", value_parser = clap::value_parser!(u32).range(1..))]
        max_tokens: Option<u32>,
        #[arg(long = "type")]
        kind: Option<Kind>,
        #[arg(long)]
        source: Vec<PathBuf>,
        /// Restrict results to this exact registered source file. Repeat to add files.
        #[arg(long)]
        source_file: Vec<PathBuf>,
        #[arg(long, value_enum, default_value = "chunk")]
        context: ContextMode,
        #[arg(long, value_enum, default_value = "semantic")]
        sort: Sort,
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "0.05")]
        recency_boost: Option<f64>,
        #[arg(long)]
        explain: bool,
        #[arg(long)]
        raw: bool,
        #[command(flatten)]
        dates: DateArgs,
        /// Find recorded activities around the semantic matches.
        #[arg(long, value_enum, requires = "query", conflicts_with_all = ["ids", "format", "max_tokens"])]
        related_by: Option<RelatedBy>,
        /// Radius around each match, such as 2h.
        #[arg(long, requires = "related_by")]
        window: Option<String>,
    },
    /// List recorded memory and Git events in chronological order.
    Timeline {
        #[command(flatten)]
        dates: DateArgs,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long, value_enum)]
        entry_kind: Option<EventKind>,
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, conflicts_with = "json")]
        ids: bool,
        #[arg(long, value_enum, conflicts_with = "around")]
        group_by: Option<TimelineGroup>,
        #[arg(long)]
        around: Option<Uuid>,
        #[arg(long, requires = "around")]
        window: Option<String>,
    },
    /// Import Git commits and manage the optional local post-commit hook.
    Git {
        #[command(subcommand)]
        command: GitAction,
    },
    /// Find candidates, apply an external summary, or undo an archive operation.
    Consolidate {
        #[command(subcommand)]
        command: Consolidation,
    },
    /// Hide selected memories while keeping their original Markdown.
    Archive(ArchiveArgs),
    /// Restore manually archived memories to ordinary retrieval.
    Unarchive(ArchiveArgs),
    /// Permanently remove only the explicitly named memory entries.
    Forget {
        #[arg(required = true)]
        entries: Vec<Uuid>,
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage the local embedding model.
    Models {
        #[command(subcommand)]
        command: ModelAction,
    },
    /// Show repository and data paths.
    Status,
    /// Check local configuration and prerequisites.
    Doctor {
        #[arg(long)]
        deep: bool,
        #[arg(long, requires = "deep")]
        json: bool,
    },
    /// Inspect and reconcile the disposable search index.
    Repair {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SearchFormat {
    Markdown,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum RelatedBy {
    Time,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TimelineGroup {
    Day,
}

#[derive(Args)]
struct DateArgs {
    /// Use yesterday's date in the local timezone.
    #[arg(long, conflicts_with_all = ["date", "since", "from", "to"])]
    yesterday: bool,
    /// Select a local calendar day (YYYY-MM-DD).
    #[arg(long, conflicts_with_all = ["since", "from", "to"])]
    date: Option<NaiveDate>,
    /// Select a trailing duration such as 7d or 2h.
    #[arg(long, conflicts_with_all = ["from", "to"])]
    since: Option<String>,
    /// Start at this local calendar date or RFC 3339 instant.
    #[arg(long)]
    from: Option<String>,
    /// Include this local calendar date or stop before this RFC 3339 instant.
    #[arg(long)]
    to: Option<String>,
}

impl DateArgs {
    fn filter(&self, now: DateTime<Utc>) -> Result<TimeFilter> {
        if self.yesterday {
            TimeFilter::yesterday(now.with_timezone(&Local).date_naive())
        } else if let Some(date) = self.date {
            TimeFilter::day(date)
        } else if let Some(since) = &self.since {
            TimeFilter::since(now, since)
        } else {
            TimeFilter::between(self.from.as_deref(), self.to.as_deref())
        }
    }
}

#[derive(Args)]
struct ArchiveArgs {
    #[arg(required = true)]
    entries: Vec<Uuid>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum GitAction {
    Ingest {
        revision: String,
    },
    Import {
        #[arg(long, conflicts_with = "range")]
        all: bool,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        limit: Option<u32>,
        #[arg(long, value_name = "YYYY-MM-DD")]
        since: Option<chrono::NaiveDate>,
        #[arg(long, value_name = "REV..REV")]
        range: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    Sync,
    Hook {
        #[command(subcommand)]
        command: HookAction,
    },
}

#[derive(Subcommand)]
enum HookAction {
    Install,
    Status,
    Uninstall,
}

#[derive(Subcommand)]
enum Consolidation {
    Candidates {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    #[command(group(clap::ArgGroup::new("summary_input").required(true).args(["summary", "summary_from_file", "summary_stdin"])))]
    Apply {
        #[arg(long, required = true, value_delimiter = ',')]
        entries: Vec<Uuid>,
        #[arg(long, value_name = "ID=HASH")]
        revision: Vec<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        summary_from_file: Option<PathBuf>,
        #[arg(long)]
        summary_stdin: bool,
        #[arg(long)]
        json: bool,
    },
    Undo {
        id: Uuid,
        #[arg(long)]
        json: bool,
    },
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
        input_stdin()
    }
}

fn input_stdin() -> Result<String> {
    let stdin = io::stdin();
    ensure!(!stdin.is_terminal(), "pipe text into stdin");
    let mut bytes = Vec::new();
    stdin
        .lock()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read text from stdin")?;
    ensure!(bytes.len() <= MAX_BODY, "input exceeds {MAX_BODY} bytes");
    String::from_utf8(bytes).context("input must be UTF-8")
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
    let error_status = if matches!(cli.command, Action::Diff { .. }) {
        ExitCode::from(2)
    } else {
        ExitCode::FAILURE
    };
    match execute(cli.command) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("nunc: {error:#}");
            error_status
        }
    }
}

fn execute(action: Action) -> Result<ExitCode> {
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
        Action::Write {
            input,
            check_duplicates,
            threshold,
        } => {
            let body = input.body()?;
            if check_duplicates {
                let threshold = threshold.unwrap_or(DEFAULT_THRESHOLD);
                ensure!(
                    (0.0..=1.0).contains(&threshold),
                    "duplicate threshold must be between 0 and 1"
                );
                match duplicates::check_write(&repo, &body, threshold) {
                    Ok(report) => print_duplicates(report, &mut io::stderr().lock())?,
                    Err(error) => eprintln!("nunc: duplicate check unavailable: {error:#}"),
                }
            }
            match fifo::write(&repo, &body)? {
                Delivery::Queued => println!("Queued for FIFO delivery"),
                Delivery::Saved(receipt) => print_receipt(receipt),
            }
        }
        Action::Duplicates {
            id,
            threshold,
            limit,
            exact,
            forget_ids,
            raw,
            json,
        } => {
            let report =
                duplicates::find(&repo, id.as_deref(), threshold, limit as usize, exact, raw)?;
            let mut out = io::stdout().lock();
            if json {
                serde_json::to_writer(&mut out, &report)?;
                writeln!(out)?;
            } else if forget_ids {
                for warning in report.warnings {
                    eprintln!("nunc: {warning}");
                }
                for id in report.suggested_forget_ids {
                    writeln!(out, "{id}")?;
                }
            } else {
                print_duplicate_groups(report, &mut out)?;
            }
        }
        Action::Reindex { full, source } => {
            repo.configure(&source, false)?;
            let mut model = LocalModel::load(&repo)?;
            let report = Index::for_repo(&repo)?.refresh(&mut model, full)?;
            println!(
                "Indexed {} chunks ({} embedded, {} deleted)",
                report.documents, report.embedded, report.deleted
            );
        }
        Action::Diff { old, new } => {
            let different = crate::diff::compare(&repo, old, new, &mut io::stdout().lock())?;
            return Ok(ExitCode::from(u8::from(different)));
        }
        Action::Search {
            query,
            limit,
            json,
            ids,
            format,
            max_tokens,
            kind,
            source,
            source_file,
            context,
            sort,
            recency_boost,
            explain,
            raw,
            dates,
            related_by,
            window,
        } => {
            let markdown = matches!(format, Some(SearchFormat::Markdown));
            let options = search::Options {
                limit: limit as usize,
                kind,
                sources: &source,
                source_files: &source_file,
                context: if ids { ContextMode::Chunk } else { context },
                sort,
                recency_boost,
                explain,
                raw,
                one_per_parent: ids || markdown,
                max_tokens: max_tokens.map(|value| value as usize),
                budget_encoding: if markdown {
                    search::BudgetEncoding::Markdown
                } else {
                    search::BudgetEncoding::Json
                },
                time: dates.filter(Utc::now())?,
                neighbor_window: related_by
                    .map(|_| timeline::duration(window.as_deref().unwrap_or("2h")))
                    .transpose()?,
            };
            let mut out = io::stdout().lock();
            if query.is_empty() {
                ensure!(
                    options.time.restricted(),
                    "provide a query or a date filter"
                );
                ensure!(
                    !markdown
                        && !explain
                        && max_tokens.is_none()
                        && recency_boost.is_none()
                        && !matches!(sort, Sort::Recent)
                        && matches!(context, ContextMode::Chunk),
                    "context, formatting, score and budget options require a semantic query"
                );
                let events = search::dated(&repo, &options)?;
                print_events(events, limit as usize, json, ids, false, &mut out)?;
                return Ok(ExitCode::SUCCESS);
            }
            let found = search::search(&repo, &query.join(" "), options)?;
            if json {
                serde_json::to_writer(&mut out, &found)?;
                writeln!(out)?;
            } else {
                for warning in found.warnings {
                    eprintln!("nunc: {warning}");
                }
                if markdown {
                    for hit in &found.results {
                        search::markdown::write_hit(&mut out, hit)?;
                    }
                    print_context_usage(found.budget.as_ref());
                    return Ok(ExitCode::SUCCESS);
                }
                if ids {
                    for hit in found.results {
                        let id = hit
                            .source
                            .matched_chunk
                            .as_ref()
                            .map_or(hit.source.id.as_str(), |chunk| chunk.parent_id.as_str());
                        writeln!(out, "{id}")?;
                    }
                    return Ok(ExitCode::SUCCESS);
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
                    let (first, last) = hit
                        .context_window
                        .as_ref()
                        .map_or((p.line_start, p.line_end), |window| {
                            (Some(window.line_start), Some(window.line_end))
                        });
                    if let (Some(first), Some(last)) = (first, last) {
                        write!(out, ":{first}-{last}")?;
                    }
                    if let Some(visibility) = p.visibility {
                        write!(out, " {}", visibility.status)?;
                        if let Some(id) = visibility.archived_into {
                            write!(out, " → {id}")?;
                        }
                        if explain && !visibility.sources.is_empty() {
                            write!(out, " from {}", serde_json::to_string(&visibility.sources)?)?;
                        }
                    }
                    if let Some(timestamp) = p.timestamp {
                        write!(out, " {timestamp}")?;
                    }
                    if let Some(commit) = p.git {
                        write!(out, " commit {}", commit.sha)?;
                    }
                    writeln!(out, " [{}]\n{}\n", p.id, hit.text)?;
                    if hit.context_window.is_some_and(|window| window.truncated) {
                        writeln!(out, "   Context shortened to fit the budget.")?;
                    }
                    if let Some(explanation) = hit.explanation {
                        writeln!(
                            out,
                            "   cosine {:.4} + recency {:.4} = {:.4}; model {} @ {}; {:?} chunks, {:?} order, {:?} context",
                            explanation.similarity,
                            explanation.recency_contribution,
                            explanation.final_score,
                            explanation.model.id,
                            explanation.model.revision,
                            explanation.chunking,
                            explanation.sort,
                            explanation.context
                        )?;
                        if let Some(chunk) = p.matched_chunk {
                            writeln!(
                                out,
                                "   matched {} in {} at lines {}–{} {}",
                                chunk.id,
                                chunk.parent_id,
                                chunk.line_start,
                                chunk.line_end,
                                chunk.heading_path.join(" / ")
                            )?;
                        }
                    }
                }
                print_context_usage(found.budget.as_ref());
                if let Some(report) = &found.temporal_context {
                    print_neighbors(report, &mut out)?;
                }
            }
        }
        Action::Timeline {
            dates,
            limit,
            entry_kind,
            raw,
            json,
            ids,
            group_by,
            around,
            window,
        } => {
            let guard = Guard::acquire(&repo, false)?;
            let events = timeline::events(
                &repo,
                &guard.ledger,
                raw,
                dates.filter(Utc::now())?,
                entry_kind,
            )?;
            let mut out = io::stdout().lock();
            if let Some(id) = around {
                let archives = guard.ledger.archives()?;
                ensure!(!guard.ledger.forgotten(id), "entry {id} was forgotten");
                ensure!(
                    raw || !guard.ledger.hidden(&archives, id),
                    "anchor {id} is archived; use --raw to include it"
                );
                let anchor = guard.ledger.read_entry(&repo, id)?;
                let timestamp = anchor.metadata.timestamp;
                let path = entries::path(&repo, id);
                let path = path
                    .strip_prefix(&repo.root)
                    .context("anchor path is outside the repository")?;
                let window = timeline::duration(window.as_deref().unwrap_or("2h"))?;
                let report = timeline::neighbors(
                    events,
                    &[(id.to_string(), timestamp.to_utc())],
                    window,
                    limit as usize,
                );
                if json {
                    serde_json::to_writer(
                        &mut out,
                        &serde_json::json!({
                            "anchor": {
                                "id": id, "timestamp": timestamp, "path": path,
                                "entry_kind": anchor.metadata.kind, "git": anchor.metadata.commit,
                            },
                            "temporal_context": report,
                        }),
                    )?;
                    writeln!(out)?;
                } else if ids {
                    for neighbor in report.activities {
                        writeln!(out, "{}", neighbor.event.source.id)?;
                    }
                } else {
                    writeln!(out, "Anchor {id} at {timestamp} {}", path.display())?;
                    print_neighbors(&report, &mut out)?;
                }
            } else {
                print_events(
                    events,
                    limit as usize,
                    json,
                    ids,
                    group_by.is_some(),
                    &mut out,
                )?;
            }
        }
        Action::Git { command } => git_action(&repo, command)?,
        Action::Consolidate { command } => consolidate(&repo, command)?,
        Action::Archive(args) => archive(&repo, args, true)?,
        Action::Unarchive(args) => archive(&repo, args, false)?,
        Action::Forget {
            entries,
            dry_run,
            yes,
            json,
        } => forget(&repo, &entries, dry_run, yes, json)?,
        Action::Models {
            command: ModelAction::Download,
        } => {
            println!("Model available at {}", model::download(&repo)?.display());
        }
        Action::Status => status(&repo, false)?,
        Action::Doctor { deep: false, .. } => status(&repo, true)?,
        Action::Doctor { deep: true, json } => {
            crate::index::health::run(&repo, false, false)?
                .print(&mut io::stdout().lock(), json)?;
        }
        Action::Repair { dry_run, json } => {
            crate::index::health::run(&repo, true, dry_run)?
                .print(&mut io::stdout().lock(), json)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_context_usage(budget: Option<&search::BudgetUsage>) {
    if let Some(budget) = budget {
        eprintln!(
            "nunc: context budget used {}/{} estimated tokens ({} shortened, {} omitted)",
            budget.estimated_tokens,
            budget.max_tokens,
            budget.truncated_results,
            budget.omitted_candidates,
        );
    }
}

fn print_events(
    events: Vec<DatedEvent>,
    limit: usize,
    json: bool,
    ids: bool,
    group_by_day: bool,
    out: &mut impl Write,
) -> Result<()> {
    let skip = events.len().saturating_sub(limit);
    let events: Vec<_> = events
        .into_iter()
        .skip(skip)
        .map(|event| event.event)
        .collect();
    if json {
        let mut report = serde_json::json!({"mode_used": "temporal", "results": events});
        if group_by_day {
            let mut groups = BTreeMap::<_, Vec<_>>::new();
            for event in &events {
                groups
                    .entry(event.local_day)
                    .or_default()
                    .push(&event.source.id);
            }
            report["days"] = serde_json::to_value(groups)?;
        }
        serde_json::to_writer(&mut *out, &report)?;
        writeln!(out)?;
        return Ok(());
    }
    let mut day = None;
    for event in &events {
        if ids {
            writeln!(out, "{}", event.source.id)?;
            continue;
        }
        if group_by_day && day != Some(event.local_day) {
            writeln!(out, "{}", event.local_day)?;
            day = Some(event.local_day);
        }
        print_event(event, out)?;
    }
    if events.is_empty() && !ids {
        writeln!(out, "No recorded activities")?;
    }
    Ok(())
}

fn print_event(event: &Event, out: &mut impl Write) -> Result<()> {
    let source = &event.source;
    let kind = match source.entry_kind {
        Some(entries::EntryKind::GitCommit) => "git-commit",
        Some(entries::EntryKind::Consolidation) => "consolidation",
        _ => "memory",
    };
    write!(
        out,
        "{} {} {} [{}]",
        source.timestamp.as_deref().unwrap_or(""),
        kind,
        source.path,
        source.id
    )?;
    if let Some(commit) = &source.git {
        write!(out, " commit {}", commit.sha)?;
    }
    if let Some(visibility) = &source.visibility {
        write!(out, " {}", visibility.status)?;
    }
    writeln!(out, " {}", event.title)?;
    Ok(())
}

fn print_neighbors(report: &NeighborReport, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "\nOther recorded activities within {} seconds of an anchor (time proximity)",
        report.window_seconds
    )?;
    for neighbor in &report.activities {
        print_event(&neighbor.event, out)?;
        for relation in &neighbor.anchors {
            writeln!(
                out,
                "  {:+} ms from {}",
                relation.delta_millis, relation.anchor_id
            )?;
        }
    }
    if report.activities.is_empty() {
        writeln!(out, "No neighboring activities")?;
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
    match fifo::status(repo)? {
        Some((pid, boundary)) => println!("Server: running (PID {pid}, boundary {boundary})"),
        None => println!("Server: stopped"),
    }
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

fn print_duplicates(report: duplicates::Report, out: &mut impl Write) -> Result<()> {
    for warning in report.warnings {
        writeln!(out, "nunc: {warning}")?;
    }
    for pair in report.matches {
        writeln!(
            out,
            "{} {} {:.4}",
            pair.detection, pair.scope, pair.similarity
        )?;
        for item in [pair.left, pair.right] {
            writeln!(out, "  {} {} {}", item.id, item.path, item.excerpt)?;
        }
    }
    Ok(())
}

fn print_duplicate_groups(report: duplicates::Report, out: &mut impl Write) -> Result<()> {
    for warning in report.warnings {
        eprintln!("nunc: {warning}");
    }
    for group in report.groups {
        let exact = group.detection == "exact";
        if let Some(hash) = group.normalized_hash {
            writeln!(out, "{} group (normalized SHA-256 {hash})", group.detection)?;
        } else {
            writeln!(out, "semantic group (review only)")?;
        }
        print_duplicate_item(
            out,
            if exact { "KEEP" } else { "REFERENCE" },
            &group.reference,
        )?;
        for member in group.members {
            let label = if group.suggested_forget_ids.contains(&member.item.id) {
                "DUPLICATE"
            } else if exact {
                "REVIEW"
            } else {
                "POSSIBLE"
            };
            let mut relation = format!(
                "{label} ({}, {:.4} to {})",
                member.scope, member.similarity, group.reference_id
            );
            if let Some(chunk) = member.reference_chunk {
                relation.push_str(&format!(
                    " [reference lines {}–{}]",
                    chunk.line_start, chunk.line_end
                ));
            }
            if member.byte_identical == Some(false) {
                relation.push_str(" [normalized text only]");
            }
            print_duplicate_item(out, &relation, &member.item)?;
        }
    }
    Ok(())
}

fn print_duplicate_item(out: &mut impl Write, label: &str, item: &duplicates::Item) -> Result<()> {
    write!(out, "  {label} {} {}", item.id, item.path)?;
    if let Some(chunk) = &item.chunk {
        write!(out, ":{}-{}", chunk.line_start, chunk.line_end)?;
    }
    if let Some(timestamp) = &item.timestamp {
        write!(out, " [{timestamp}]")?;
    }
    writeln!(
        out,
        " {}",
        item.excerpt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    )?;
    Ok(())
}

fn consolidate(repo: &Repo, command: Consolidation) -> Result<()> {
    match command {
        Consolidation::Candidates { query, limit, json } => {
            let found = search::search(
                repo,
                &query.join(" "),
                search::Options {
                    limit: limit as usize,
                    kind: Some(Kind::Memory),
                    sources: &[],
                    source_files: &[],
                    context: ContextMode::Parent,
                    sort: Sort::Semantic,
                    recency_boost: Some(0.0),
                    explain: false,
                    raw: false,
                    one_per_parent: true,
                    max_tokens: None,
                    budget_encoding: search::BudgetEncoding::Json,
                    time: TimeFilter::default(),
                    neighbor_window: None,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string(&found)?);
            } else {
                for warning in found.warnings {
                    eprintln!("nunc: {warning}");
                }
                for hit in found.results {
                    let chunk = hit
                        .source
                        .matched_chunk
                        .context("missing candidate revision")?;
                    println!(
                        "{} {:.4} {} {}\n{}\n",
                        hit.source.id, hit.score, hit.source.path, chunk.parent_hash, hit.text
                    );
                }
            }
        }
        Consolidation::Apply {
            entries,
            revision,
            summary,
            summary_from_file,
            summary_stdin,
            json,
        } => {
            let body = if let Some(body) = summary {
                body
            } else if let Some(path) = summary_from_file {
                String::from_utf8(crate::fs::read_regular(&path, MAX_BODY)?)?
            } else {
                ensure!(summary_stdin, "select a summary input");
                input_stdin()?
            };
            let mut expected = BTreeMap::new();
            for item in revision {
                let (id, hash) = item
                    .split_once('=')
                    .context("revision must have form ID=HASH")?;
                ensure!(
                    expected.insert(id.parse()?, hash.to_owned()).is_none(),
                    "duplicate revision precondition"
                );
            }
            let receipt = Guard::acquire(repo, true)?.apply(&entries, &expected, &body)?;
            refresh_pending(repo);
            print_apply(&receipt, json)?;
        }
        Consolidation::Undo { id, json } => {
            let receipt = Guard::acquire(repo, true)?.undo(id)?;
            refresh_pending(repo);
            print_apply(&receipt, json)?;
        }
    }
    Ok(())
}

fn print_apply(receipt: &ledger::Applied, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(receipt)?);
    } else {
        println!("{} {}", receipt.status, receipt.summary_id);
        for id in &receipt.archived_entries {
            println!("  {id}");
        }
    }
    Ok(())
}

fn refresh_pending(repo: &Repo) {
    if let Err(error) = LocalModel::load(repo)
        .and_then(|mut model| Index::for_repo(repo)?.refresh(&mut model, false))
    {
        eprintln!("nunc: durable change saved; indexing pending: {error:#}");
    }
}

const INLINE_GIT_REFRESH_LIMIT: usize = 256;

const FORGET_WARNING: &str = "Surviving summaries, Git history, backups, and storage remnants may still contain this information.";

fn forget(repo: &Repo, ids: &[Uuid], dry_run: bool, yes: bool, json: bool) -> Result<()> {
    if dry_run {
        return print_forget(
            &Ledger::read(repo)?.forget_plan(repo, ids)?,
            "dry-run",
            json,
        );
    }
    let mut guard = Guard::acquire(repo, true)?;
    let plan = guard.ledger.forget_plan(repo, ids)?;
    let interactive = !yes && !plan.targets.is_empty();
    if interactive {
        ensure!(
            io::stdin().is_terminal(),
            "forget requires an interactive confirmation or --yes"
        );
        eprint!("{}", format_forget(&plan, "confirmation required"));
        eprint!("Type yes to forget these entries: ");
        io::stderr().flush()?;
        let mut response = String::new();
        io::stdin().read_line(&mut response)?;
        ensure!(
            response.trim().eq_ignore_ascii_case("yes"),
            "forget cancelled"
        );
    }
    if !interactive {
        if json && !plan.targets.is_empty() {
            eprint!("{}", format_forget(&plan, "forget"));
        } else if !json {
            print!("{}", format_forget(&plan, "forget"));
        }
    }
    guard.forget(&plan)?;
    Index::for_repo(repo)?
        .purge(guard.forgotten_ids())
        .context("forget committed; retry forget or run nunc reindex --full to clean the index")?;
    if json {
        print_forget(&plan, "forgotten", true)?;
    } else {
        println!("Forgot {} entries", plan.targets.len());
    }
    Ok(())
}

fn print_forget(plan: &ledger::ForgetPlan, status: &str, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({"status": status, "targets": plan.targets,
            "already_forgotten": plan.already_forgotten, "dependencies": plan.dependencies,
            "warnings": [FORGET_WARNING]})
        );
    } else {
        print!("{}", format_forget(plan, status));
    }
    Ok(())
}

fn format_forget(plan: &ledger::ForgetPlan, status: &str) -> String {
    let mut out = format!("Forget {status}\n");
    for id in &plan.targets {
        out.push_str(&format!("  {id}\n"));
    }
    for id in &plan.already_forgotten {
        out.push_str(&format!("  {id} already forgotten\n"));
    }
    for dep in &plan.dependencies {
        let kind = if dep.direct { "direct" } else { "transitive" };
        out.push_str(&format!(
            "  affected summary {} ({kind} dependency)\n",
            dep.summary_id
        ));
    }
    out.push_str(FORGET_WARNING);
    out.push('\n');
    out
}

fn git_action(repo: &Repo, command: GitAction) -> Result<()> {
    let (counts, dry_run) = match command {
        GitAction::Ingest { revision } => (git::ingest(repo, &revision)?, false),
        GitAction::Sync => (git::import(repo, git::Selection::default())?, false),
        GitAction::Import {
            all,
            limit,
            since,
            range,
            dry_run,
        } => (
            git::import(
                repo,
                git::Selection {
                    all,
                    limit,
                    since,
                    range,
                    dry_run,
                },
            )?,
            dry_run,
        ),
        GitAction::Hook { command } => {
            let path = match command {
                HookAction::Install => git::change_hook(repo, true)?,
                HookAction::Uninstall => git::change_hook(repo, false)?,
                HookAction::Status => git::hook_path(repo)?,
            };
            println!(
                "Post-commit hook {} at {}",
                git::hook_status(repo, &path)?.name(),
                path.display()
            );
            return Ok(());
        }
    };
    let verb = if dry_run { "would import" } else { "saved" };
    println!(
        "{} selected, {} {verb}, {} existing, {} forgotten",
        counts.selected, counts.saved, counts.existing, counts.forgotten
    );
    if !dry_run && counts.selected != 0 {
        if counts.selected <= INLINE_GIT_REFRESH_LIMIT {
            refresh_pending(repo);
        } else {
            eprintln!(
                "nunc: history saved; run nunc reindex or let nunc serve index the bulk import"
            );
        }
    }
    Ok(())
}

fn archive(repo: &Repo, args: ArchiveArgs, hide: bool) -> Result<()> {
    let report = if args.dry_run {
        let ledger = Ledger::read(repo)?;
        ledger.archive_plan(repo, &args.entries, hide, true)?
    } else {
        Guard::acquire(repo, true)?.archive(&args.entries, hide)?
    };
    let mut output = io::stdout().lock();
    if args.json {
        serde_json::to_writer(&mut output, &report)?;
        writeln!(output)?;
    } else {
        let qualifier = if report.dry_run { " (dry run)" } else { "" };
        writeln!(
            output,
            "{}{}: {} changed, {} unchanged",
            report.operation,
            qualifier,
            report.changed.len(),
            report.unchanged.len()
        )?;
        for id in &report.changed {
            writeln!(output, "  {id}")?;
        }
        for id in &report.unchanged {
            writeln!(output, "  {id} unchanged")?;
        }
    }
    Ok(())
}
