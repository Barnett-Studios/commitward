# commitward

**A deterministic, fail-open human-sign-off gate for high-stakes commits.**

`commitward` matches a commit's diff against a checkpoint registry and blocks the commit
(exit 2) *only* when a guarded change fires and no one has acknowledged it with a
`HITL-ACK:` trailer in the commit message. It is **fail-open by construction**: a missing
binary, a git error, an unreadable registry — anything that isn't a deliberate,
unacknowledged fire — lets the commit through. The gate never blocks on its own failure.

It is not a linter and not a CI framework. It is one small, greppable primitive:
*approval, not correctness*, for **agentic** commits where a human (or an orchestrator)
must sign off on a narrow set of high-stakes changes.

> Part of the Barnett Studios agentic-harness toolkit → cxpak · **commitward** · abproof · …

## Install

**As a git hook (most common):**

```sh
# from your repo, install a commit-msg hook that runs commitward
./install-hook.sh
```

The hook is fail-open and disables with `COMMITWARD_HITL=off`. If your repo uses a global
`core.hooksPath`, the installer warns and tells you how to target that directory instead.

**As a CLI (crates.io):**

```sh
cargo install commitward
commitward --base origin/main --format markdown
```

**As a container image:**

```sh
docker run --rm -v "$PWD:/repo" ghcr.io/barnett-studios/commitward \
  --base origin/main --format markdown
```

**As a library crate** (in-process, e.g. for another Rust tool):

```toml
[dependencies]
commitward = "0.1"
```

```rust
use commitward::{compile, detect, exit_class, load_checkpoints, merge};
```

## The registry — `checkpoints.yaml`

A checkpoint fires on one of three modes: `paths` (regex over changed file paths),
`content` (regex over *added* lines, with `content_exempt_paths`), or a code-driven
`semantic` check. commitward ships a default global baseline; a repo adds or overrides
checkpoints in `.dotclaude/checkpoints.yaml` (repo entries override global ones by name).

```yaml
version: "1"
checkpoints:
  - name: agent-instructions-self-mod
    summary: "Editing agent-governing instructions"
    paths:
      - "(^|/)CLAUDE\\.md$"
      - "(^|/)rules/.*\\.md$"
  - name: destructive-ops
    summary: "A destructive shell command was added"
    content:
      - "\\brm\\s+-rf\\b"
    content_exempt_paths:
      - "(^|/)docs/"
```

Regex is the Rust `regex` crate: no look-around, no back-references.

## Acknowledging a fire — `HITL-ACK:`

When a checkpoint fires, the commit is blocked (exit 2) until the message carries a
matching trailer:

```
Refactor the gate registry

HITL-ACK: gate-self-mod reviewed with the owner; see ADR-0010
```

An acknowledged fire returns exit **1** (fired, but allowed to proceed) — distinct from
exit **0** (nothing fired). Only exit 2 blocks.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | no checkpoint fired — or any fail-open path (missing git/registry, parse error) |
| `1` | at least one checkpoint fired and **all** fired checkpoints are acknowledged |
| `2` | at least one checkpoint fired and is **unacknowledged** — human sign-off required |
| `64` | usage error (bad flag / missing argument) |

See [`CONTRACT.md`](CONTRACT.md) for the full interface and the fail-open guarantee.
