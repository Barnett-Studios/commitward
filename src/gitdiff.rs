//! Parsers that turn `git diff` output into the pure inputs `detect` consumes.
//!
//! `parse_name_status` reads `git diff --name-status -M`; `parse_added_lines`
//! reads unified `git diff`. Both are total (never panic) on arbitrary input —
//! the CLI feeds them subprocess output and degrades fail-open on anything odd.

use crate::FileEntry;
use std::collections::HashMap;

/// Parse `git diff --name-status -M` output into `FileEntry` rows.
///
/// Each non-empty line is tab-separated: the first field's first char is the
/// status; the path is the **last** field, so a rename line `R100\told\tnew`
/// yields `{ status: 'R', path: "new" }`. Lines with fewer than two fields, an
/// empty status, or an empty path are skipped.
pub fn parse_name_status(out: &str) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 2 {
            continue;
        }
        let status = match fields[0].chars().next() {
            Some(c) => c,
            None => continue,
        };
        let path = fields[fields.len() - 1];
        if path.is_empty() {
            continue;
        }
        entries.push(FileEntry {
            status,
            path: path.to_string(),
        });
    }
    entries
}

/// Parse a unified diff into added lines grouped by destination file.
///
/// Ported verbatim from the framework's `git::parse_added_lines` to preserve the
/// gate's exact detection behavior — in particular its **hunk-state defense**: a
/// `+++ ` line is a file header only *before* the first `@@` hunk marker for a
/// file; once inside a hunk, a `+++ `-prefixed line is captured as added content.
/// This prevents an attacker from prepending a benign `++ note` line to neutralise
/// content scanning for the rest of a file's additions. `+++ /dev/null` (deletion)
/// clears the current file; a `diff --git` header resets hunk state.
pub fn parse_added_lines(diff: &str) -> HashMap<String, Vec<String>> {
    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut in_hunk = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
            current_file = None;
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if line.starts_with("+++ ") {
            if in_hunk {
                // Inside a hunk: an added content line whose text begins with "++ ".
                if let Some(ref file) = current_file {
                    let stripped = line.strip_prefix('+').unwrap_or("").to_string();
                    result.entry(file.clone()).or_default().push(stripped);
                }
            } else if line == "+++ /dev/null" {
                current_file = None;
            } else if let Some(path) = line.strip_prefix("+++ b/") {
                // Trim trailing whitespace: git appends a '\t' path-boundary
                // delimiter for filenames with spaces even under core.quotePath=false.
                current_file = Some(path.trim_end().to_string());
            } else {
                current_file = None;
            }
        } else if line.starts_with('+') {
            if let Some(ref file) = current_file {
                let stripped = line.strip_prefix('+').unwrap_or("").to_string();
                result.entry(file.clone()).or_default().push(stripped);
            }
        }
    }

    result
}
