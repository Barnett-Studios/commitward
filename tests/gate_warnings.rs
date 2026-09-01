//! NF3 (commitward#7): the `gate` subcommand must not report a clean pass for a check
//! it could not perform.
//!
//! `gate` is the containerized front door — the path a consuming harness invokes over
//! stdin/stdout — so a silent failure here is a silent failure in production, not just in
//! the CLI. Drives the real binary; no in-process shortcuts.

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_commitward");

fn gate(request: &str) -> (i32, serde_json::Value) {
    let mut child = Command::new(BIN)
        .arg("gate")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn commitward gate");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(request.as_bytes())
        .expect("write request");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("gate must always emit one JSON envelope: {e}; got {stdout:?}"));
    (out.status.code().unwrap_or(-1), json)
}

const GOOD_REGISTRY: &str = r#"
version: "1"
checkpoints:
  - name: guard-a
    summary: Guards A
    paths:
      - "(^|/)a\\.txt$"
"#;

#[test]
fn a_malformed_registry_is_an_error_envelope_not_a_clean_pass() {
    // The registry is supplied and unparseable. Previously this was swallowed into an
    // empty checkpoint set: status "ok", nothing fired, exit_class 0 — a security control
    // reporting success exactly when it could not run.
    let request = serde_json::json!({
        "diff": "",
        "name_status": "M\ta.txt",
        "commit_msg": "chore: something",
        "global_registry_yaml": "checkpoints:\n  - name: [unclosed\n    summary: broken\n",
    })
    .to_string();

    let (code, env) = gate(&request);
    assert_eq!(
        env["status"], "error",
        "a malformed registry must not yield a `status: ok` envelope; got {env}"
    );
    assert_ne!(code, 0, "and must not exit 0");
    let msg = env["body"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("global") && msg.contains("parse"),
        "the message must name which registry failed and why; got {msg:?}"
    );
}

#[test]
fn a_valid_registry_without_a_base_warns_that_checkpoint_removed_is_inactive() {
    let request = serde_json::json!({
        "diff": "",
        "name_status": "M\tsrc/lib.rs",
        "commit_msg": "chore: something",
        "global_registry_yaml": GOOD_REGISTRY,
    })
    .to_string();

    let (code, env) = gate(&request);
    assert_eq!(
        env["status"], "ok",
        "a valid registry still evaluates: {env}"
    );
    assert_eq!(code, 0);
    let warnings = env["body"]["warnings"]
        .as_array()
        .expect("body.warnings must exist on every ok envelope")
        .iter()
        .map(|w| w.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        warnings.iter().any(|w| w.contains("checkpoint-removed")),
        "with no base registry the checkpoint-removed guard cannot fire, and the caller \
         has no way to know that from `exit_class: 0` alone; got {warnings:?}"
    );
}

#[test]
fn an_empty_checkpoint_set_warns_that_everything_passes() {
    let request = serde_json::json!({
        "diff": "",
        "name_status": "M\tsrc/lib.rs",
        "commit_msg": "chore: something",
    })
    .to_string();

    let (_code, env) = gate(&request);
    let warnings = env["body"]["warnings"].as_array().expect("warnings array");
    // The claim narrowed with commitward#9 — the compiled-in anchor still applies when no
    // registry was supplied, so "every commit passes" would now be false. What must not
    // change is that supplying nothing is reported as a configuration hole rather than
    // read as a clean pass.
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or_default();
            s.contains("no checkpoints were supplied") && s.contains("passes")
        }),
        "no registry at all is the loudest silent pass there is; got {warnings:?}"
    );
}

#[test]
fn a_fully_supplied_request_produces_no_warnings() {
    // Guard: the tests above would pass on an implementation that warned unconditionally.
    // A complete request must come back clean, or the warnings are noise and get ignored.
    let request = serde_json::json!({
        "diff": "",
        "name_status": "M\tsrc/lib.rs",
        "commit_msg": "chore: something",
        "global_registry_yaml": GOOD_REGISTRY,
        "base_global_registry_yaml": GOOD_REGISTRY,
    })
    .to_string();

    let (code, env) = gate(&request);
    assert_eq!(env["status"], "ok");
    assert_eq!(code, 0);
    assert_eq!(
        env["body"]["warnings"].as_array().map(|a| a.len()),
        Some(0),
        "a complete request must warn about nothing; got {}",
        env["body"]["warnings"]
    );
    assert_eq!(env["body"]["exit_class"], 0, "and still evaluate normally");
}

// ── commitward#20: the warnings cover the registry inputs, not the diff inputs ──────────
//
// `body.warnings` named the guards that could not run for the REGISTRY inputs and said
// nothing about the DIFF inputs. Omit `diff` and every content-mode checkpoint is silently
// inactive; omit `name_status` and every checkpoint of both modes is. Either way the
// envelope was `status: "ok"`, `fired: []`, `exit_class: 0` — a clean pass for a change the
// gate never saw.
//
// Same class as #4 (base registry) and #7 (parse error), on the one input dimension those
// did not cover. Behaviour does not change here: `exit_class` stays what it was and the
// gate stays fail-open. Only the reporting.

/// One content-mode checkpoint and one path-mode, so a request can silence exactly one.
const TWO_MODE_REGISTRY: &str = r#"
version: "1"
checkpoints:
  - name: destructive-shell
    summary: destructive shell command added
    content:
      - "rm -rf"
  - name: touches-scripts
    summary: touches a script
    paths:
      - "^scripts/"
"#;

const DEPLOY_DIFF: &str = "diff --git a/scripts/deploy.sh b/scripts/deploy.sh\n\
--- a/scripts/deploy.sh\n\
+++ b/scripts/deploy.sh\n\
@@ -1 +1,2 @@\n\
 set -e\n\
+rm -rf /var/lib/data\n";

fn warnings_of(v: &serde_json::Value) -> Vec<String> {
    v["body"]["warnings"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_control_fires_when_both_inputs_are_supplied() {
    // Without this, everything below is satisfied by a gate that warns unconditionally.
    let (_code, v) = gate(
        &serde_json::json!({
            "diff": DEPLOY_DIFF,
            "name_status": "M\tscripts/deploy.sh",
            "commit_msg": "chore: clean up",
            "global_registry_yaml": TWO_MODE_REGISTRY,
        })
        .to_string(),
    );
    let fired: Vec<String> = v["body"]["fired"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        fired.iter().any(|n| n == "destructive-shell"),
        "the content checkpoint must fire when both inputs are present: {fired:?}"
    );
    let ws = warnings_of(&v);
    assert!(
        !ws.iter().any(|w| w.contains("content-mode")),
        "no input was missing, so nothing should be warned about content mode: {ws:?}"
    );
}

#[test]
fn omitting_diff_warns_that_content_checkpoints_could_not_run() {
    let (_code, v) = gate(
        &serde_json::json!({
            "name_status": "M\tscripts/deploy.sh",
            "commit_msg": "chore: clean up",
            "global_registry_yaml": TWO_MODE_REGISTRY,
        })
        .to_string(),
    );
    let ws = warnings_of(&v);
    assert!(
        ws.iter().any(|w| w.contains("destructive-shell")),
        "a content-mode checkpoint that could not run must be NAMED, or the envelope reads \
         as a partial evaluation that succeeded: {ws:?}"
    );
    // The path checkpoint COULD run — it must not be swept into the same warning.
    assert!(
        !ws.iter().any(|w| w.contains("touches-scripts")),
        "the path checkpoint had its input and must not be reported as unable to run: {ws:?}"
    );
}

#[test]
fn omitting_name_status_warns_that_every_checkpoint_could_not_run() {
    // Both modes, not just the one whose name matches the missing field. `Mode::Content`
    // iterates `files` and only then looks up the added lines, so an empty `name_status`
    // silences it too — and the first version of this warning named `touches-scripts` only,
    // while `destructive-shell` sat silenced beside it with `+rm -rf /var/lib/data` in the
    // diff. A reader given a list of what could not run reasonably concludes that whatever
    // is NOT on it ran; that is a cleaner-looking clean pass than no list at all.
    let (_code, v) = gate(
        &serde_json::json!({
            "diff": DEPLOY_DIFF,
            "commit_msg": "chore: clean up",
            "global_registry_yaml": TWO_MODE_REGISTRY,
        })
        .to_string(),
    );
    let ws = warnings_of(&v);
    assert!(
        ws.iter().any(|w| w.contains("touches-scripts")),
        "a path-mode checkpoint that could not run must be named: {ws:?}"
    );
    assert!(
        ws.iter().any(|w| w.contains("destructive-shell")),
        "a content-mode checkpoint is silenced by an empty name_status just as surely — it \
         iterates the changed files before it looks at any added line — and must be named: \
         {ws:?}"
    );
}

/// A registry whose only checkpoint is the semantic one, so `name_status` decides on its own
/// whether anything at all could run.
const SEMANTIC_ONLY_REGISTRY: &str = r#"
version: "1"
checkpoints:
  - name: checkpoint-removed
    summary: a checkpoint was deleted from the registry
    semantic: checkpoint_removed
"#;

#[test]
fn omitting_name_status_names_the_silenced_checkpoint_removed_guard_too() {
    // The second instance of the same cause. `Semantic(CheckpointRemoved)` reads `files`
    // (via `has_registry_touch`) and belonged to neither the content list nor the path list,
    // so it was silenced with nothing saying so. The base registry IS supplied here, so the
    // pre-existing "no base registry" warning cannot be what satisfies this.
    let (_code, v) = gate(
        &serde_json::json!({
            "diff": DEPLOY_DIFF,
            "commit_msg": "chore: clean up",
            "global_registry_yaml": SEMANTIC_ONLY_REGISTRY,
            "base_global_registry_yaml": SEMANTIC_ONLY_REGISTRY,
        })
        .to_string(),
    );
    let ws = warnings_of(&v);
    assert!(
        !ws.iter().any(|w| w.contains("no base registry supplied")),
        "the base registry is supplied, so that warning must not be what makes this pass: \
         {ws:?}"
    );
    assert!(
        ws.iter()
            .any(|w| w.contains("could NOT run") && w.contains("checkpoint-removed")),
        "the checkpoint-removed guard cannot run without changed paths, and must be named: \
         {ws:?}"
    );
}

#[test]
fn a_request_carrying_no_change_at_all_says_there_was_nothing_to_evaluate() {
    // The sharpest row in the report: a registry, a commit message, and no change —
    // `exit_class: 0`, and the only warning was about the base registry. "Nothing to
    // evaluate" is a different statement from "guard X could not run", and gets its own.
    let (_code, v) = gate(
        &serde_json::json!({
            "commit_msg": "chore: clean up",
            "global_registry_yaml": TWO_MODE_REGISTRY,
        })
        .to_string(),
    );
    let ws = warnings_of(&v);
    assert!(
        ws.iter().any(|w| w.contains("no change was supplied")),
        "a request with neither diff nor name_status evaluated nothing, and must say so \
         rather than report a clean pass: {ws:?}"
    );
}

/// One content-mode checkpoint only, so omitting `diff` silences the whole registry —
/// the exact row the report calls out.
const CONTENT_ONLY_REGISTRY: &str = r#"
version: "1"
checkpoints:
  - name: destructive-shell
    summary: destructive shell command added
    content:
      - "rm -rf"
"#;

#[test]
fn a_silenced_content_guard_still_reports_exit_class_0_but_no_longer_silently() {
    // Fail-open is not in question and must not move: the verdict for this request was
    // `exit_class: 0` before and stays `0`. What changes is that the envelope now says the
    // guard could not run, so `0` can be read as "nothing fired" rather than mistaken for
    // "nothing to worry about". Warnings are additive telemetry, not a new refusal.
    let (code, v) = gate(
        &serde_json::json!({
            "name_status": "M\tscripts/deploy.sh",
            "commit_msg": "chore: clean up",
            "global_registry_yaml": CONTENT_ONLY_REGISTRY,
        })
        .to_string(),
    );
    assert_eq!(v["body"]["exit_class"], 0, "exit_class moved: {v}");
    assert_eq!(code, 0, "process exit moved: {v}");
    assert_eq!(v["status"], "ok", "status moved: {v}");
    assert_eq!(
        v["body"]["fired"].as_array().map(|a| a.len()),
        Some(0),
        "nothing can fire with no added lines: {v}"
    );
    assert!(
        warnings_of(&v)
            .iter()
            .any(|w| w.contains("destructive-shell")),
        "the silenced guard must be named: {v}"
    );
}

#[test]
fn a_vacuous_request_still_reports_exit_class_0_but_no_longer_silently() {
    let (code, v) = gate(
        &serde_json::json!({
            "commit_msg": "chore: clean up",
            "global_registry_yaml": CONTENT_ONLY_REGISTRY,
        })
        .to_string(),
    );
    assert_eq!(v["body"]["exit_class"], 0, "exit_class moved: {v}");
    assert_eq!(code, 0, "process exit moved: {v}");
    assert_eq!(v["status"], "ok", "status moved: {v}");
}
