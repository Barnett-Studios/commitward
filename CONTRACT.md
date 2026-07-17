# commitward — Contract

commitward is a **Policy Gate** component: it turns a high-stakes action (a commit
touching a guarded change) into an allow / human-sign-off decision. This document is the
stable interface. Three front doors wrap **one** core library; all three obey the same
fail-open guarantee.

## The fail-open guarantee (non-negotiable)

> An absent, broken, or misconfigured commitward **degrades**, it never blocks. The only
> outcome that blocks a commit is a *deliberate* fired-and-unacknowledged checkpoint
> (exit 2). Every infrastructure failure — missing binary, missing/unreadable/malformed
> registry, git not present, unknown base ref, malformed diff — resolves to **exit 0**
> (allow), emitting a diagnostic to stderr rather than failing silently.

This is deliberate: correctness never depends on the gate being present or healthy.

## Front door 1 — CLI

```
commitward [OPTIONS]
```

| Option | Default | Meaning |
|---|---|---|
| `--base <ref>` | `origin/main` | diff `<ref>..HEAD` |
| `--cached` | off | diff the staged index against HEAD (used by the commit-msg hook) |
| `--commit-msg-file <path>` | — | file holding the commit message to scan for `HITL-ACK:` trailers |
| `--registry <path>` | `$COMMITWARD_REGISTRY`, else `checkpoints.yaml` beside the binary | global checkpoint baseline |
| `--repo-registry <path>` | `.commitward/checkpoints.yaml` | repo-local overrides (override global by name) |
| `--format <text\|json\|markdown>` | `text` | output format |
| `-h`, `--help` | — | usage |

**Diff semantics:** commitward shells `git diff -c core.quotePath=false --<mode>
--diff-filter=ACDMRT --no-renames`. `--no-renames` is deliberate — a rename of a guarded
file surfaces as delete-old + add-new, so a guard on the *old* path still fires.

**Off switch:** `COMMITWARD_HITL=off` → exit 0 unconditionally.

**Exit codes:** `0` none-fired-or-fail-open · `1` fired+all-acked · `2` fired+unacked ·
`64` usage error.

## Front door 2 — Library crate

```rust
pub fn load_checkpoints(path: &Path) -> Result<Vec<Checkpoint>, CheckpointError>;
pub fn merge(global: Vec<Checkpoint>, repo: Vec<Checkpoint>) -> Vec<Checkpoint>;
pub fn compile(cps: Vec<Checkpoint>) -> Result<Vec<CompiledCheckpoint>, CheckpointError>;
pub fn detect(
    checkpoints: &[CompiledCheckpoint],
    files: &[FileEntry],
    added_lines: &HashMap<String, Vec<String>>,
    base_checkpoint_names: Option<&[String]>,
) -> Vec<Fired>;
pub fn extract_acks(commit_msg: &str) -> Vec<Ack>;
pub fn partition_ack<'a>(fired: &'a [Fired], acks: &[Ack]) -> (Vec<&'a Fired>, Vec<&'a Fired>);
pub fn exit_class(fired_len: usize, unacked_len: usize) -> i32; // 0 | 1 | 2, self-contained
pub fn extract_checkpoint_names(yaml_text: &str) -> Vec<String>;

pub mod gitdiff {
    pub fn parse_name_status(out: &str) -> Vec<crate::FileEntry>;
    pub fn parse_added_lines(diff: &str) -> HashMap<String, Vec<String>>;
}
```

The caller supplies `files` and `added_lines` (pure inputs) — the library never shells git
itself, so it is trivially testable and host-agnostic. Types `Checkpoint`, `Mode`,
`SemanticKind`, `CompiledCheckpoint`, `CheckpointError`, `FileEntry`, `Fired`, `Ack` are
public. Every fallible entry point returns `Result`; nothing panics on hostile input.

## Front door 3 — Container image

`ghcr.io/barnett-studios/commitward`. `ENTRYPOINT ["commitward"]`; the default checkpoint
baseline is baked at `/etc/commitward/checkpoints.yaml` (`COMMITWARD_REGISTRY` points at
it). Mount a repo at `/repo` to gate it. Same flags, same exit codes, same fail-open
guarantee as the CLI.

## Checkpoint registry format

```yaml
version: "1"
checkpoints:
  - name: <unique-id>
    summary: <human description>
    standards_doc: <optional path>       # governing doc, informational
    # exactly one mode:
    paths:    ["<regex over changed paths>"]
    content:  ["<regex over added lines>"]
    content_exempt_paths: ["<regex>"]     # only with `content`
    semantic: checkpoint_removed          # code-driven check
```

Regex is the Rust `regex` crate (linear-time; no look-around / back-references). A repo
checkpoint with the same `name` as a global one replaces it (`merge`).

## Acknowledgement protocol

A `HITL-ACK: <checkpoint-name> <free-text reason>` line anywhere in the commit message
acknowledges that checkpoint's fire. Machine-greppable, auditable, one per fired
checkpoint. An acknowledged fire lifts the block (exit 2 → exit 1); it does not erase the
fire from the report.

## Compatibility

Semver on the crate. The CLI flags, exit codes, registry schema, and the `HITL-ACK`
trailer are the stable public surface; breaking any is a major version bump.
