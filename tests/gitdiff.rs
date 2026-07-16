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
