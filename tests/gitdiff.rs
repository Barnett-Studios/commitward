use commitward::gitdiff::{parse_added_lines, parse_name_status};

#[test]
fn name_status_parses_status_and_path() {
    let out = "M\tsrc/a.rs\nA\tsrc/b.rs\nD\told.rs\n";
    let v = parse_name_status(out);
    assert_eq!(v.len(), 3);
    assert_eq!(v[0].status, 'M');
    assert_eq!(v[0].path, "src/a.rs");
    assert_eq!(v[1].status, 'A');
    assert_eq!(v[2].status, 'D');
}

#[test]
fn name_status_rename_takes_new_path() {
    // Parser-robustness only: the shipped CLI uses `--no-renames`, so it never
    // emits an "R100\told\tnew" line. This asserts that IF fed one, the parser
    // takes the new path — it does not imply the binary produces rename lines.
    let out = "R100\tsrc/old.rs\tsrc/new.rs\n";
    let v = parse_name_status(out);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].status, 'R');
    assert_eq!(v[0].path, "src/new.rs");
}

#[test]
fn added_lines_grouped_by_file_excluding_plusplus_header() {
    let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 keep
+added one
+added two
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -0,0 +1 @@
+only b
";
    let m = parse_added_lines(diff);
    assert_eq!(
        m.get("src/a.rs").unwrap(),
        &vec!["added one".to_string(), "added two".to_string()]
    );
    assert_eq!(m.get("src/b.rs").unwrap(), &vec!["only b".to_string()]);
    // the `+++ b/...` file header must NOT be counted as an added line
    assert!(!m
        .get("src/a.rs")
        .unwrap()
        .iter()
        .any(|l| l.contains("b/src/a.rs")));
}

#[test]
fn added_line_starting_with_plusplus_inside_hunk_is_captured_not_skipped() {
    // Hunk-state defense: once inside a hunk, an added line whose content begins
    // with "++ " must be captured as content — a naive "any +++ line is a header"
    // parser would drop it, letting an attacker neutralise content scanning by
    // prepending a benign "++ note" line.
    let diff = "\
diff --git a/danger.sh b/danger.sh
--- a/danger.sh
+++ b/danger.sh
@@ -0,0 +1,2 @@
+++ decorative banner
+rm -rf /
";
    let m = parse_added_lines(diff);
    let added = m.get("danger.sh").unwrap();
    assert!(
        added.iter().any(|l| l.contains("rm -rf /")),
        "real added line must be captured"
    );
    assert!(
        added.iter().any(|l| l.contains("decorative banner")),
        "a `+++ `-prefixed line inside a hunk must be captured, not treated as a header"
    );
}

// ── commitward#3: the two parsers must produce the SAME path key ──────────────────────────
//
// `detect`'s content arm joins `files` (from --name-status) to `added_lines` (from the
// unified diff) on the path string. A mismatch is a silent skip: the file's added lines are
// never scanned against the content denylist and the run reports normally.
//
// The path strings below — the `--name-status` fields and the `+++` headers, including
// where git puts the quotes and its trailing tab — are CAPTURED from a real repository with
// these filenames staged under `-c core.quotePath=false`, the flag the CLI passes. The
// `diff --git` / `@@` / `+` scaffolding around them is minimal and written here: what is
// being pinned is the path forms, and inventing those is how a parser test comes to agree
// with the parser instead of with git. `tests/quoted_paths_e2e.rs` runs the whole chain
// against git itself.

use commitward::gitdiff::unquote_c_style;

/// `core.quotePath=false` covers bytes ≥ 0x80 and nothing else. Git C-quotes a path
/// containing `"`, `\`, or a control character whatever that setting says — which is why
/// this class survived the flag.
#[test]
fn quote_path_false_is_not_enough_for_backslashes_quotes_and_control_chars() {
    // Captured: git emits these for sub/back\slash.sh, sub/quote".sh, sub/café.sh
    assert_eq!(
        unquote_c_style(r#""sub/back\\slash.sh""#),
        r"sub/back\slash.sh"
    );
    assert_eq!(unquote_c_style(r#""sub/quote\".sh""#), "sub/quote\".sh");
    assert_eq!(
        unquote_c_style(r#""sub/tab\tinside.sh""#),
        "sub/tab\tinside.sh"
    );
    assert_eq!(
        unquote_c_style(r#""sub/newline\ninside.sh""#),
        "sub/newline\ninside.sh"
    );
    // Non-ASCII already arrives unquoted under the flag — unchanged, and unquoting is
    // therefore safe to apply unconditionally.
    assert_eq!(unquote_c_style("sub/café.sh"), "sub/café.sh");
    assert_eq!(unquote_c_style("sub/plain.sh"), "sub/plain.sh");
}

/// Octal escapes are per BYTE, so a multi-byte character is several of them. Decoding
/// escape-by-escape into a `String` would corrupt it; this decodes to bytes first.
#[test]
fn octal_escapes_reassemble_a_multibyte_character() {
    // What git emits for café.sh under the DEFAULT core.quotePath — the gate envelope's
    // caller supplies these strings and may not have disabled it.
    assert_eq!(unquote_c_style(r#""sub/caf\303\251.sh""#), "sub/café.sh");
}

/// The join, end to end: every one of these paths must key identically on both sides.
#[test]
fn both_parsers_agree_on_the_key_for_every_captured_path() {
    // Verbatim from `git -c core.quotePath=false diff --cached --name-status`.
    let name_status = concat!(
        "A\t\"sub/back\\\\slash.sh\"\n",
        "A\tsub/café.sh\n",
        "A\tsub/plain.sh\n",
        "A\t\"sub/quote\\\".sh\"\n",
        "A\tsub/trail .sh\n",
        "A\tsub/endswithspace \n",
    );
    // Verbatim `+++` headers from the same diff, each followed by one added line so the
    // parser has something to attribute.
    let diff = concat!(
        "diff --git a b\n+++ \"b/sub/back\\\\slash.sh\"\n@@ -0,0 +1 @@\n+DANGER\n",
        "diff --git a b\n+++ b/sub/café.sh\n@@ -0,0 +1 @@\n+DANGER\n",
        "diff --git a b\n+++ b/sub/plain.sh\n@@ -0,0 +1 @@\n+DANGER\n",
        "diff --git a b\n+++ \"b/sub/quote\\\".sh\"\n@@ -0,0 +1 @@\n+DANGER\n",
        "diff --git a b\n+++ b/sub/trail .sh\t\n@@ -0,0 +1 @@\n+DANGER\n",
        "diff --git a b\n+++ b/sub/endswithspace \t\n@@ -0,0 +1 @@\n+DANGER\n",
    );

    let files = parse_name_status(name_status);
    let added = parse_added_lines(diff);
    assert_eq!(files.len(), 6, "six paths staged");

    for f in &files {
        assert!(
            added.contains_key(&f.path),
            "no added lines keyed under {:?} — its content would be skipped silently. \
             Keys present: {:?}",
            f.path,
            {
                let mut k: Vec<&String> = added.keys().collect();
                k.sort();
                k
            }
        );
        assert_eq!(
            added.get(&f.path).map(|v| v.as_slice()),
            Some(["DANGER".to_string()].as_slice()),
            "the added line must reach the denylist for {:?}",
            f.path
        );
    }
}

/// A path whose LAST character is a space. `trim_end()` ate it on one side and not the
/// other; exactly one trailing tab — git's delimiter — is what may be stripped.
#[test]
fn a_path_ending_in_a_space_keys_the_same_on_both_sides() {
    let files = parse_name_status("A\tsub/endswithspace \n");
    assert_eq!(files[0].path, "sub/endswithspace ", "name-status keeps it");
    let added =
        parse_added_lines("diff --git a b\n+++ b/sub/endswithspace \t\n@@ -0,0 +1 @@\n+DANGER\n");
    assert!(
        added.contains_key("sub/endswithspace "),
        "the trailing space belongs to the path; only git's tab delimiter comes off. \
         Keys: {:?}",
        added.keys().collect::<Vec<_>>()
    );
}

/// The hunk-state defense must survive the rewrite: a `+++ ` line INSIDE a hunk is added
/// content, not a header, so prepending `++ note` cannot neutralise scanning for the rest
/// of the file.
#[test]
fn a_plus_plus_plus_line_inside_a_hunk_is_still_content_not_a_header() {
    let added = parse_added_lines(
        "diff --git a b\n+++ b/a.sh\n@@ -0,0 +2 @@\n+++ \"b/evil.sh\"\n+DANGER\n",
    );
    assert_eq!(
        added.get("a.sh").map(|v| v.as_slice()),
        Some(["++ \"b/evil.sh\"".to_string(), "DANGER".to_string()].as_slice()),
        "both lines belong to a.sh: {added:?}"
    );
    assert!(!added.contains_key("evil.sh"), "and no header was believed");
}

// ── commitward#26 — quoted path AND trailing tab ────────────────────────────
//
// The two triggers are independent and the suite never combined them. Quoting fires on `"`,
// `\` or a control character; the tab delimiter is appended when the path contains a SPACE.
// A path with both produces `+++ "b/…"\t`, where the tab sits OUTSIDE the closing quote —
// so `unquote_c_style` (which returns its input unchanged unless the LAST byte is `"`) hands
// back a still-quoted string, `strip_prefix("b/")` fails on the leading quote, and every
// following `+` line is discarded with `current_file = None`.
//
// The consequence is not a parse detail: a commit whose only change is `rm -rf /` in such a
// file exits 0 from the content gate.

/// Both real headers, exactly as git emits them (captured from a real repo in the issue,
/// under both `core.quotePath` settings — identical output).
const QUOTED_WITH_TAB: &[(&str, &str)] = &[
    ("+++ \"b/sub/sp ace\\\"and.sh\"\t", "sub/sp ace\"and.sh"),
    ("+++ \"b/sub/sp ace\\\\and.sh\"\t", "sub/sp ace\\and.sh"),
];

#[test]
fn a_quoted_header_with_gits_trailing_tab_still_yields_its_path() {
    for (header, want) in QUOTED_WITH_TAB {
        let diff = format!("{header}\n@@ -0,0 +1 @@\n+rm -rf /\n");
        let got = parse_added_lines(&diff);
        assert!(
            got.contains_key(*want),
            "header {header:?} must key as {want:?}; parsed keys were {:?}",
            got.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            got[*want],
            vec!["rm -rf /".to_string()],
            "the added line must reach the content gate for {want:?}"
        );
    }
}

/// The control that makes the test above about the CODE and not the fixture. Each trigger
/// ALONE already worked (commitward#15 closed those), so if these regress the failure is
/// something other than the space-plus-quote combination.
#[test]
fn control_each_trigger_alone_still_yields_its_path() {
    let cases: &[(&str, &str)] = &[
        // quoted, no space -> no trailing tab
        ("+++ \"b/sub/quote\\\".sh\"", "sub/quote\".sh"),
        ("+++ \"b/sub/back\\\\slash.sh\"", "sub/back\\slash.sh"),
        // space, no quote trigger -> trailing tab, unquoted
        ("+++ b/sub/trail .sh\t", "sub/trail .sh"),
        // neither
        ("+++ b/sub/plain.sh", "sub/plain.sh"),
    ];
    for (header, want) in cases {
        let diff = format!("{header}\n@@ -0,0 +1 @@\n+rm -rf /\n");
        let got = parse_added_lines(&diff);
        assert!(
            got.contains_key(*want),
            "control regressed: {header:?} must key as {want:?}; got {:?}",
            got.keys().collect::<Vec<_>>()
        );
    }
}

/// A trailing space belongs to the PATH, not to git. `trim_end()` here would eat it and
/// desync this parser's key from `parse_name_status`'s — the failure the module doc warns
/// about — so exactly one `\t` is stripped and nothing else.
#[test]
fn a_path_ending_in_a_space_keeps_it_after_the_tab_is_stripped() {
    let got = parse_added_lines("+++ b/sub/endswithspace \t\n@@ -0,0 +1 @@\n+rm -rf /\n");
    assert!(
        got.contains_key("sub/endswithspace "),
        "the trailing space is part of the path: {:?}",
        got.keys().collect::<Vec<_>>()
    );
}
