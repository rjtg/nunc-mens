use crate::ledger::Guard;
use crate::repo::Repo;
use anyhow::{Context, Result};
use similar::{Algorithm, DiffOp, capture_diff_slices, group_diff_ops, udiff::UnifiedHunkHeader};
use std::io::{self, Write};
use uuid::Uuid;

pub fn compare(repo: &Repo, old: Uuid, new: Uuid, out: &mut impl Write) -> Result<bool> {
    let guard = Guard::acquire(repo, false)?;
    let old_entry = guard.ledger.read_entry(repo, old)?;
    let new_entry = guard.ledger.read_entry(repo, new)?;
    if old_entry.body == new_entry.body {
        return Ok(false);
    }
    write_diff(old, new, &old_entry.body, &new_entry.body, out)
        .and_then(|()| out.flush())
        .context("write Markdown diff")?;
    Ok(true)
}

fn write_diff(
    old_id: Uuid,
    new_id: Uuid,
    old: &str,
    new: &str,
    out: &mut impl Write,
) -> io::Result<()> {
    let old: Vec<_> = old.split_inclusive('\n').collect();
    let new: Vec<_> = new.split_inclusive('\n').collect();
    let ops = line_changes(&old, &new);
    writeln!(out, "--- a/{old_id}.md\n+++ b/{new_id}.md")?;
    for hunk in group_diff_ops(ops, 3) {
        writeln!(out, "{}", UnifiedHunkHeader::new(&hunk))?;
        for change in hunk.iter().flat_map(|op| op.iter_changes(&old, &new)) {
            let line = change.value();
            write!(out, "{}{line}", change.tag())?;
            if !line.ends_with('\n') {
                writeln!(out, "\n\\ No newline at end of file")?;
            }
        }
    }
    Ok(())
}

fn line_changes(old: &[&str], new: &[&str]) -> Vec<DiffOp> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_len = old.len() - prefix - suffix;
    let new_len = new.len() - prefix - suffix;
    // Bound the quadratic worst case without making output depend on timing.
    if old_len + new_len <= 8192 || old_len == 0 || new_len == 0 {
        return capture_diff_slices(Algorithm::Myers, old, new);
    }

    let mut ops = Vec::new();
    if prefix != 0 {
        ops.push(DiffOp::Equal {
            old_index: 0,
            new_index: 0,
            len: prefix,
        });
    }
    ops.push(DiffOp::Replace {
        old_index: prefix,
        old_len,
        new_index: prefix,
        new_len,
    });
    if suffix != 0 {
        ops.push(DiffOp::Equal {
            old_index: old.len() - suffix,
            new_index: new.len() - suffix,
            len: suffix,
        });
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_changed_region_keeps_context_and_correct_hunk_ranges() -> Result<()> {
        let old = format!("head\nprior\n{}", "one\n".repeat(9_000));
        let new = format!("head\nprior\n{}", "two\n".repeat(9_001));
        let old = old + "tail\n";
        let new = new + "tail\n";
        let mut patch = Vec::new();
        write_diff(Uuid::nil(), Uuid::nil(), &old, &new, &mut patch)?;
        let patch = String::from_utf8(patch)?;
        assert!(patch.contains("@@ -1,9003 +1,9004 @@\n head\n prior\n"));
        assert_eq!(patch.lines().filter(|line| *line == "-one").count(), 9_000);
        assert_eq!(patch.lines().filter(|line| *line == "+two").count(), 9_001);
        assert!(patch.ends_with("\n tail\n"));
        Ok(())
    }
}
