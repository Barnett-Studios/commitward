#!/usr/bin/env bash
# The README's container form, run against a real image: mount a repo, read it with git,
# fire a checkpoint (commitward#13).
#
# Usage: tests/container_documented_path.sh <image-tag>
#
# This is the check that was missing. The `gate` subcommand takes the diff and the registry
# as JSON on stdin, so it needs neither git nor a baked registry — CI exercised it, it
# passed, and the *documented* path (mounted repo + the git-reading CLI) was inert in the
# published image for every diff there has ever been.
#
# Non-empty stdout is asserted separately, and first. The failure mode of this path is
# silence: git is missing, or refuses the foreign-owned mount, the gate fails open as
# designed, and the process exits 0 having printed nothing at all. A `grep` for a checkpoint
# name does fail on that — but it fails in a way a reader would misattribute to the
# checkpoint logic rather than to the image.
#
# Both directions are checked. A gate that fires on everything passes an assertion that only
# looks for exit 2, and is as useless as one that fires on nothing.
set -euo pipefail

IMAGE="${1:?usage: container_documented_path.sh <image-tag>}"
# pwd -P: on a host whose temp dir is a symlink (macOS /var → /private/var), the symlinked
# path is not what the container runtime shares.
WORK="$(cd "$(mktemp -d)" && pwd -P)"
trap 'rm -rf "$WORK"' EXIT

# core.hooksPath=/dev/null: the host may have a global commit-msg hook installed — plausibly
# commitward's own — and this fixture must not be gated by it.
g() {
    git -C "$WORK" -c core.hooksPath=/dev/null -c commit.gpgsign=false \
        -c user.email=ci@example.com -c user.name=ci "$@"
}

CODE=0
OUT=""
run_gate() {
    set +e
    OUT="$(docker run --rm -v "$WORK:/repo" "$IMAGE" --cached --format json 2>"$WORK/.stderr")"
    CODE=$?
    set -e
    if [ -z "$OUT" ]; then
        echo "FAIL: empty stdout — the gate never evaluated anything (exit $CODE)." >&2
        # First lines only: a failed `git diff` prints its whole usage screen, and the
        # diagnostic that identifies the cause is the first line.
        echo "      stderr: $(head -3 "$WORK/.stderr")" >&2
        exit 1
    fi
}

g init -q .
echo x > "$WORK/f.txt"
g add -A
g commit -qm init

# 1. An ordinary edit must NOT block, and must still produce a report.
echo more >> "$WORK/f.txt"
g add -A
run_gate
if [ "$CODE" != "0" ]; then
    echo "FAIL: an ordinary edit must not block, got exit $CODE: $OUT" >&2
    exit 1
fi

# 2. Staging checkpoints.yaml is the one change `anchor-gate-integrity` guarantees fires, and
#    it supplies no registry of its own — so this also proves the image's baseline is reachable.
printf 'version: "1"\ncheckpoints: []\n' > "$WORK/checkpoints.yaml"
g add -A
run_gate
if ! printf '%s' "$OUT" | grep -q 'anchor-gate-integrity'; then
    echo "FAIL: staging checkpoints.yaml did not fire the compiled-in anchor: $OUT" >&2
    exit 1
fi
if [ "$CODE" != "2" ]; then
    echo "FAIL: a fired, unacknowledged checkpoint must exit 2, got $CODE: $OUT" >&2
    exit 1
fi

echo "ok: $IMAGE allows an ordinary change (exit 0) and fires on the documented path (exit 2)"
