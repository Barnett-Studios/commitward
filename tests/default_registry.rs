//! The **shipped** default registry, exercised as a consumer gets it.
//!
//! `corpus_detect_cases` in `src/lib.rs` covers the detection *engine* against a
//! fixture registry under `tests/corpus/`. That fixture carries `gate-self-mod` and
//! `checkpoint-removed`; the registry commitward actually ships did not. So the engine
//! was green while the product shipped with its self-protection off — the exact shape
//! of NF2 (commitward#7).
//!
//! Every test here loads `checkpoints.yaml` from the repo root: the file the Docker
//! image and the installers place beside the executable.

use commitward::{compile, detect, load_checkpoints, Checkpoint, FileEntry, Fired};
use std::collections::HashMap;
use std::path::PathBuf;

fn shipped() -> Vec<Checkpoint> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("checkpoints.yaml");
    load_checkpoints(&path).expect("the shipped default registry must load")
}

fn modified(path: &str) -> FileEntry {
    FileEntry {
        status: 'M',
        path: path.to_string(),
    }
}

fn fire(cps: Vec<Checkpoint>, files: &[FileEntry], base: Option<&[String]>) -> Vec<String> {
    let compiled = compile(cps).expect("the shipped registry must compile");
    let added: HashMap<String, Vec<String>> = HashMap::new();
    let mut names: Vec<String> = detect(&compiled, files, &added, base)
        .iter()
        .map(|f: &Fired| f.name.clone())
        .collect();
    names.sort();
    names
}

fn fire_with_added(
    cps: Vec<Checkpoint>,
    files: &[FileEntry],
    added: HashMap<String, Vec<String>>,
    base: Option<&[String]>,
) -> Vec<String> {
    let compiled = compile(cps).expect("the shipped registry must compile");
    let mut names: Vec<String> = detect(&compiled, files, &added, base)
        .iter()
        .map(|f: &Fired| f.name.clone())
        .collect();
    names.sort();
    names
}

fn names_of(cps: &[Checkpoint]) -> Vec<String> {
    cps.iter().map(|c| c.name.clone()).collect()
}

#[test]
fn shipped_registry_guards_its_own_default_path() {
    let fired = fire(shipped(), &[modified("checkpoints.yaml")], None);
    assert!(
        fired.contains(&"gate-self-mod".to_string()),
        "editing the registry the gate reads must fire gate-self-mod; got {fired:?}"
    );
}

#[test]
fn shipped_registry_guards_the_repo_local_override() {
    // `.commitward/checkpoints.yaml` is where `main.rs` looks for the repo registry, so
    // it is a second way to weaken the gate and must be guarded too.
    let fired = fire(shipped(), &[modified(".commitward/checkpoints.yaml")], None);
    assert!(
        fired.contains(&"gate-self-mod".to_string()),
        "the repo-local registry is equally load-bearing; got {fired:?}"
    );
}

#[test]
fn shipped_registry_guards_the_installed_hook() {
    // The commit-msg hook is what invokes the gate at all. Deleting it is a quieter way
    // to disable the gate than editing any checkpoint.
    let fired = fire(shipped(), &[modified(".git-hooks/commit-msg")], None);
    assert!(
        fired.contains(&"gate-self-mod".to_string()),
        "the hook that invokes the gate must itself be guarded; got {fired:?}"
    );
}

#[test]
fn shipped_registry_detects_a_removed_checkpoint() {
    let base = names_of(&shipped());
    let mut weakened = shipped();
    weakened.retain(|c| c.name != "destructive-ops");
    let fired = fire(
        weakened,
        &[modified("checkpoints.yaml")],
        Some(base.as_slice()),
    );
    assert!(
        fired.contains(&"checkpoint-removed".to_string()),
        "removing a checkpoint from the registry must fire checkpoint-removed; got {fired:?}"
    );
}

#[test]
fn nf2_deleting_the_guard_and_using_it_in_one_commit_does_not_pass_silently() {
    // The headline NF2 scenario, end to end: the agent removes `destructive-ops` and
    // introduces `rm -rf /` in the SAME commit. `destructive-ops` cannot fire — it no
    // longer exists — so the only thing standing between this commit and a clean gate is
    // the registry's self-protection.
    let base = names_of(&shipped());
    let mut weakened = shipped();
    weakened.retain(|c| c.name != "destructive-ops");

    let mut added: HashMap<String, Vec<String>> = HashMap::new();
    added.insert(
        "scripts/cleanup.sh".to_string(),
        vec!["rm -rf / --no-preserve-root".to_string()],
    );
    let files = vec![modified("checkpoints.yaml"), modified("scripts/cleanup.sh")];

    let fired = fire_with_added(weakened, &files, added, Some(base.as_slice()));
    assert!(
        !fired.is_empty(),
        "an agent that deletes the destructive-ops checkpoint and adds `rm -rf /` in one \
         commit must not sail through a silent gate — nothing fired"
    );
    assert!(
        fired.contains(&"checkpoint-removed".to_string())
            && fired.contains(&"gate-self-mod".to_string()),
        "both guards should catch this independently, so neither is a single point of \
         failure; got {fired:?}"
    );
}

#[test]
fn shipped_registry_stays_quiet_on_ordinary_work() {
    // Guard: the tests above would all pass on a registry that fired on everything. A
    // normal source edit must still produce a clean gate, or the gate becomes noise and
    // gets switched off — which is the real-world failure mode for a checkpoint system.
    let files = vec![modified("src/handler.rs"), modified("README.md")];
    let base = names_of(&shipped());
    let fired = fire(shipped(), &files, Some(base.as_slice()));
    assert!(
        fired.is_empty(),
        "ordinary work must not fire anything; got {fired:?}"
    );
}

#[test]
fn shipped_registry_still_catches_destructive_content() {
    // Guard: adding the new checkpoints must not have disturbed the existing ones.
    let mut added: HashMap<String, Vec<String>> = HashMap::new();
    added.insert(
        "scripts/cleanup.sh".to_string(),
        vec!["rm -rf / --no-preserve-root".to_string()],
    );
    let fired = fire_with_added(shipped(), &[modified("scripts/cleanup.sh")], added, None);
    assert!(
        fired.contains(&"destructive-ops".to_string()),
        "the pre-existing destructive-ops checkpoint must still fire; got {fired:?}"
    );
}
