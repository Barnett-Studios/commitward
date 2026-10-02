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
    std::fs::create_dir_all(dir.join(".commitward")).unwrap();
    std::fs::write(dir.join(".commitward/checkpoints.yaml"), REGISTRY).unwrap();
    std::fs::write(dir.join("danger.sh"), "echo hi\n").unwrap();
    git(dir, &["add", "danger.sh", ".commitward/checkpoints.yaml"]);
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
            d.join(".commitward/checkpoints.yaml").to_str().unwrap(),
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

/// commitward#30: `cargo install` places only the binary — nothing puts
/// `checkpoints.yaml` beside it, which is `default_registry()`'s resolution when
/// neither `--registry` nor `$COMMITWARD_REGISTRY` is given. Unlike
/// `fail_open_exit_0_when_registries_absent` above, this test supplies NEITHER flag
/// nor env var, so `default_registry()` runs for real — and resolves beside
/// `CARGO_BIN_EXE_commitward`, a `target/debug` directory that has no
/// `checkpoints.yaml` next to it either. This is the exact gap the issue measured on
/// the published crate: the documented install leaves every shipped checkpoint
/// inactive.
#[test]
fn the_documented_cargo_install_route_still_enforces_the_shipped_baseline() {
    let (repo, base) = setup("cargo-install-route");
    let d = &repo.dir;
    // The README's own worked example: editing CLAUDE.md, guarded by the shipped
    // `agent-instructions-self-mod` checkpoint.
    std::fs::write(d.join("CLAUDE.md"), "y\n").unwrap();
    git(d, &["add", "CLAUDE.md"]);
    assert!(
        git(d, &["commit", "-m", "edit CLAUDE.md"]).status.success(),
        "CLAUDE.md commit"
    );
    let msg = d.join("msg.txt");
    std::fs::write(&msg, "edit CLAUDE.md\n").unwrap(); // no HITL-ACK
    let out = Command::new(BIN)
        .current_dir(d)
        .env_remove("COMMITWARD_REGISTRY")
        .args(["--base", &base, "--commit-msg-file", msg.to_str().unwrap()])
        .output()
        .expect("commitward binary runs");
    assert_eq!(
        out.status.code(),
        Some(2),
        "the shipped agent-instructions-self-mod checkpoint must fire on the documented \
         cargo-install route, with no --registry and no $COMMITWARD_REGISTRY; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
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
    // Two fires now, not one: `add_guarded_change` commits `.commitward/checkpoints.yaml`,
    // and the compiled-in anchor watches the gate's own files (commitward#9). Both must be
    // acked for the block to lift — which is the contract, "a *matching* ack per fire".
    std::fs::write(
        &msg,
        "add danger\n\nHITL-ACK: danger-file smoke test\n\
         HITL-ACK: anchor-gate-integrity adopting a registry is a gate change\n",
    )
    .unwrap();
    let out = commitward(
        d,
        &[
            "--base",
            &base,
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            d.join(".commitward/checkpoints.yaml").to_str().unwrap(),
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

/// NF2, driven through the **real CLI** against the **shipped** registry (commitward#7).
///
/// `default_registry.rs` proves the same scenario at the engine layer (`detect(&compiled, …)`).
/// That validates the logic but not the wrapper a consumer actually runs: argument parsing,
/// registry resolution, base-ref diffing and the exit-code contract all sit in `main::run`, and
/// none of them were on the NF2 path. Review on #8 asked for that seam to be closed.
///
/// The scenario: an agent removes `destructive-ops` from the repo-local registry AND introduces a
/// dangerous recursive-force removal, in ONE commit. `destructive-ops` cannot fire — it no longer
/// exists — so the only thing between this and a clean gate is the registry's self-protection.
#[test]
fn nf2_registry_weakening_fires_through_the_real_cli() {
    let (repo, base) = setup("nf2-cli");
    let d = &repo.dir;

    // Base commit: the shipped default registry, copied in as the repo-local one so this
    // exercises the real file rather than a fixture written for the test.
    let shipped = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("checkpoints.yaml"),
    )
    .expect("the shipped default registry must be readable");
    std::fs::create_dir_all(d.join(".commitward")).unwrap();
    std::fs::write(d.join(".commitward/checkpoints.yaml"), &shipped).unwrap();
    git(d, &["add", ".commitward/checkpoints.yaml"]);
    assert!(
        git(d, &["commit", "-m", "adopt the default registry"])
            .status
            .success(),
        "registry commit"
    );
    let base_with_registry = rev_parse_head(d);
    let _ = base;

    // The attack, in one commit: delete the guard, use what it guarded.
    let weakened: String = shipped
        .lines()
        .scan(false, |skipping, line| {
            if line.starts_with("  - name: ") {
                *skipping = line.contains("destructive-ops");
            }
            Some(if *skipping { None } else { Some(line) })
        })
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !weakened.contains("destructive-ops"),
        "the test must actually remove the checkpoint it claims to"
    );
    std::fs::write(
        d.join(".commitward/checkpoints.yaml"),
        format!("{weakened}\n"),
    )
    .unwrap();
    std::fs::write(d.join("cleanup.sh"), "rm -rf / --no-preserve-root\n").unwrap();
    git(d, &["add", ".commitward/checkpoints.yaml", "cleanup.sh"]);
    assert!(
        git(d, &["commit", "-m", "tidy up"]).status.success(),
        "attack commit"
    );

    let out = commitward(d, &["--base", &base_with_registry, "--format", "json"]);
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert_eq!(
        code, 2,
        "removing a guard and using what it guarded, in one commit, must reach a human — \
         got exit {code}; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("gate-self-mod") || stdout.contains("checkpoint-removed"),
        "the fire must name a self-protection checkpoint; stdout:\n{stdout}"
    );
}

// ── commitward#9: the registry cannot be the sole protector of the registry ──────
//
// #8 shipped `gate-self-mod` and `checkpoint-removed` in the default registry. Both live
// *in* the registry, so deleting the registry deletes its own guard in the same act — and
// `checkpoint-removed` cannot fire when there is no base registry to compare against
// (#4). A single commit that removes the whole file therefore fired nothing at all: the
// guard was self-referential, and the PR's own `checkpoints.yaml` comments said so.
//
// The anchor is compiled into the binary, so there is no on-disk edit that removes it.

/// The acceptance criterion from #9, driven through the shipped CLI on a real repo.
#[test]
fn anchor_fires_when_the_whole_registry_is_deleted_in_one_commit() {
    let (repo, base) = setup("anchor-wipe");
    let d = &repo.dir;
    let _ = base;

    // Base: a repo-local registry exists and guards something.
    std::fs::create_dir_all(d.join(".commitward")).unwrap();
    std::fs::write(d.join(".commitward/checkpoints.yaml"), REGISTRY).unwrap();
    git(d, &["add", ".commitward/checkpoints.yaml"]);
    assert!(
        git(d, &["commit", "-m", "adopt a registry"])
            .status
            .success(),
        "registry commit"
    );
    let base_with_registry = rev_parse_head(d);

    // The attack: delete the registry outright and use what it guarded, in one commit.
    // Nothing on disk can fire afterwards — there is no registry left to fire from, and
    // no global one either (--registry points at nothing).
    assert!(
        git(d, &["rm", "-q", ".commitward/checkpoints.yaml"])
            .status
            .success(),
        "git rm registry"
    );
    std::fs::write(d.join("danger.sh"), "echo hi\n").unwrap();
    git(d, &["add", "danger.sh"]);
    assert!(
        git(d, &["commit", "-m", "simplify"]).status.success(),
        "attack commit"
    );

    let out = commitward(
        d,
        &[
            "--base",
            &base_with_registry,
            "--registry",
            "/nonexistent/global.yaml",
            "--format",
            "json",
        ],
    );
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    assert_eq!(
        code, 2,
        "deleting the registry that guards the registry must still reach a human — \
         got exit {code}; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("checkpoints.yaml"),
        "the fire must name the registry file that was removed; stdout:\n{stdout}"
    );
}

#[test]
fn anchor_does_not_fire_on_an_ordinary_commit() {
    // Guard: an anchor that fires on everything is not a gate, it is a nuisance that
    // teaches people to pass --format and ignore the output. Same repo shape as above,
    // minus the registry edit.
    let (repo, base) = setup("anchor-quiet");
    let d = &repo.dir;
    std::fs::write(d.join("README.md"), "seed\nmore prose\n").unwrap();
    git(d, &["add", "README.md"]);
    assert!(
        git(d, &["commit", "-m", "docs"]).status.success(),
        "ordinary commit"
    );

    let out = commitward(
        d,
        &[
            "--base",
            &base,
            "--registry",
            "/nonexistent/global.yaml",
            "--format",
            "json",
        ],
    );
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        code, 0,
        "an ordinary commit must still pass; stdout:\n{stdout}"
    );
}

// ── checkpoint-removed: the guard that protects the registry (commitward#4) ───────────────
//
// The gate envelope already reported an unresolvable base as a warning (commitward#7). The
// CLI did not — it passed `Some(vec![])` to `detect` after a failed `git show`, which claims
// "the base registry declared nothing" rather than "I could not find out". Same control, two
// front doors, one of them silent.

const REMOVED_REGISTRY: &str = "version: \"1\"\n\
checkpoints:\n\
\x20 - name: registry-shrunk\n\
\x20   summary: a checkpoint was removed from the registry\n\
\x20   semantic: checkpoint_removed\n\
\x20 - name: doomed\n\
\x20   summary: the one that gets deleted\n\
\x20   paths:\n\
\x20     - \"(^|/)nothing-matches-this$\"\n";

/// Same registry with `doomed` gone — the removal the semantic guard must catch.
const REMOVED_REGISTRY_AFTER: &str = "version: \"1\"\n\
checkpoints:\n\
\x20 - name: registry-shrunk\n\
\x20   summary: a checkpoint was removed from the registry\n\
\x20   semantic: checkpoint_removed\n\
";

fn json_of(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("stdout is not json ({e}): {text}"))
}

/// A base ref that does not resolve is the shallow-clone / first-commit case. The guard
/// cannot run, and the run must say so instead of exiting 0 like a clean pass.
#[test]
fn an_unresolvable_base_warns_that_checkpoint_removed_could_not_run() {
    let (repo, _base) = setup("no-base");
    let d = &repo.dir;
    std::fs::create_dir_all(d.join(".commitward")).unwrap();
    std::fs::write(d.join(".commitward/checkpoints.yaml"), REMOVED_REGISTRY).unwrap();
    git(d, &["add", "."]);
    assert!(git(d, &["commit", "-m", "adopt registry"]).status.success());

    let out = commitward(
        d,
        &[
            "--base",
            "refs/heads/does-not-exist",
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            d.join(".commitward/checkpoints.yaml").to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    let warnings = json_of(&out)["warnings"]
        .as_array()
        .expect("the CLI's json carries warnings")
        .iter()
        .map(|w| w.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("checkpoint-removed") && w.contains("INACTIVE")),
        "an unresolvable base disables the guard and the run must say so; got {warnings:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checkpoint-removed"),
        "and on stderr too, for the operator who is not parsing json: {stderr}"
    );
}

/// The other side, and the one that keeps the warning worth reading: a run whose base
/// resolves must warn about nothing. A gate that warns on every invocation has told the
/// operator to stop reading warnings.
#[test]
fn a_resolvable_base_produces_no_warning() {
    let (repo, base) = setup("with-base");
    let d = &repo.dir;
    std::fs::create_dir_all(d.join(".commitward")).unwrap();
    std::fs::write(d.join(".commitward/checkpoints.yaml"), REMOVED_REGISTRY).unwrap();
    git(d, &["add", "."]);
    assert!(git(d, &["commit", "-m", "adopt registry"]).status.success());

    let out = commitward(
        d,
        &[
            "--base",
            &base,
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            d.join(".commitward/checkpoints.yaml").to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    assert_eq!(
        json_of(&out)["warnings"].as_array().map(|a| a.len()),
        Some(0),
        "a resolvable base has nothing to warn about: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// …and a registry that declares no `checkpoint_removed` must not warn either, even with no
/// base at all. Nothing was disabled, so the warning would be about a guard this repo never
/// asked for.
#[test]
fn no_checkpoint_removed_in_the_registry_means_no_warning_about_it() {
    let (repo, _base) = setup("no-guard");
    let d = &repo.dir;
    add_guarded_change(d);

    let out = commitward(
        d,
        &[
            "--base",
            "refs/heads/does-not-exist",
            "--registry",
            "/nonexistent/global.yaml",
            "--repo-registry",
            d.join(".commitward/checkpoints.yaml").to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    let warnings = json_of(&out)["warnings"]
        .as_array()
        .expect("warnings")
        .clone();
    assert!(
        !warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("checkpoint-removed")),
        "this registry has no checkpoint_removed guard, so none was disabled: {warnings:?}"
    );
}

/// A checkpoint removed from a registry named something other than `checkpoints.yaml` — the
/// `$COMMITWARD_REGISTRY` / `--registry` case. The library's fallback only knows the suffix,
/// so the CLI has to name its registries or the guard cannot see the change that removed one.
#[test]
fn a_removal_from_a_non_standard_registry_filename_still_fires() {
    let (repo, base) = setup("odd-name");
    let d = &repo.dir;
    std::fs::write(d.join("my-gates.yaml"), REMOVED_REGISTRY).unwrap();
    git(d, &["add", "."]);
    assert!(git(d, &["commit", "-m", "adopt registry"]).status.success());
    let base_with_registry = rev_parse_head(d);

    // Now delete `doomed` from it. The changed path is `my-gates.yaml` — no `checkpoints.yaml`
    // suffix anywhere in this repo.
    std::fs::write(d.join("my-gates.yaml"), REMOVED_REGISTRY_AFTER).unwrap();
    git(d, &["add", "."]);
    assert!(git(d, &["commit", "-m", "shrink registry"])
        .status
        .success());

    let out = commitward(
        d,
        &[
            "--base",
            &base_with_registry,
            "--registry",
            d.join("my-gates.yaml").to_str().unwrap(),
            "--repo-registry",
            "/nonexistent/repo.yaml",
            "--format",
            "json",
        ],
    );
    let v = json_of(&out);
    let fired: Vec<&str> = v["fired"]
        .as_array()
        .expect("fired")
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(
        fired.contains(&"registry-shrunk"),
        "removing `doomed` from a registry named my-gates.yaml must fire the semantic \
         guard; got {v}"
    );
    let matched: Vec<&str> = v["fired"]
        .as_array()
        .expect("fired")
        .iter()
        .find(|f| f["name"] == "registry-shrunk")
        .and_then(|f| f["matched"].as_array())
        .expect("matched")
        .iter()
        .filter_map(|m| m.as_str())
        .collect();
    assert_eq!(matched, vec!["doomed"], "and name what was removed");
    let _ = base;
}
