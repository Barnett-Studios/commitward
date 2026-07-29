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
