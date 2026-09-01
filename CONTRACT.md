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

**Fail-open is not fail-silent.** A gate that cannot evaluate must say so; a check that did not run
must never be reported as a check that passed. Concretely (commitward#7):

- The **CLI** keeps exit 0 on a malformed registry, with a stderr diagnostic — unchanged.
- The **`gate` envelope** returns `status: "error"` and a non-zero exit when a *supplied* registry
  will not parse. Under ADR-0052 that tells the consumer "do not trust this result, fall back to
  your in-process path", so the system still fails open — audibly at both layers rather than
  silently at one. An **absent** registry remains an empty set: supplying nothing is a
  configuration choice, supplying something unparseable is a defect.
- Every `ok` envelope carries `body.warnings`, naming the guards that could not run — for the
  **change** inputs as well as the registry ones (commitward#20). A request that omits `diff`
  cannot fire any content-mode checkpoint, and one that omits `name_status` cannot fire **any
  checkpoint at all** — content and semantic modes both read the changed-file list before they
  read anything else. Both used to return `fired: []`, `exit_class: 0` and warn only about the
  registry, which is a clean pass for a change the gate never saw. A request carrying neither
  gets its own line, because "nothing was evaluated" is a different statement from "guard X could
  not run".

  Each warning names the checkpoints the **missing input** actually silences, derived from what
  each mode consumes rather than from the request field that shares the mode's name. The first
  implementation paired `diff`↔content and `name_status`↔path, so a request with `diff` and no
  `name_status` named one silenced checkpoint while omitting another that was equally silenced —
  a list authoritative enough to be trusted, and wrong. The checkpoints are **named**, not
  counted, and the warning is conditional on such a checkpoint actually being compiled, so a
  registry that declares no content checkpoints is not warned about added lines. Behaviour is
  unchanged: `exit_class` and the fail-open posture are exactly what they were.

**The default registry carries self-protection, with a documented residual.** The shipped
`checkpoints.yaml` carries `gate-self-mod` (path) and `checkpoint-removed` (semantic), so removing
*a* checkpoint and exercising what it guarded in the same commit fires two independent guards rather
than nothing.

Those two entries do not survive removal of themselves — they live in the file they guard, and
`checkpoint-removed` additionally needs base checkpoint names, so with no resolvable base ref it
cannot fire at all. A registry cannot be the sole thing that protects the registry.

**It now says so instead of passing quietly (commitward#4).** Two states were being conflated. An
unresolvable base ref — a shallow clone, an unknown base, a repository with no commits — means the
guard *did not run*; a base ref that resolves to a commit with no registry means it ran and found
nothing to have been removed. Only the first is `base_checkpoint_names: None`, and only the first
warns. The CLI used to turn a failed lookup into an empty set and pass `Some(&[])`, so an
un-runnable guard was indistinguishable from a clean one — the fail-*silent* direction, at the one
front door the `gate` envelope's `body.warnings` did not cover. The CLI's `--format json` now
carries the same `warnings` array, and the warning is conditional on a `checkpoint_removed` entry
actually being compiled: on a registry that declares none, nothing was disabled, and a warning on
the ordinary path is one operators learn to skip.

**A registry is recognised by the paths in play, not only by its filename.** `checkpoint-removed`
requires the change to touch a registry, and the library's default test is the `checkpoints.yaml`
suffix. A registry located by `$COMMITWARD_REGISTRY` or `--registry` may be named anything, so
editing it to delete a checkpoint used to be invisible — the same blind spot recorded below for
`gate-self-mod`, but with no operator escape because the test was hard-coded. The CLI now names its
registries via `detect_with_registry_paths`, additively to the suffix rule. The CLI also unions base
names from **both** registries; it previously read only the repo one, so a checkpoint removed from
the global registry was caught by the `gate` envelope and not by the CLI.

**So one checkpoint is not in the registry.** `compile()` merges `anchor_checkpoints()` —
`anchor-gate-integrity`, compiled into the binary — into *every* registry, including an empty one,
and applies it last so a same-named on-disk entry cannot shadow it. It watches the gate's own files
(`checkpoints.yaml` at any depth, `.commitward/checkpoints.yaml`, any file named `commit-msg` at
any depth, `install-hook.sh`).

The hook pattern is deliberately not a list of hook-directory conventions. `install-hook.sh` takes
the hooks directory as its first argument (else `$COMMITWARD_HOOKS_DIR`, else the repo-local hooks
dir), so the set of installable locations is open and no enumeration can be complete — the previous
list named `.git-hooks/` and `.git/hooks/`, the second of which git can never show in a diff because
nothing under `.git/` is tracked, while `.githooks/`, `.husky/` and dotclaude's own
`scripts/git-hooks/` fired nothing (commitward#14). `commit-msg` names exactly one thing in a git
repo; `commit-msg.sample` and the installer's `commit-msg.pre-commitward` backup do not match. There is no edit to a YAML file that removes it, and no registry at all is still
not an unguarded gate.

Consequences worth stating: a commit that touches a registry or the hook now **always** fires at
least one checkpoint, including the commit that first adopts a registry — acknowledge it with a
`HITL-ACK` line like any other. And the anchor is deliberately narrow: it covers the gate's own
integrity, not policy. An anchor that grew to cover policy would be a second registry that no repo
could declare or amend, which is the thing this design exists to avoid.

Still open: `residual_gap_adr0010_checkpoint_removed_itself_removed` — a removed
`checkpoint_removed` entry still produces no *semantic* fire. The anchor covers the act (the file
changed), not the semantics of what was removed from it.

This remains an honest-operator control, not an adversarial one — the acknowledgement protocol below
is self-acknowledgeable by the committing agent, by design.

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

Both registry paths, plus the installed `commit-msg` hook and `install-hook.sh`, are guarded by the
default `gate-self-mod` checkpoint. A registry located via `$COMMITWARD_REGISTRY` cannot be matched
by a static pattern — add its path to `gate-self-mod` yourself if you use that variable.
| `--format <text\|json\|markdown>` | `text` | output format. `json` is `{fired, acked, unacked, warnings}` — `warnings` names the guards that could not run, matching the `gate` envelope's `body.warnings` |
| `-h`, `--help` | — | usage |

**Diff semantics:** commitward shells `git diff -c core.quotePath=false --<mode>
--diff-filter=ACDMRT --no-renames`. `core.quotePath=false` stops git escaping bytes ≥ 0x80 and **only**
those — a path containing `"`, `\`, a tab or a newline is C-quoted whatever that flag says.
Both parsers decode that quoting (`gitdiff::unquote_c_style`) so the two views of the diff key
on the same string. They did not, and the content join `added_lines[path]` missed: the file's
added lines were never scanned against the denylist and the run exited 0 like any clean commit
(commitward#3). A `+++` header's trailing tab is git's own path delimiter and exactly one is
stripped — trimming all trailing whitespace ate a space belonging to the path itself. `--no-renames` is deliberate — a rename of a guarded
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
    base_checkpoint_names: Option<&[String]>,   // None = could not find out; Some(&[]) = base held nothing
) -> Vec<Fired>;
pub fn detect_with_registry_paths(                // as `detect`, plus the repo-relative
    checkpoints: &[CompiledCheckpoint],           // registry paths in play, so a registry
    files: &[FileEntry],                          // named anything else is still recognised
    added_lines: &HashMap<String, Vec<String>>,   // (additive to the `checkpoints.yaml` rule)
    base_checkpoint_names: Option<&[String]>,
    registry_paths: &[String],
) -> Vec<Fired>;
pub fn checkpoint_removed_is_compiled(checkpoints: &[CompiledCheckpoint]) -> bool;
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

Three things the image must carry for that to be true, and it is one contract for **both**
Dockerfiles — the source build and the published `Dockerfile.dist`. Measured on the images
themselves (commitward#13), staging a `checkpoints.yaml` — the change the anchor guarantees
fires:

| missing | result |
|---|---|
| `git` (the CLI shells `git diff`) | fail-open, exit 0, **empty stdout** — no verdict, any diff |
| `git config --system safe.directory '*'` | same silence: the mount belongs to the host user, not uid 10001, and git answers `fatal: detected dubious ownership`. Installing git is **not** sufficient on its own |
| the baked baseline at `/etc/commitward/checkpoints.yaml` | not silent — the compiled-in anchor still fires (exit 2), but every checkpoint in the shipped registry is inactive, because the CLI's default is "beside the binary", a path no container populates |

The published image shipped without git and without the baseline for its whole life, because
the only container path under test was `gate`, which takes the diff and the registry on stdin
and needs neither. `tests/container_documented_path.sh` now runs the documented form against
both images in CI, and asserts non-empty stdout separately from the verdict — the failure mode
of this path is silence, not a wrong answer.

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
