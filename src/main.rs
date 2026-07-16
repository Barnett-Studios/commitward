//! commitward — a deterministic, fail-open human-sign-off gate for high-stakes
//! commits. Shells `git diff` itself, matches a diff against a checkpoint
//! registry, and exits 2 only when a checkpoint fires and is not acknowledged by
//! a `HITL-ACK:` trailer in the commit message. Every infrastructure error
//! (missing git, unreadable registry, malformed diff) degrades to exit 0 — the
//! gate never blocks on its own failure.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use commitward::gitdiff::{parse_added_lines, parse_name_status};
use commitward::{
    compile, detect, exit_class, extract_acks, extract_checkpoint_names, load_checkpoints, merge,
    partition_ack, FileEntry,
};

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
    --repo-registry <path>    Repo-local checkpoint overrides (default: .dotclaude/checkpoints.yaml)
    --format <text|json|markdown>   Output format (default: text)
    -h, --help                Print this help

EXIT CODES:
    0   no checkpoint fired, all fired checkpoints acknowledged, or any fail-open path
    2   at least one checkpoint fired and is unacknowledged (human sign-off required)
   64   usage error (bad flag / missing argument)

Disable entirely with COMMITWARD_HITL=off.";

fn main() {
    let code = run();
    std::process::exit(code);
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

    let repo_path = repo_registry.unwrap_or_else(|| repo_root.join(".dotclaude/checkpoints.yaml"));
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
        .unwrap_or(Path::new(".dotclaude/checkpoints.yaml"));
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

/// Shell `git diff` with the same flags the framework uses: `--no-renames`
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
