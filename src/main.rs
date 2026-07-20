//! commitward — a deterministic, fail-open human-sign-off gate for high-stakes
//! commits. Shells `git diff` itself, matches a diff against a checkpoint
//! registry, and exits 2 only when a checkpoint fires and is not acknowledged by
//! a `HITL-ACK:` trailer in the commit message. Every infrastructure error
//! (missing git, unreadable registry, malformed diff) degrades to exit 0 — the
//! gate never blocks on its own failure.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use commitward::gitdiff::{parse_added_lines, parse_name_status};
use commitward::{
    compile, detect, exit_class, extract_acks, extract_checkpoint_names, load_checkpoints, merge,
    partition_ack, Checkpoint, FileEntry,
};
use serde::Deserialize;

const USAGE: &str = "\
commitward — deterministic fail-open HITL commit gate

USAGE:
    commitward [OPTIONS]

OPTIONS:
    --base <ref>              Base ref to diff against HEAD (default: origin/main)
    --cached                  Diff the staged index against HEAD instead of a base ref
    --commit-msg-file <path>  File holding the commit message to scan for HITL-ACK trailers
    --registry <path>         Global checkpoint baseline (default: $COMMITWARD_REGISTRY,
                              else checkpoints.yaml next to the binary)
    --repo-registry <path>    Repo-local checkpoint overrides (default: .commitward/checkpoints.yaml)
    --format <text|json|markdown>   Output format (default: text)
    -h, --help                Print this help

EXIT CODES:
    0   no checkpoint fired, all fired checkpoints acknowledged, or any fail-open path
    2   at least one checkpoint fired and is unacknowledged (human sign-off required)
   64   usage error (bad flag / missing argument)

SUBCOMMANDS:
    gate    Read a JSON gate request on stdin (diff + registries inlined) and write an
            ADR-0052 response envelope on stdout — for programmatic/container consumption
            (network-free). The block decision is carried in body.exit_class, not the exit code.

Disable entirely with COMMITWARD_HITL=off.";

fn main() {
    // The `gate` envelope subcommand (ADR-0052) is intercepted before flag parsing; every other
    // invocation is the native git-reading CLI below.
    if std::env::args().nth(1).as_deref() == Some("gate") {
        std::process::exit(run_gate());
    }
    let code = run();
    std::process::exit(code);
}

/// The `gate` envelope subcommand (ADR-0052): read a JSON request on stdin — the unified diff, the
/// `git diff --name-status` output, the commit message, and the checkpoint registries inlined as
/// YAML — evaluate the checkpoints, and write a `{schema_version, status, body}` envelope on stdout.
///
/// The gate DECISION is carried in `body.exit_class` (0 = pass / all-acked, 2 = unacked fire), NOT
/// the process exit code: the process exits 0 on any successful evaluation, so a consumer's
/// `ComponentInvoker` never mistakes a fired gate for a transport failure. Only an infrastructure
/// error (unreadable stdin, malformed request) exits non-zero with a `status:"error"` envelope.
///
/// Fully self-contained: the diff and registries are inlined, so no git, no network, no mounts —
/// safe under `docker run --network none`. The native `commitward` CLI (which shells `git` itself)
/// stays the path for git hooks and standalone use.
fn run_gate() -> i32 {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        println!("{}", error_envelope(&format!("failed to read stdin: {e}")));
        return 1;
    }
    match gate_envelope(&input) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            println!("{}", error_envelope(&e));
            1
        }
    }
}

/// The `gate` request: everything commitward needs to evaluate, inlined (no git, no mounts). Every
/// field is optional so a caller supplies only what it has; the evaluation fails open on absent
/// inputs, mirroring the native CLI.
#[derive(Deserialize)]
struct GateRequest {
    #[serde(default)]
    diff: String,
    #[serde(default)]
    name_status: String,
    #[serde(default)]
    commit_msg: String,
    #[serde(default)]
    global_registry_yaml: Option<String>,
    #[serde(default)]
    repo_registry_yaml: Option<String>,
    /// The repo registry as it stood at the base ref — enables checkpoint-removed detection.
    #[serde(default)]
    base_repo_registry_yaml: Option<String>,
    /// The global registry as it stood at the base ref. Unioned with the repo base so a removed
    /// checkpoint is detected whichever registry it lived in — matching the native CLI, which
    /// unions base names from both `promise/checkpoints.yaml` and `.dotclaude/checkpoints.yaml`.
    #[serde(default)]
    base_global_registry_yaml: Option<String>,
}

fn gate_envelope(input: &str) -> Result<String, String> {
    let req: GateRequest =
        serde_json::from_str(input).map_err(|e| format!("invalid gate request JSON: {e}"))?;

    // Inlined YAML → checkpoints. `load_checkpoints` takes a path, so materialize each registry to a
    // self-cleaning temp file; an absent registry is an empty set (fail-open, mirrors the native CLI).
    let global_cps = load_inlined_registry(req.global_registry_yaml.as_deref(), "global")?;
    let repo_cps = load_inlined_registry(req.repo_registry_yaml.as_deref(), "repo")?;
    let compiled =
        compile(merge(global_cps, repo_cps)).map_err(|e| format!("registry compile error: {e}"))?;

    let files = parse_name_status(&req.name_status);
    let added = parse_added_lines(&req.diff);
    // Union base checkpoint names from both the repo and global base registries, mirroring the
    // native CLI (`local_base.union(&global_base)`). `None` only when neither was supplied.
    let base_names: Option<Vec<String>> = match (&req.base_repo_registry_yaml, &req.base_global_registry_yaml)
    {
        (None, None) => None,
        (repo, global) => {
            let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            if let Some(y) = repo {
                names.extend(extract_checkpoint_names(y));
            }
            if let Some(y) = global {
                names.extend(extract_checkpoint_names(y));
            }
            Some(names.into_iter().collect())
        }
    };

    let fired = detect(&compiled, &files, &added, base_names.as_deref());
    let acks = extract_acks(&req.commit_msg);
    let (_acked, unacked) = partition_ack(&fired, &acks);
    let ec = exit_class(fired.len(), unacked.len());

    let unacked_names: Vec<&str> = unacked.iter().map(|f| f.name.as_str()).collect();
    let body = serde_json::json!({
        "fired": &fired,
        "unacked": unacked_names,
        "exit_class": ec,
    });
    Ok(ok_envelope(body))
}

/// Load an inlined-YAML registry via a self-cleaning temp file. A parse error is fail-open (empty
/// set, as the native CLI does); only a temp-file infrastructure error propagates.
fn load_inlined_registry(yaml: Option<&str>, label: &str) -> Result<Vec<Checkpoint>, String> {
    match yaml {
        Some(y) => {
            let tmp = write_temp_yaml(y, label)?;
            Ok(load_checkpoints(&tmp.0).unwrap_or_default())
        }
        None => Ok(vec![]),
    }
}

/// A temp file removed on drop (every path, including panic).
struct TempYaml(PathBuf);
impl Drop for TempYaml {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Monotonic per-process counter so two `gate` calls in one process (or parallel tests) never
/// collide on the temp filename despite sharing a pid.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write `content` to a uniquely-named temp file with `create_new` (O_EXCL) so a predictable path
/// can never be made to follow a pre-existing symlink.
fn write_temp_yaml(content: &str, label: &str) -> Result<TempYaml, String> {
    use std::io::Write as _;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "commitward-gate-{label}-{}-{seq}.yaml",
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("create temp {label} registry: {e}"))?;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("write temp {label} registry: {e}"))?;
    Ok(TempYaml(path))
}

/// An `ok`-status ADR-0052 envelope wrapping a computed body.
fn ok_envelope(body: serde_json::Value) -> String {
    serde_json::json!({ "schema_version": "1", "status": "ok", "body": body }).to_string()
}

/// An `error`-status envelope — the ADR-0052 sentinel. A consumer treats `status != "ok"` as an
/// infrastructure failure and falls back to its in-process path rather than trusting a result.
fn error_envelope(message: &str) -> String {
    serde_json::json!({ "schema_version": "1", "status": "error", "body": { "message": message } })
        .to_string()
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();

    let mut base_ref = String::from("origin/main");
    let mut cached = false;
    let mut commit_msg_file: Option<PathBuf> = None;
    let mut registry: Option<PathBuf> = None;
    let mut repo_registry: Option<PathBuf> = None;
    let mut format = String::from("text");

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--cached" => cached = true,
            "--base" => {
                i += 1;
                match args.get(i) {
                    Some(v) => base_ref = v.clone(),
                    None => return usage_err("--base requires an argument"),
                }
            }
            "--commit-msg-file" => {
                i += 1;
                match args.get(i) {
                    Some(v) => commit_msg_file = Some(PathBuf::from(v)),
                    None => return usage_err("--commit-msg-file requires an argument"),
                }
            }
            "--registry" => {
                i += 1;
                match args.get(i) {
                    Some(v) => registry = Some(PathBuf::from(v)),
                    None => return usage_err("--registry requires an argument"),
                }
            }
            "--repo-registry" => {
                i += 1;
                match args.get(i) {
                    Some(v) => repo_registry = Some(PathBuf::from(v)),
                    None => return usage_err("--repo-registry requires an argument"),
                }
            }
            "--format" => {
                i += 1;
                match args.get(i).map(String::as_str) {
                    Some(f @ ("text" | "json" | "markdown")) => format = f.to_string(),
                    Some(other) => {
                        return usage_err(&format!("unknown format '{other}' (text|json|markdown)"))
                    }
                    None => return usage_err("--format requires an argument"),
                }
            }
            other => return usage_err(&format!("unknown flag '{other}'")),
        }
        i += 1;
    }

    // Global off switch (fail-open by construction).
    if std::env::var("COMMITWARD_HITL").as_deref() == Ok("off") {
        return 0;
    }

    let repo_root = git_toplevel().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // ── Registries: global baseline + repo overrides, merged. ──────────────
    let global_path = registry.unwrap_or_else(default_registry);
    if !global_path.exists() {
        eprintln!(
            "commitward: WARNING global checkpoint registry not found at {} — \
             global baseline INACTIVE (only repo-local overrides apply). Pass --registry or set \
             COMMITWARD_REGISTRY. (fail-open: continuing)",
            global_path.display()
        );
    }
    let global_cps = match load_checkpoints(&global_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "commitward: registry load error (fail-open): {}: {e}",
                global_path.display()
            );
            vec![]
        }
    };

    let repo_path = repo_registry.unwrap_or_else(|| repo_root.join(".commitward/checkpoints.yaml"));
    let repo_cps = match load_checkpoints(&repo_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "commitward: registry load error (fail-open): {}: {e}",
                repo_path.display()
            );
            vec![]
        }
    };

    let compiled = match compile(merge(global_cps, repo_cps)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("commitward: registry compile error (fail-open): {e}");
            return 0;
        }
    };

    // ── Diff (fail-open on any git error). ─────────────────────────────────
    let base_or_cached: Option<&str> = if cached {
        None
    } else {
        Some(base_ref.as_str())
    };
    let files: Vec<FileEntry> = match git_diff_name_status(&repo_root, base_or_cached) {
        Ok(out) => parse_name_status(&out),
        Err(e) => {
            eprintln!("commitward: git diff failed (fail-open): {e}");
            return 0;
        }
    };
    let added = git_diff_unified0(&repo_root, base_or_cached)
        .map(|out| parse_added_lines(&out))
        .unwrap_or_default();

    // ── Base checkpoint names for checkpoint-removed detection. ─────────────
    // Read the repo-registry file as it stood at the base ref; a checkpoint that
    // exists at base but not now was removed.
    let name_ref: &str = if cached { "HEAD" } else { base_ref.as_str() };
    let repo_rel = repo_path
        .strip_prefix(&repo_root)
        .unwrap_or(Path::new(".commitward/checkpoints.yaml"));
    let base_names: Vec<String> = git_show(&repo_root, name_ref, &repo_rel.to_string_lossy())
        .map(|text| extract_checkpoint_names(&text))
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let fired = detect(&compiled, &files, &added, Some(&base_names));

    // ── Acks. ──────────────────────────────────────────────────────────────
    let commit_msg = match &commit_msg_file {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => git_log_messages(&repo_root, &base_ref).unwrap_or_default(),
    };
    let acks = extract_acks(&commit_msg);
    let (acked, unacked) = partition_ack(&fired, &acks);

    match format.as_str() {
        "json" => {
            let obj = serde_json::json!({ "fired": &fired, "acked": &acked, "unacked": &unacked });
            match serde_json::to_string_pretty(&obj) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("commitward: json serialize error: {e}"),
            }
        }
        "markdown" => print_markdown(&fired, &acked, &unacked),
        _ => print_text(&fired, &acked, &unacked),
    }

    exit_class(fired.len(), unacked.len())
}

fn usage_err(msg: &str) -> i32 {
    eprintln!("commitward: {msg}");
    eprintln!("{USAGE}");
    64
}

/// Default global registry: `$COMMITWARD_REGISTRY`, else `checkpoints.yaml`
/// beside the executable (where the Docker image and installers place it).
fn default_registry() -> PathBuf {
    if let Ok(p) = std::env::var("COMMITWARD_REGISTRY") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("checkpoints.yaml");
        }
    }
    PathBuf::from("checkpoints.yaml")
}

fn git_toplevel() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn git_diff_name_status(cwd: &Path, base: Option<&str>) -> std::io::Result<String> {
    run_git_diff(cwd, base, "--name-status")
}

fn git_diff_unified0(cwd: &Path, base: Option<&str>) -> std::io::Result<String> {
    run_git_diff(cwd, base, "--unified=0")
}

/// Shell `git diff` with the same flags commitward uses: `--no-renames`
/// (renames surface as D+A so a guarded *old* path still fires) and
/// `--diff-filter=ACDMRT`. `unknown/bad revision` degrades to empty output.
fn run_git_diff(cwd: &Path, base: Option<&str>, mode: &str) -> std::io::Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    cmd.args(["-c", "core.quotePath=false"]);
    cmd.args(["diff", mode, "--diff-filter=ACDMRT", "--no-renames"]);
    match base {
        None => {
            cmd.arg("--cached");
        }
        Some(r) => {
            cmd.arg(format!("{r}..HEAD"));
        }
    }
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision") || stderr.contains("bad revision") {
            return Ok(String::new());
        }
        return Err(std::io::Error::other(format!(
            "git diff {mode} failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_show(cwd: &Path, gitref: &str, path: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["show", &format!("{gitref}:{path}")])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

fn git_log_messages(cwd: &Path, base_ref: &str) -> Option<String> {
    let range = format!("{base_ref}..HEAD");
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["log", "--format=%B", &range])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

fn print_text(
    fired: &[commitward::Fired],
    acked: &[&commitward::Fired],
    unacked: &[&commitward::Fired],
) {
    if fired.is_empty() {
        println!("No checkpoints fired.");
        return;
    }
    println!(
        "{} checkpoint(s) fired — {} unacked, {} acked",
        fired.len(),
        unacked.len(),
        acked.len()
    );
    for f in unacked {
        println!("  UNACKED  {} — {}", f.name, f.summary);
        for m in &f.matched {
            println!("             matched: {m}");
        }
    }
    for f in acked {
        println!("  acked    {} — {}", f.name, f.summary);
    }
}

fn print_markdown(
    fired: &[commitward::Fired],
    acked: &[&commitward::Fired],
    unacked: &[&commitward::Fired],
) {
    println!("## HITL Checkpoints\n");
    if fired.is_empty() {
        println!("No checkpoints fired.");
        return;
    }
    println!(
        "**{} fired — {} unacked, {} acked**\n",
        fired.len(),
        unacked.len(),
        acked.len()
    );
    if !unacked.is_empty() {
        println!("### Unacknowledged ({})\n", unacked.len());
        for f in unacked {
            println!("- **{}** — {}", f.name, f.summary);
            for m in &f.matched {
                println!("  - Matched: `{m}`");
            }
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use serde_json::{json, Value};

    const REGISTRY: &str = r#"version: "1"
checkpoints:
  - name: touches-claude-md
    summary: edits CLAUDE.md
    paths:
      - "(^|/)CLAUDE\\.md$"
"#;

    fn body_of(out: &str) -> Value {
        let v: Value = serde_json::from_str(out).expect("gate output is JSON");
        assert_eq!(v["schema_version"], "1");
        assert_eq!(v["status"], "ok");
        v["body"].clone()
    }

    #[test]
    fn a_path_checkpoint_fires_and_is_unacked_by_default() {
        let req = json!({
            "name_status": "M\tCLAUDE.md",
            "commit_msg": "docs: tweak CLAUDE.md",
            "global_registry_yaml": REGISTRY,
        })
        .to_string();
        let body = body_of(&gate_envelope(&req).unwrap());
        // Decision is in the body; the process still exits 0 (checked at the CLI layer).
        assert_eq!(body["exit_class"], 2, "an unacked fire blocks");
        assert!(body["fired"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "touches-claude-md"));
        assert_eq!(body["unacked"], json!(["touches-claude-md"]));
    }

    #[test]
    fn a_hitl_ack_trailer_clears_the_fire() {
        let req = json!({
            "name_status": "M\tCLAUDE.md",
            "commit_msg": "docs: tweak CLAUDE.md\n\nHITL-ACK: touches-claude-md intentional",
            "global_registry_yaml": REGISTRY,
        })
        .to_string();
        let body = body_of(&gate_envelope(&req).unwrap());
        // exit_class: 0 none fired · 1 fired-but-all-acked (proceed) · 2 unacked fire (block).
        // An acked fire is informational (1), not a block — the commit proceeds.
        assert_eq!(
            body["exit_class"], 1,
            "an acked fire is informational, not a block"
        );
        assert_eq!(body["unacked"], json!([]));
    }

    #[test]
    fn an_unmatched_diff_does_not_fire() {
        let req = json!({
            "name_status": "M\tsrc/lib.rs",
            "commit_msg": "feat: x",
            "global_registry_yaml": REGISTRY,
        })
        .to_string();
        let body = body_of(&gate_envelope(&req).unwrap());
        assert_eq!(body["exit_class"], 0);
        assert_eq!(body["fired"], json!([]));
    }

    #[test]
    fn invalid_request_json_is_a_hard_error_not_a_false_clean_pass() {
        assert!(gate_envelope("not json").is_err());
    }

    #[test]
    fn a_removed_checkpoint_is_detected_through_the_global_base() {
        // The checkpoint_removed guard fires when a base checkpoint name is absent from the
        // current registry and a checkpoints.yaml is touched. Supplying the removed name ONLY via
        // base_global_registry_yaml must still fire it, proving the global base is unioned in — the
        // native CLI unions base names from both promise/checkpoints.yaml and .dotclaude/.
        let current = "version: \"1\"\ncheckpoints:\n  - name: guard-removed\n    summary: Detect removed checkpoints\n    semantic: checkpoint_removed\n";
        let base_global = "version: \"1\"\ncheckpoints:\n  - name: guard-removed\n    summary: Detect removed checkpoints\n    semantic: checkpoint_removed\n  - name: old-global-guard\n    summary: An old global path guard\n    paths:\n      - \"(^|/)secrets$\"\n";
        let req = json!({
            "name_status": "M\tpromise/checkpoints.yaml",
            "commit_msg": "chore: edit registry",
            "global_registry_yaml": current,
            "base_global_registry_yaml": base_global,
        })
        .to_string();
        let body = body_of(&gate_envelope(&req).unwrap());
        assert!(
            body["fired"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["name"] == "guard-removed"),
            "checkpoint_removed guard should fire on a global-base removal: {body}"
        );
        assert_eq!(body["exit_class"], 2, "an unacked removed-checkpoint blocks");
    }
}
