//! End-to-end smoke tests for the `commitward` binary against real temp git
//! repos. Proves the exit-code contract and the fail-open guarantee (ADR-0048):
//! exit 2 on a fired-and-unacked checkpoint, exit 0 (with a warning) when the
//! registry is absent, and exit 0 when a HITL-ACK trailer acknowledges the fire.
//! No external test deps — the binary path comes from CARGO_BIN_EXE_commitward.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_commitward");

/// Temp git repo, removed on drop (including on test-panic unwind).
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
            "user.email=smoke@example.com",
            "-c",
            "user.name=smoke",
            "-c",
            "core.hooksPath=/dev/null", // isolate from any global commit-msg hook
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs")
}

fn commitward(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("commitward binary runs")
}

fn rev_parse_head(dir: &Path) -> String {
    String::from_utf8(git(dir, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string()
}

/// Fresh repo with one seed commit; returns (repo, base-ref = seed HEAD).
fn setup(name: &str) -> (TempRepo, String) {
    let dir = std::env::temp_dir().join(format!("commitward-smoke-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(git(&dir, &["init"]).status.success(), "git init");
    std::fs::write(dir.join("README.md"), "seed\n").unwrap();
    git(&dir, &["add", "README.md"]);
    assert!(
        git(&dir, &["commit", "-m", "seed"]).status.success(),
        "seed commit"
    );
    let base = rev_parse_head(&dir);
    (TempRepo { dir }, base)
}

const REGISTRY: &str = "version: \"1\"\n\
checkpoints:\n\
\x20 - name: danger-file\n\
\x20   summary: touches danger.sh\n\
\x20   paths:\n\
\x20     - \"(^|/)danger\\\\.sh$\"\n";

/// Stage a repo-local registry firing on danger.sh, add danger.sh, commit.
fn add_guarded_change(dir: &Path) {
    std::fs::create_dir_all(dir.join(".dotclaude")).unwrap();
    std::fs::write(dir.join(".dotclaude/checkpoints.yaml"), REGISTRY).unwrap();
    std::fs::write(dir.join("danger.sh"), "echo hi\n").unwrap();
    git(dir, &["add", "danger.sh", ".dotclaude/checkpoints.yaml"]);
    assert!(
        git(dir, &["commit", "-m", "add danger"]).status.success(),
        "danger commit"
    );
}

#[test]
fn fires_exit_2_on_guarded_path_without_ack() {
    let (repo, base) = setup("fire");
    let d = &repo.dir;
    add_guarded_change(d);
    let msg = d.join("msg.txt");
    std::fs::write(&msg, "add danger\n").unwrap(); // no HITL-ACK
    let out = commitward(
        d,
        &[
            "--base",
            &base,
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            d.join(".dotclaude/checkpoints.yaml").to_str().unwrap(),
            "--commit-msg-file",
            msg.to_str().unwrap(),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 (fired+unacked); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fail_open_exit_0_when_registries_absent() {
    let (repo, base) = setup("failopen");
    let d = &repo.dir;
    // A guarded-looking change exists, but no registry is reachable.
    std::fs::write(d.join("danger.sh"), "echo hi\n").unwrap();
    git(d, &["add", "danger.sh"]);
    git(d, &["commit", "-m", "add danger"]);
    let out = commitward(
        d,
        &[
            "--base",
            &base,
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            "/nonexistent/repo.yaml",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "absent registry must fail open to exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("WARNING"),
        "fail-open path must emit a diagnostic, not silently disable"
    );
}

#[test]
fn ack_trailer_lifts_the_block_to_exit_1() {
    // Exit-code contract: 0 = nothing fired, 1 = fired+all-acked (allowed to
    // proceed), 2 = fired+unacked (blocked). A matching HITL-ACK does not erase
    // the fire — it lifts the *block*: the same change that returns 2 unacked
    // returns 1 acked. Only exit 2 blocks a commit.
    let (repo, base) = setup("ack");
    let d = &repo.dir;
    add_guarded_change(d);
    let msg = d.join("msg.txt");
    std::fs::write(&msg, "add danger\n\nHITL-ACK: danger-file smoke test\n").unwrap();
    let out = commitward(
        d,
        &[
            "--base",
            &base,
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            d.join(".dotclaude/checkpoints.yaml").to_str().unwrap(),
            "--commit-msg-file",
            msg.to_str().unwrap(),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a matching HITL-ACK trailer must lift the block (fired+acked -> exit 1, not 2); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
