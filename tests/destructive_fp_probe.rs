//! Regression gate for the `destructive-ops` HITL checkpoint's false-positive
//! surface (audit 2026-07-16). Runs a battery of git/filesystem command lines
//! through the REAL compiled checkpoint (production `detect()` path) and asserts
//! each fires / does-not-fire per the locked policy. Not `#[ignore]` — this is a
//! standing contract; if a pattern edit regresses it, this test fails.

use commitward::{compile, detect, load_checkpoints, CompiledCheckpoint, FileEntry};
use std::collections::HashMap;

fn fires(compiled: &[CompiledCheckpoint], line: &str) -> bool {
    let files = vec![FileEntry {
        status: 'M',
        path: "scripts/probe.sh".to_string(),
    }];
    let mut added = HashMap::new();
    added.insert("scripts/probe.sh".to_string(), vec![line.to_string()]);
    detect(compiled, &files, &added, None)
        .iter()
        .any(|f| f.name == "destructive-ops")
}

#[test]
fn destructive_ops_fire_policy() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cps = load_checkpoints(&root.join("promise/checkpoints.yaml")).expect("load");
    let compiled = compile(cps).expect("compile");

    // Genuinely destructive — MUST fire.
    let must_fire = [
        "git branch -D old-feature",              // force-delete unmerged
        "git filter-branch --tree-filter x HEAD", // history rewrite
        "git-filter-branch --tree-filter x",      // hyphenated binary form
        "git filter-repo --path secrets",
        "git push --force origin main",
        "git push -f",
        "git checkout --orphan clean-main",
        "git update-ref -d refs/heads/x",
        "git reflog expire --expire=now --all",
        "rm -rf /",
        "rm -rf $DEPLOY",
        // Kept-by-policy (work-destroying, reviewed on purpose — audit 2026-07-16):
        "git clean -fdx",
        "git clean -fd",
        "git reset --hard origin/main",
        "git reset --hard HEAD",
        // Irreducible mention: a string literal CONTAINING a real command still
        // fires — no regex separates mention from execution without a shell
        // parser. Documented limitation; fail-open + human-ack is the backstop.
        "return \"git reset --hard discards work\";",
    ];

    // Benign / silenced / mentions — MUST NOT fire.
    let must_not_fire = [
        "git branch -d merged-feature", // safe merged-only delete (case fix)
        "# do not use filter-branch here", // comment mention (anchor fix)
        "label = \"filter-branch\"",    // string mention (anchor fix)
        "let filter_branch_note = 1;",  // identifier
        "echo prefer --force-with-lease always", // mention (standalone pattern removed)
        "git push --force-with-lease origin f", // safe force variant (silenced)
        "git gc --prune=now",           // maintenance (silenced)
        "git clean -n",                 // dry-run, no -f
        "git reset --soft HEAD~1",      // soft — not destructive
        "git reset HEAD file.txt",      // unstage
        "git push origin main",         // normal push
        "git push --follow-tags origin main", // -f-adjacent word must not trip
        "git push -u origin my-feature-off", // benign branch name (f-ending token)
        "rm -rf ./build",               // benign target (already narrowed)
    ];

    let mut failures = Vec::new();
    for line in must_fire {
        if !fires(&compiled, line) {
            failures.push(format!("SHOULD FIRE but did not: {line:?}"));
        }
    }
    for line in must_not_fire {
        if fires(&compiled, line) {
            failures.push(format!("FALSE POSITIVE (fired): {line:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "destructive-ops policy regressions:\n{}",
        failures.join("\n")
    );
}
