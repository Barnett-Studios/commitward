//! Parsers that turn `git diff` output into the pure inputs `detect` consumes.
//!
//! `parse_name_status` reads `git diff --name-status`; `parse_added_lines` reads
//! unified `git diff`. Both are total (never panic) on arbitrary input — the CLI
//! feeds them subprocess output and degrades fail-open on anything odd.
//!
//! NOTE: the CLI (`main.rs`) always shells `--no-renames` (a rename surfaces as
//! delete-old + add-new so a path guard on the *old* name still fires — the
//! rename-evasion defense). So `parse_name_status` sees only two-field
//! `STATUS\tPATH` lines in practice; it also handles rename (`R100\told\tnew`)
//! lines defensively, but the shipped binary never produces them.

use crate::FileEntry;
use std::collections::HashMap;

/// Decode git's C-style path quoting back to the real path (commitward#3).
///
/// Git quotes a path whenever it contains a `"`, a backslash, or a control character —
/// **regardless of `core.quotePath`**, which only governs bytes ≥ 0x80. So
/// `core.quotePath=false` (which the CLI sets) is enough for `café.sh` and not for
/// `back\slash.sh`, `quote".sh`, or a path with a tab in it. Measured against real git
/// output, not inferred:
///
/// ```text
/// name-status : "sub/back\\slash.sh"          +++ header : +++ "b/sub/back\\slash.sh"
/// name-status : sub/café.sh                   +++ header : +++ b/sub/café.sh
/// ```
///
/// The quoted header is the damaging half. `strip_prefix("+++ b/")` cannot match
/// `+++ "b/…`, so `current_file` became `None` and every subsequent `+` line was dropped
/// until the next header — the file's added content was never scanned against the content
/// denylist, and nothing said so. That is a false negative in a security control, not a
/// mis-keyed lookup.
///
/// Returns the input unchanged when it is not a quoted string, so a caller can apply it
/// unconditionally. Decodes to BYTES and then lossily to `String`: git emits `\NNN` octal
/// per byte, so a multi-byte character arrives as several escapes and decoding
/// escape-by-escape into a `String` would corrupt it.
pub fn unquote_c_style(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'"' || b[b.len() - 1] != b'"' {
        return s.to_string();
    }
    let inner = &b[1..b.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' {
            out.push(inner[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = inner.get(i) else {
            // Trailing backslash: not valid git output. Keep it rather than dropping a
            // byte — this parser is total on arbitrary input by contract.
            out.push(b'\\');
            break;
        };
        match c {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'0'..=b'7' => {
                // Up to three octal digits, one BYTE.
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3 {
                    match inner.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            val = val * 8 + u32::from(d - b'0');
                            i += 1;
                            n += 1;
                        }
                        _ => break,
                    }
                }
                i -= 1; // the outer `i += 1` below consumes the last digit
                out.push((val & 0xff) as u8);
            }
            // Unknown escape: git does not emit these. Keep the character itself rather
            // than guessing, so an odd input degrades to a wrong-but-present path instead
            // of a silently shortened one.
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `git diff --name-status` output into `FileEntry` rows.
///
/// Each non-empty line is tab-separated: the first field's first char is the
/// status; the path is the **last** field. In normal CLI use (`--no-renames`)
/// every line is two fields (`STATUS\tPATH`). Taking the last field also makes
/// the parser robust to a rename line `R100\told\tnew` → `{ status: 'R', path:
/// "new" }` if ever fed one, but the shipped binary does not emit renames.
/// Lines with fewer than two fields, an empty status, or an empty path are skipped.
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
        // Unquoted here and in `parse_added_lines`, so the two views key on the same
        // string. `detect`'s content arm joins them on the path, and a form mismatch there
        // is a silent skip (commitward#3).
        entries.push(FileEntry {
            status,
            path: unquote_c_style(path),
        });
    }
    entries
}

/// Parse a unified diff into added lines grouped by destination file.
///
/// Preserves the gate's exact detection behavior — in particular its
/// **hunk-state defense**: a
/// `+++ ` line is a file header only *before* the first `@@` hunk marker for a
/// file; once inside a hunk, a `+++ `-prefixed line is captured as added content.
/// This prevents an attacker from prepending a benign `++ note` line to neutralise
/// content scanning for the rest of a file's additions. `+++ /dev/null` (deletion)
/// clears the current file; a `diff --git` header resets hunk state.
/// The destination path from a `+++ ` file header, or `None` if it is not one.
///
/// Two forms, both measured against real git output:
///
/// ```text
/// +++ b/sub/plain.sh               unquoted
/// +++ b/sub/trail .sh\t            unquoted, with git's tab delimiter appended
/// +++ "b/sub/quote\".sh"           C-quoted — the `b/` is INSIDE the quotes
/// ```
///
/// The tab is stripped as EXACTLY ONE trailing `\t`, not by `trim_end()`. `trim_end()` also
/// ate a trailing space belonging to the path itself, so `sub/endswithspace ` keyed as
/// `sub/endswithspace` here and as `sub/endswithspace ` in `parse_name_status` — the desync
/// commitward#3 describes, which is real but for a narrower input than the ticket states. A
/// path containing a literal tab is C-quoted, so an unquoted header's trailing tab is
/// unambiguously git's delimiter.
fn header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("+++ ")?;
    // BEFORE the quote check, not inside the unquoted branch (commitward#26). The two things
    // that shape this header are independent: git quotes on `"`, `\` or a control character,
    // and appends the tab delimiter when the path contains a SPACE. A path with both produces
    // `+++ "b/…"\t`, where the tab sits OUTSIDE the closing quote — and `unquote_c_style`
    // returns its input unchanged unless the LAST byte is `"`, so the string came back still
    // quoted, `strip_prefix("b/")` failed on the leading quote, and every following `+` line
    // was discarded. A commit whose only change was `rm -rf /` in such a file exited 0.
    //
    // Stripping it here is safe for the quoted form for the same reason the doc above gives
    // for the unquoted one: a path containing a literal tab is C-quoted, so that tab appears
    // as the two characters `\t` INSIDE the quotes and the closing `"` is the last byte of
    // the path's own text. A trailing raw tab is therefore always git's delimiter.
    let rest = rest.strip_suffix('\t').unwrap_or(rest);
    if rest.starts_with('"') {
        let unquoted = unquote_c_style(rest);
        return unquoted.strip_prefix("b/").map(str::to_string);
    }
    rest.strip_prefix("b/").map(str::to_string)
}

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
            } else {
                current_file = header_path(line);
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
