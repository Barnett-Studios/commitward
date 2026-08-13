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
# An explicit template so `$TMPDIR` is honoured: bare `mktemp -d` ignores it on macOS, and a
# container runtime that only shares part of the filesystem (colima shares `$HOME`) mounts
# anything outside as a silently EMPTY directory — which fails as "the gate evaluated nothing",
# indistinguishable from the defect this script exists to catch. Set TMPDIR to a shared path.
# pwd -P: on a host whose temp dir is a symlink (macOS /var → /private/var), the symlinked
# path is not what the runtime shares either.
WORK="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/commitward-doc-path-XXXXXX")" && pwd -P)"
trap 'rm -rf "$WORK"' EXIT
# A git checkout is 755 under the usual umask; `mktemp -d` is 700, which uid 10001 inside
# the container cannot even traverse. Without this the fixture tests a case the README's
# form does not describe — and fails with the same silent empty stdout, which is how it was
# noticed. The `-u` invocation at the end of this script is the case for a repo that really
# is private.
chmod 755 "$WORK"

# core.hooksPath=/dev/null: the host may have a global commit-msg hook installed — plausibly
# commitward's own — and this fixture must not be gated by it.
g() {
    git -C "$WORK" -c core.hooksPath=/dev/null -c commit.gpgsign=false \
        -c user.email=ci@example.com -c user.name=ci "$@"
}

CODE=0
OUT=""
DOCKER_UID_ARGS=()
run_gate() {
    set +e
    # `${a[@]+"${a[@]}"}` and not `"${a[@]}"`: under `set -u`, bash 3.2 — which is what macOS
    # ships — treats an empty array expansion as an unbound variable and aborts. The CI
    # runner's bash 5 does not, so this only ever failed for someone running it by hand.
    OUT="$(docker run --rm ${DOCKER_UID_ARGS[@]+"${DOCKER_UID_ARGS[@]}"} -v "$WORK:/repo" \
           "$IMAGE" --cached --format json 2>"$WORK/.stderr")"
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

# 2. Staging checkpoints.yaml is the one change `anchor-gate-integrity` guarantees fires. It
#    supplies no registry of its own, so the second grep is a separate fact: `gate-self-mod`
#    lives in the SHIPPED baseline, and it can only fire if the image baked that baseline in
#    and pointed COMMITWARD_REGISTRY at it. The anchor is compiled into the binary and fires
#    either way — an image with no registry at all passes the first grep alone, which is how
#    that omission stayed invisible.
printf 'version: "1"\ncheckpoints: []\n' > "$WORK/checkpoints.yaml"
g add -A
run_gate
if ! printf '%s' "$OUT" | grep -q 'anchor-gate-integrity'; then
    echo "FAIL: staging checkpoints.yaml did not fire the compiled-in anchor: $OUT" >&2
    exit 1
fi
if ! printf '%s' "$OUT" | grep -q 'gate-self-mod'; then
    echo "FAIL: the shipped registry's own checkpoint did not fire — the image has no baked" >&2
    echo "      baseline, so every checkpoint but the compiled-in anchor is inactive: $OUT" >&2
    exit 1
fi
if [ "$CODE" != "2" ]; then
    echo "FAIL: a fired, unacknowledged checkpoint must exit 2, got $CODE: $OUT" >&2
    exit 1
fi

# 3. A repo that is not world-readable. The documented form runs as the image's uid 10001,
#    which cannot traverse a 700 checkout — same silent empty stdout as a missing git. The
#    answer is `-u`, and it has to keep working: commitward only ever reads the repo, so it
#    needs no identity of its own.
chmod 700 "$WORK"
DOCKER_UID_ARGS=(-u "$(id -u):$(id -g)")
run_gate
if [ "$CODE" != "2" ]; then
    echo "FAIL: -u form on a private repo must still fire, got exit $CODE: $OUT" >&2
    exit 1
fi

echo "ok: $IMAGE allows an ordinary change (exit 0), fires on the documented path (exit 2),
    and still fires as -u \$(id -u) on a non-world-readable repo"
