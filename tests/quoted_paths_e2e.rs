//! The content gate against real git, on paths git chooses to quote (commitward#3).
//!
//! `tests/gitdiff.rs` pins the two parsers on captured path forms. This runs the whole chain
//! — git → `--name-status` + unified diff → both parsers → `detect`'s content join — against
//! filenames git actually quotes, because the defect lived in the JOIN between two things
//! that were each individually correct.
//!
//! The failure it guards is a false negative in a security control: a denylisted line added
//! to `sub/quote".sh` was never scanned, and the run exited 0 like any clean commit.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_commitward");

struct TempRepo {
    dir: PathBuf,
}
impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.email=quoted@example.com",
            "-c",
            "user.name=quoted",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs")
}

/// A content-mode checkpoint on the string this test adds to each file.
const CONTENT_REGISTRY: &str = "version: \"1\"\n\
checkpoints:\n\
\x20 - name: destructive-content\n\
\x20   summary: a destructive command was added\n\
\x20   content:\n\
\x20     - \"rm -rf /\"\n";

/// Every class git treats differently, in one commit. `plain` and `café` are the controls:
/// if the fix ever regresses into unquoting too eagerly, they are what notices.
const PATHS: &[&str] = &[
    "sub/plain.sh",
    "sub/café.sh",
    "sub/quote\".sh",
    "sub/back\\slash.sh",
    "sub/trail .sh",
    "sub/endswithspace ",
    // commitward#26: BOTH triggers at once. Each of the six above exercises exactly one —
    // quoting (`"`, `\`, control char) or the space that makes git append its tab — and the
    // combination is the case none of them reach.
    "sub/sp ace\"and.sh",
    "sub/sp ace\\and.sh",
];

#[test]
fn a_denylisted_line_in_a_quoted_path_fires_the_content_checkpoint() {
    let dir = std::env::temp_dir().join(format!("commitward-quoted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
    let repo = TempRepo { dir };
    let d = &repo.dir;

    assert!(git(d, &["init"]).status.success(), "git init");
    std::fs::write(d.join("README.md"), "seed\n").expect("seed");
    std::fs::write(d.join("gates.yaml"), CONTENT_REGISTRY).expect("registry");
    git(d, &["add", "README.md", "gates.yaml"]);
    assert!(git(d, &["commit", "-m", "seed"]).status.success(), "seed");
    let base = String::from_utf8(git(d, &["rev-parse", "HEAD"]).stdout)
        .expect("utf8")
        .trim()
        .to_string();

    for p in PATHS {
        std::fs::write(d.join(p), "#!/bin/sh\nrm -rf /\n")
            .unwrap_or_else(|e| panic!("write {p}: {e}"));
    }
    git(d, &["add", "-A"]);
    assert!(
        git(d, &["commit", "-m", "add scripts"]).status.success(),
        "commit"
    );

    let out = Command::new(BIN)
        .current_dir(d)
        .args([
            "--base",
            &base,
            "--registry",
            d.join("gates.yaml").to_str().expect("utf8 path"),
            "--repo-registry",
            "/nonexistent/repo.yaml",
            "--format",
            "json",
        ])
        .output()
        .expect("commitward runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("stdout is not json ({e}): {text}"));

    let matched: Vec<String> = v["fired"]
        .as_array()
        .expect("fired")
        .iter()
        .find(|f| f["name"] == "destructive-content")
        .and_then(|f| f["matched"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // Enumerated from PATHS — the specification side. Reading the set off `matched` and
    // checking it is non-empty would pass while four of six were skipped, which is exactly
    // the shape of this defect.
    for p in PATHS {
        assert!(
            matched.iter().any(|m| m == p),
            "`rm -rf /` in {p:?} was not scanned by the content gate. Matched: {matched:?}"
        );
    }
    assert_ne!(
        out.status.code(),
        Some(0),
        "and an unacknowledged fire must not exit 0"
    );
}

/// The eight-path run above still exits 2 when this path is dropped, because the other seven
/// fire — so it cannot show the consequence. This one stages the space-plus-quote path as the
/// ONLY file with added lines, which is the shape the issue measured at `exit 0`: a clean pass
/// for a commit whose sole change is `rm -rf /`.
#[test]
fn the_space_and_quote_path_alone_still_exits_2() {
    for (i, (label, path, want_exit)) in [
        // CONTROL first, so a harness that fires on nothing is visible.
        ("control", "sub/ordinary.sh", 2),
        ("subject", "sub/sp ace\"and.sh", 2),
        ("subject", "sub/sp ace\\and.sh", 2),
    ]
    .iter()
    .enumerate()
    {
        // Indexed, not keyed on the path: the two subject paths are the SAME LENGTH and
        // `path.len()` collided them into one directory.
        let dir = std::env::temp_dir().join(format!("commitward-alone-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        let repo = TempRepo { dir };
        let d = &repo.dir;

        assert!(git(d, &["init"]).status.success(), "git init");
        std::fs::write(d.join("README.md"), "seed\n").expect("seed");
        std::fs::write(d.join("gates.yaml"), CONTENT_REGISTRY).expect("registry");
        git(d, &["add", "README.md", "gates.yaml"]);
        assert!(git(d, &["commit", "-m", "seed"]).status.success(), "seed");
        let base = String::from_utf8(git(d, &["rev-parse", "HEAD"]).stdout)
            .expect("utf8")
            .trim()
            .to_string();

        std::fs::write(d.join(path), "#!/bin/sh\nrm -rf /\n")
            .unwrap_or_else(|e| panic!("write {path}: {e}"));
        git(d, &["add", "-A"]);
        assert!(
            git(d, &["commit", "-m", "one file"]).status.success(),
            "commit"
        );

        let out = Command::new(BIN)
            .current_dir(d)
            .args([
                "--base",
                &base,
                "--registry",
                d.join("gates.yaml").to_str().expect("utf8 path"),
                "--repo-registry",
                "/nonexistent/repo.yaml",
                "--format",
                "json",
            ])
            .output()
            .expect("commitward runs");

        assert_eq!(
            out.status.code(),
            Some(*want_exit),
            "{label} {path:?}: a commit adding `rm -rf /` must not pass; stdout was {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
