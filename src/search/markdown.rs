use super::Match;
use std::io::{self, Write};

pub(crate) fn write_hit(out: &mut impl Write, hit: &Match) -> io::Result<()> {
    let source = &hit.source;
    writeln!(out, "## Result {}\n", hit.rank)?;
    field(out, "Path", &source.path)?;
    if let Some(chunk) = &source.matched_chunk {
        field(out, "Parent ID", &chunk.parent_id)?;
        field(out, "Matched chunk", &chunk.id)?;
        writeln!(
            out,
            "- Matched lines {}–{}",
            chunk.line_start, chunk.line_end
        )?;
        if !chunk.heading_path.is_empty() {
            field(out, "Heading", &chunk.heading_path.join(" / "))?;
        }
    } else {
        field(out, "ID", &source.id)?;
    }
    if let Some(timestamp) = &source.timestamp {
        field(out, "Timestamp", timestamp)?;
    }
    if let Some(commit) = &source.git {
        field(out, "Git commit", &commit.sha)?;
    }
    if let Some(visibility) = &source.visibility {
        field(out, "Status", visibility.status)?;
        if let Some(id) = visibility.archived_into {
            field(out, "Archived into", &id.to_string())?;
        }
    }
    if let Some(window) = hit.context_window {
        writeln!(
            out,
            "- Returned lines {}–{}",
            window.line_start, window.line_end
        )?;
        if window.truncated {
            writeln!(out, "- Context shortened to fit budget")?;
        }
    }
    if let Some(explanation) = &hit.explanation {
        writeln!(out, "- Score {}", explanation.score_summary())?;
        if let Some(model) = explanation.model {
            field(out, "Model", model.id)?;
            field(out, "Model revision", model.revision)?;
        }
    }

    // Treat each original excerpt as literal text. A longer outer fence
    // allows the excerpt to contain its own Markdown and arbitrary fences.
    let ticks = longest_run(&hit.text, b'`');
    let tildes = longest_run(&hit.text, b'~');
    let (marker, count) = if ticks < tildes {
        ("`", ticks)
    } else {
        ("~", tildes)
    };
    let fence = marker.repeat((count + 1).max(3));
    write!(out, "\n{fence}\n{}", hit.text)?;
    if !hit.text.ends_with('\n') {
        writeln!(out)?;
    }
    writeln!(out, "{fence}\n")
}

fn field(out: &mut impl Write, label: &str, value: &str) -> io::Result<()> {
    let ticks = "`".repeat(longest_run(value, b'`') + 1);
    write!(out, "- {label} {ticks} ")?;
    for ch in value.chars() {
        if ch.is_control() {
            write!(out, "{}", ch.escape_default())?;
        } else {
            write!(out, "{ch}")?;
        }
    }
    writeln!(out, " {ticks}")
}

fn longest_run(text: &str, marker: u8) -> usize {
    let mut run = 0;
    let mut longest = 0;
    for byte in text.bytes() {
        if byte == marker {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}
