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
    checkpoint_removed_is_compiled, compile, detect_with_registry_paths, exit_class, extract_acks,
    extract_checkpoint_names, load_checkpoints, merge, parse_checkpoints, partition_ack,
    Checkpoint, FileEntry, Mode, SemanticKind,
};
use serde::Deserialize;

/// The registry this crate ships (`checkpoints.yaml` at the repo root), embedded at
/// build time. `cargo install` places only the `[[bin]]` target — nothing puts this
/// file beside the installed binary, so `default_registry()`'s resolution is
/// unreachable on that route and every shipped checkpoint went inactive (commitward#30).
/// `include_str!` bakes the content in regardless of install method, as a FALLBACK
/// used only when nothing more specific was asked for: an explicit `--registry` or
/// `$COMMITWARD_REGISTRY` that points at a missing file still fails open with the old
/// warning, exactly as before — this only covers the case where neither was given and
/// the default path (beside the binary) doesn't exist either.
const SHIPPED_REGISTRY: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/checkpoints.yaml"));

const USAGE: &str = "\
commitward — deterministic fail-open HITL commit gate

USAGE:
    commitward [OPTIONS]

OPTIONS:
    --base <ref>              Base ref to diff against HEAD (default: origin/main)
    --cached                  Diff the staged index against HEAD instead of a base ref
    --commit-msg-file <path>  File holding the commit message to scan for HITL-ACK trailers
    --registry <path>         Global checkpoint baseline (default: $COMMITWARD_REGISTRY,
                              else checkpoints.yaml next to the binary, else the
                              compiled-in shipped baseline if neither is reachable)
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
    /// The repo-relative path `global_registry_yaml` was loaded from, when it is not named
    /// `checkpoints.yaml` (commitward#24). The `gate` request has no filesystem to resolve a
    /// path from — the `checkpoint_removed` guard's only other signal is the suffix
    /// convention, so a registry under any other name needs the caller to name it, the same
    /// parity `detect_with_registry_paths` already gives the native CLI via `--registry`.
    #[serde(default)]
    global_registry_path: Option<String>,
    /// As `global_registry_path`, for `repo_registry_yaml` (the native CLI's `--repo-registry`).
    #[serde(default)]
    repo_registry_path: Option<String>,
}

/// Per-registry-side verifiability for `checkpoint_removed`, shared by both front doors
/// (commitward#24 review rounds 2 and 3 — the native CLI's `run()` carried its own copy of
/// the same diff-touched heuristic the `gate` envelope was redesigned away from, with the
/// identical bug: it warned on nearly every ordinary commit and could be silenced by an
/// unrelated decoy file matching the `checkpoints.yaml` suffix convention).
///
/// Purely structural, no diff involved: a side is unverifiable when the caller explicitly
/// named a path for it (`--registry`/`$COMMITWARD_REGISTRY` or `--repo-registry`;
/// `global_registry_path`/`repo_registry_path` on the `gate` request) but no base content
/// was found there. An ordinary commit that never names a custom path never reaches this —
/// the default `checkpoints.yaml` convention is already covered by the suffix rule inside
/// `detect_with_registry_paths` itself.
///
/// `sides` is `(label, path_named, base_found)` per registry side. Returns the shared
/// `body.warnings`/stderr message, or `None` if every named side is verifiable.
fn unverifiable_registry_warning(sides: &[(&str, bool, bool)]) -> Option<String> {
    let unverifiable: Vec<&str> = sides
        .iter()
        .filter(|(_, named, found)| *named && !*found)
        .map(|(label, _, _)| *label)
        .collect();
    if unverifiable.is_empty() {
        return None;
    }
    Some(format!(
        "checkpoint_removed cannot verify removal for: {} — a checkpoint deleted from that \
         registry will not be detected this run",
        unverifiable.join("; ")
    ))
}

fn gate_envelope(input: &str) -> Result<String, String> {
    // Global off switch (commitward#21): --help says "Disable entirely with
    // COMMITWARD_HITL=off", and the native CLI honours that on its own path (see `run()`),
    // but `gate` is reached through the same binary and the same documented switch — a
    // consumer should not have to know there's a second place this needs setting. Exit
    // stays 0/`status: ok` (ADR-0052: only an infrastructure error is non-zero here), and
    // `exit_class` goes to 0 so a disabled gate can never read as a block. Fail-open is not
    // fail-silent (CONTRACT.md): the warning says the gate did not evaluate, so this is not
    // mistaken for "evaluated and clean".
    if std::env::var("COMMITWARD_HITL").as_deref() == Ok("off") {
        let body = serde_json::json!({
            "fired": Vec::<serde_json::Value>::new(),
            "unacked": Vec::<String>::new(),
            "exit_class": 0,
            // Machine-readable, not just the English in `warnings`: `exit_class: 0` with
            // `fired: []` is indistinguishable from an ordinary clean pass unless a
            // consumer parses prose. `bypassed: true` here, `false` on every other path
            // (never absent — a field a consumer must remember to check for is a field
            // they will eventually forget to check for) is the one thing to test instead.
            "bypassed": true,
            "warnings": ["COMMITWARD_HITL=off — the gate did not evaluate; no checkpoint could fire"],
        });
        return Ok(ok_envelope(body));
    }

    let req: GateRequest =
        serde_json::from_str(input).map_err(|e| format!("invalid gate request JSON: {e}"))?;

    // Inlined YAML → checkpoints. `load_checkpoints` takes a path, so materialize each registry to a
    // self-cleaning temp file; an absent registry is an empty set (fail-open, mirrors the native CLI).
    let global_cps = load_inlined_registry(req.global_registry_yaml.as_deref(), "global")?;
    let repo_cps = load_inlined_registry(req.repo_registry_yaml.as_deref(), "repo")?;
    // Asked *before* compile, which now always adds the compiled-in anchor (commitward#9).
    // The NF3 warning below is about what the caller supplied — "you configured nothing" is
    // still true and still worth saying when the only thing standing is the anchor.
    let no_registry_supplied = global_cps.is_empty() && repo_cps.is_empty();
    let compiled =
        compile(merge(global_cps, repo_cps)).map_err(|e| format!("registry compile error: {e}"))?;

    let files = parse_name_status(&req.name_status);
    let added = parse_added_lines(&req.diff);
    // Union base checkpoint names from both the repo and global base registries, mirroring the
    // native CLI (`local_base.union(&global_base)`). `None` only when neither was supplied.
    let base_names: Option<Vec<String>> =
        match (&req.base_repo_registry_yaml, &req.base_global_registry_yaml) {
            (None, None) => None,
            (repo, global) => {
                let mut names: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                if let Some(y) = repo {
                    names.extend(extract_checkpoint_names(y));
                }
                if let Some(y) = global {
                    names.extend(extract_checkpoint_names(y));
                }
                Some(names.into_iter().collect())
            }
        };

    // commitward#24: `checkpoint_removed`'s only other signal besides the `checkpoints.yaml`
    // suffix is a caller-named path — the `gate` request has no filesystem to resolve one
    // from, so a registry under any other name needs it supplied explicitly.
    let registry_paths: Vec<String> = [&req.global_registry_path, &req.repo_registry_path]
        .into_iter()
        .filter_map(|p| p.clone())
        .collect();

    let fired = detect_with_registry_paths(
        &compiled,
        &files,
        &added,
        base_names.as_deref(),
        &registry_paths,
    );
    let acks = extract_acks(&req.commit_msg);
    let (_acked, unacked) = partition_ack(&fired, &acks);
    let ec = exit_class(fired.len(), unacked.len());

    // NF3: name every guard that could not run. A clean `exit_class: 0` means "nothing
    // fired", which a consumer reads as "nothing to worry about" — so the result has to
    // say which checks were not performed, or the two are indistinguishable.
    let mut warnings: Vec<String> = Vec::new();
    if no_registry_supplied {
        warnings.push(
            "no checkpoints were supplied (global_registry_yaml / repo_registry_yaml both \
             absent or empty) — only the compiled-in gate-integrity anchor applies, so every \
             commit that does not touch the gate's own files passes"
                .to_string(),
        );
    }
    if base_names.is_none() {
        warnings.push(
            "no base registry supplied (base_repo_registry_yaml / base_global_registry_yaml) \
             — the checkpoint-removed guard is INACTIVE for this call, so a checkpoint \
             deleted in this change will not be detected"
                .to_string(),
        );
    }
    // commitward#24, second half, redesigned per review: the first cut warned off a
    // diff-touched heuristic (`registry_touched`), which fired on every ordinary commit
    // (nothing touches the registry, so "not touched" was always true) and could be
    // silenced by an unrelated decoy file matching the `checkpoints.yaml` suffix while a
    // real deletion in the actually-named registry went unreported. The replacement asks a
    // purely structural question with no diff involved at all: for each side the caller
    // EXPLICITLY named a custom path for (`global_registry_path`/`repo_registry_path`), is
    // that side's own base content (`base_global_registry_yaml`/`base_repo_registry_yaml`)
    // actually present? If a path is named but its base is missing, `checkpoint_removed`
    // cannot verify that specific registry regardless of what the diff says — and if no
    // custom path was ever named, the default `checkpoints.yaml` convention already covers
    // it without needing this check at all, so an ordinary commit never reaches it.
    let global_label = format!(
        "global registry at {:?} has no base_global_registry_yaml",
        req.global_registry_path.as_deref().unwrap_or_default()
    );
    let repo_label = format!(
        "repo registry at {:?} has no base_repo_registry_yaml",
        req.repo_registry_path.as_deref().unwrap_or_default()
    );
    let guard_unverified_warning = checkpoint_removed_is_compiled(&compiled)
        .then(|| {
            unverifiable_registry_warning(&[
                (
                    global_label.as_str(),
                    req.global_registry_path.is_some(),
                    req.base_global_registry_yaml.is_some(),
                ),
                (
                    repo_label.as_str(),
                    req.repo_registry_path.is_some(),
                    req.base_repo_registry_yaml.is_some(),
                ),
            ])
        })
        .flatten();
    let guard_unverified = guard_unverified_warning.is_some();
    if let Some(w) = guard_unverified_warning {
        warnings.push(w);
    }

    // commitward#20: the warnings above cover the REGISTRY inputs and said nothing about the
    // DIFF inputs. Omitting `diff` silenced every content-mode checkpoint, and omitting
    // `name_status` silenced every checkpoint of both modes, while the envelope still read
    // `status: "ok"`, `fired: []`, `exit_class: 0` — a clean pass for a change the gate never
    // saw. Same class as #4 (base registry) and #7 (parse error), on the one input dimension
    // those did not cover.
    //
    // The absent/empty distinction is not recoverable from the request (`String` +
    // `serde(default)`) and does not need to be: the actionable condition is observable after
    // compile — a checkpoint of a given mode is present and the input that mode reads is
    // empty. Behaviour does not change here. `exit_class` and the fail-open posture are
    // untouched; only the reporting is.
    //
    // Names, not a count: "1 guard could not run" sends the reader back to the registry to
    // work out which one. The compiled-in anchor is not excluded — it is a real checkpoint
    // and it is silenced by exactly the same omission.
    //
    // And the set is derived from the inputs each mode actually CONSUMES, not from the
    // request field that shares the mode's name. The first version of this warning paired
    // `diff`→content and `name_status`→path, which is what the field names suggest and not
    // what the evaluator does: `Mode::Content` iterates `files` and only then looks up the
    // added lines, so an empty `name_status` silences it as surely as an empty `diff`, and
    // `Semantic(CheckpointRemoved)` reads `files` too (via `has_registry_touch`) and was in
    // neither list. A request carrying `diff` and no `name_status` therefore got a warning
    // that named a DIFFERENT checkpoint than the one silenced — a list authoritative enough
    // to be trusted, and wrong.
    //
    // Exhaustive match, no wildcard: a new `Mode` variant is a compile error here rather
    // than a checkpoint that is silently absent from every warning.
    /// Which request input is missing, for a checkpoint that cannot run without it.
    enum Missing {
        Paths,
        Added,
    }
    fn silenced_by(mode: &Mode, no_paths: bool, no_added: bool) -> Option<Missing> {
        match mode {
            Mode::Path(_) | Mode::Semantic(SemanticKind::CheckpointRemoved) => {
                no_paths.then_some(Missing::Paths)
            }
            // Paths first: with no changed files the content patterns are never reached, so
            // that is the input to report as missing.
            Mode::Content { .. } => {
                if no_paths {
                    Some(Missing::Paths)
                } else if no_added {
                    Some(Missing::Added)
                } else {
                    None
                }
            }
        }
    }
    if files.is_empty() && added.is_empty() {
        // Distinct from "guard X could not run": nothing was evaluated at all. A request
        // carrying a registry and a commit message but no change returned `exit_class: 0`,
        // and the only warning was about the base registry.
        warnings.push(
            "no change was supplied (diff / name_status both absent or empty) — NOTHING was \
             evaluated, so this result reports that no checkpoint fired against an empty \
             change, not that the change is clean"
                .to_string(),
        );
    } else {
        let mut by_paths: Vec<&str> = Vec::new();
        let mut by_added: Vec<&str> = Vec::new();
        for c in &compiled {
            match silenced_by(&c.mode, files.is_empty(), added.is_empty()) {
                Some(Missing::Paths) => by_paths.push(c.name.as_str()),
                Some(Missing::Added) => by_added.push(c.name.as_str()),
                None => {}
            }
        }
        if !by_paths.is_empty() {
            warnings.push(format!(
                "no changed paths were supplied (`name_status` absent or empty) — these \
                 checkpoint(s) could NOT run and cannot fire: {}",
                by_paths.join(", ")
            ));
        }
        if !by_added.is_empty() {
            // Not "`diff` absent or empty": a binary-only change carries a `diff` that is
            // neither, with no added lines in it. What silences these is the absence of
            // added lines, which is what the sentence now says.
            warnings.push(format!(
                "the change carried no added lines (`diff` has none) — these content-mode \
                 checkpoint(s) could NOT run and cannot fire: {}",
                by_added.join(", ")
            ));
        }
    }

    let unacked_names: Vec<&str> = unacked.iter().map(|f| f.name.as_str()).collect();
    let body = serde_json::json!({
        "fired": &fired,
        "unacked": unacked_names,
        "exit_class": ec,
        "bypassed": false,
        // Structured, not just the English in `warnings`: a consumer that wants to branch
        // on "can this call's checkpoint_removed result be trusted" without parsing prose.
        "guard_unverified": guard_unverified,
        "warnings": warnings,
    });
    Ok(ok_envelope(body))
}

/// Load an inlined-YAML registry via a self-cleaning temp file.
///
/// A parse error **propagates** (NF3, commitward#7). It used to be swallowed into an empty
/// checkpoint set, which meant a malformed registry produced a clean, `status: "ok"` pass —
/// a security control reporting success precisely when it could not run. That is the
/// fail-*silent* direction, and it is the one failure mode a gate must never have.
///
/// The caller turns this into an `error` envelope, which ADR-0052 defines as "do not trust
/// this result, fall back to your in-process path". The system still fails open overall —
/// commitward is not a blocking control — but it now does so audibly at both layers instead
/// of silently at one.
///
/// An **absent** registry is still an empty set: supplying nothing is a configuration
/// choice, supplying something unparseable is a defect.
fn load_inlined_registry(yaml: Option<&str>, label: &str) -> Result<Vec<Checkpoint>, String> {
    match yaml {
        Some(y) => {
            let tmp = write_temp_yaml(y, label)?;
            load_checkpoints(&tmp.0).map_err(|e| format!("{label} registry failed to parse: {e}"))
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
    //
    // "Nothing more specific was asked for" means no --registry AND no
    // $COMMITWARD_REGISTRY — the latter is checked again here, not only inside
    // `default_registry()`, because the embedded fallback below must NOT trigger
    // when an operator named a specific path that happens to be missing (that stays
    // the old INACTIVE warning: they asked for something, and it isn't there).
    let used_default_path = registry.is_none() && std::env::var("COMMITWARD_REGISTRY").is_err();
    // commitward#24 review round 3: "was a custom path named for this side" — captured
    // before `registry` is consumed below. `!used_default_path` is exactly that: neither
    // `--registry` nor `$COMMITWARD_REGISTRY` given means the default convention path
    // applies, which the checkpoint_removed suffix rule already covers without this check.
    let global_path_named = !used_default_path;
    let global_path = registry.unwrap_or_else(default_registry);
    let global_cps = if !global_path.exists() {
        if used_default_path {
            eprintln!(
                "commitward: NOTE global checkpoint registry not found at {} — using the \
                 compiled-in default baseline (commitward#30). Place a checkpoints.yaml there, \
                 or set COMMITWARD_REGISTRY / pass --registry, to use a different one.",
                global_path.display()
            );
            parse_checkpoints(SHIPPED_REGISTRY).unwrap_or_else(|e| {
                eprintln!(
                    "commitward: compiled-in default baseline failed to parse (fail-open): {e}"
                );
                vec![]
            })
        } else {
            eprintln!(
                "commitward: WARNING global checkpoint registry not found at {} — \
                 global baseline INACTIVE (only repo-local overrides apply). Pass --registry or \
                 set COMMITWARD_REGISTRY. (fail-open: continuing)",
                global_path.display()
            );
            vec![]
        }
    } else {
        match load_checkpoints(&global_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "commitward: registry load error (fail-open): {}: {e}",
                    global_path.display()
                );
                vec![]
            }
        }
    };

    let repo_path_named = repo_registry.is_some();
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
    // Read each registry as it stood at the base ref; a checkpoint that exists at base but
    // not now was removed.
    //
    // BOTH registries, unioned — not just the repo one (commitward#4). The gate envelope
    // unions `base_repo_registry_yaml` and `base_global_registry_yaml`, and its comment said
    // it was "matching the native CLI"; the native CLI read only the repo registry, so a
    // checkpoint that lived in the global one and was deleted was detected through one front
    // door and not the other. The comment asserting the parity is what kept that invisible.
    //
    // A registry outside the repo tree is skipped rather than failed on: `git show <ref>:
    // <path>` can only address paths inside the work tree, so an installed baseline at
    // /etc/commitward has no base version to read and never did.
    // WHETHER THE REF RESOLVES is the discriminator, not whether a registry was found at it.
    //
    // `git show <ref>:<path>` fails for two reasons that must not be conflated. If the ref
    // resolves and the file is not there, that is an ANSWER — the base declared no
    // checkpoints, so nothing can have been removed from it. That is the ordinary state of
    // the commit that first adopts a registry. If the ref does not resolve at all — a shallow
    // clone, an unknown base, a repository with no commits — there is no answer to be had and
    // the guard cannot run.
    //
    // Only the second is `None`. Keying off the read instead would warn on every adoption
    // commit, and a warning that fires on the ordinary path is one operators learn to skip.
    let name_ref: &str = if cached { "HEAD" } else { base_ref.as_str() };
    let base_ref_resolves = git_rev_parse_commit(&repo_root, name_ref);
    let mut base_names: HashSet<String> = HashSet::new();
    // Per-path "is this registry verifiable at base" (commitward#24 review round 3) —
    // tracked separately from `base_names` itself, which only records WHICH checkpoints
    // existed, not WHERE. This is NOT "did `git_show` return content": per the comment
    // above, an ABSENT file at a resolving ref is itself a determined answer (zero
    // checkpoints there — the ordinary state of a commit that first adopts the registry),
    // not an unverifiable one. The only way a named, in-repo path is *unverifiable* once
    // the ref resolves is if it is OUTSIDE the repo tree, where `git show` cannot address
    // it at all — a different, already-documented limitation. Defaults to `true`
    // (verifiable) so a path this loop never reaches (ref does not resolve; the other
    // branch handles that case entirely) is never mistaken for unverifiable here.
    let mut repo_base_found = true;
    let mut global_base_found = true;
    if base_ref_resolves {
        for (path, found) in [
            (&repo_path, &mut repo_base_found),
            (&global_path, &mut global_base_found),
        ] {
            let Some(rel) = repo_relative(path, &repo_root) else {
                *found = false; // outside the repo: no base version exists to read
                continue;
            };
            if let Some(text) = git_show(&repo_root, name_ref, &rel) {
                base_names.extend(extract_checkpoint_names(&text));
            }
        }
    }

    // `None` — not `Some(&[])` — when the ref did not resolve. They are different claims:
    // `Some(&[])` says the base declared no checkpoints, so nothing can have been removed.
    // Passing it after a failed lookup reports a check that never ran as a check that passed,
    // which is the one failure mode CONTRACT.md says a gate must not have. The CLI did
    // exactly that on every shallow clone and every unknown base ref, because
    // `git_show(..).unwrap_or_default()` turned "could not find out" into "found nothing".
    let base_arg: Option<Vec<String>> = if base_ref_resolves {
        Some(base_names.into_iter().collect())
    } else {
        None
    };

    // The registries in play, so a `$COMMITWARD_REGISTRY` under a non-standard name is still
    // recognised as a registry when it changes (the library's fallback only knows the
    // `checkpoints.yaml` suffix).
    let registry_paths: Vec<String> = [&repo_path, &global_path]
        .iter()
        .filter_map(|p| repo_relative(p, &repo_root))
        .collect();

    let fired = detect_with_registry_paths(
        &compiled,
        &files,
        &added,
        base_arg.as_deref(),
        &registry_paths,
    );

    // Fail-open is not fail-silent: name the guard that could not run. Conditional on the
    // guard being COMPILED — on a registry that declares no `checkpoint_removed` there is
    // nothing to disable, and a warning on the ordinary path is one operators learn to skip.
    let mut warnings: Vec<String> = Vec::new();
    if base_arg.is_none() && checkpoint_removed_is_compiled(&compiled) {
        warnings.push(format!(
            "checkpoint-removed guard INACTIVE for this run ({name_ref} does not resolve to a \
             commit) — a checkpoint deleted in this change will not be detected. Usual \
             causes: a shallow clone, an unknown base ref, or a repository with no commits."
        ));
    } else if base_arg.is_some() && checkpoint_removed_is_compiled(&compiled) {
        // commitward#24 review round 3: the previous version of this branch repeated the
        // `gate` envelope's own first-cut mistake — a diff-touched heuristic
        // (`registry_touched`) that fired on nearly every ordinary commit (nothing in an
        // unrelated change touches the registry, so "not touched" was almost always true)
        // and could be silenced by an unrelated decoy file matching the `checkpoints.yaml`
        // suffix while a real deletion in the actually-named registry went unreported.
        // `unverifiable_registry_warning` is the same structural check `gate` was
        // redesigned to use: warn only when `--registry`/`$COMMITWARD_REGISTRY` or
        // `--repo-registry` named a path and no base content was found for it — never
        // diff-based, so an ordinary commit that touches nothing never reaches a warning.
        let global_label = format!(
            "global registry at {} has no base content at {name_ref}",
            global_path.display()
        );
        let repo_label = format!(
            "repo registry at {} has no base content at {name_ref}",
            repo_path.display()
        );
        if let Some(w) = unverifiable_registry_warning(&[
            (global_label.as_str(), global_path_named, global_base_found),
            (repo_label.as_str(), repo_path_named, repo_base_found),
        ]) {
            warnings.push(w);
        }
    }
    for w in &warnings {
        eprintln!("commitward: WARNING {w} (fail-open: continuing)");
    }

    // ── Acks. ──────────────────────────────────────────────────────────────
    let commit_msg = match &commit_msg_file {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => git_log_messages(&repo_root, &base_ref).unwrap_or_default(),
    };
    let acks = extract_acks(&commit_msg);
    let (acked, unacked) = partition_ack(&fired, &acks);

    match format.as_str() {
        "json" => {
            // `warnings` is additive — a consumer reading `fired`/`acked`/`unacked` is
            // unaffected, and one that only had stderr to go on now has the same signal the
            // gate envelope already carried.
            let obj = serde_json::json!({
                "fired": &fired, "acked": &acked, "unacked": &unacked, "warnings": &warnings,
            });
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

/// A registry's path relative to the repository root, or `None` if it lies outside.
///
/// NOT a bare `strip_prefix`. `git rev-parse --show-toplevel` returns a canonical path while
/// the path a caller passes usually is not, and on macOS a repo under `$TMPDIR` differs by a
/// whole `/private` prefix. A bare strip fails there and reports "no registry lies inside the
/// repository" — turning the checkpoint-removed guard off, with a warning stating a reason
/// that is not the real one, in precisely the environment the tests run in.
///
/// The registry FILE is deliberately not canonicalized — only its directory. A registry
/// deleted by the very change being gated no longer exists in the work tree, and reading it
/// at the base ref is the whole point.
///
/// Canonicalizing the registry's directory is the half that carries this; mutating it away
/// turns the tests red. Canonicalizing `repo_root` is a no-op whenever `git_toplevel()`
/// answered — git returns a canonical path — and is kept for the branch where it did not and
/// `current_dir()` supplied the root instead, which carries no such guarantee. That branch
/// has no test, so the line is defensive rather than pinned, and saying so beats implying a
/// coverage it does not have.
fn repo_relative(path: &Path, repo_root: &Path) -> Option<String> {
    let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let abs = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    };
    abs.strip_prefix(&root)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

/// Does `gitref` name a commit in this repository?
///
/// The question that separates "the base declared nothing" from "there is no base to ask" —
/// the two states the checkpoint-removed guard must not conflate. `^{commit}` so a tag or a
/// tree does not answer yes for something that cannot be diffed against.
fn git_rev_parse_commit(cwd: &Path, gitref: &str) -> bool {
    Command::new("git")
        .current_dir(cwd)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{gitref}^{{commit}}"),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        assert_eq!(
            body["exit_class"], 2,
            "an unacked removed-checkpoint blocks"
        );
    }
}
