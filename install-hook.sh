#!/usr/bin/env bash
# install-hook.sh — install the commitward commit-msg hook into a git repo.
#
# Idempotent: re-running is a no-op (the hook is rewritten to the same bytes).
# Safe: a pre-existing FOREIGN commit-msg hook is backed up once to
# `commit-msg.pre-commitward` before being replaced.
#
# Hooks dir resolution (in order): first positional arg, then $COMMITWARD_HOOKS_DIR,
# then the repo-local hooks dir `$(git rev-parse --git-dir)/hooks`. It deliberately
# does NOT resolve `core.hooksPath` (which `git rev-parse --git-path hooks` would),
# so running it inside a repo that inherits a *global* hooks path can never clobber
# that global hook — a non-local target must be named explicitly.
set -euo pipefail

MARKER="managed-by: commitward"

hooks_dir="${1:-${COMMITWARD_HOOKS_DIR:-}}"
if [ -z "$hooks_dir" ]; then
    hooks_dir="$(git rev-parse --git-dir)/hooks"
    # If core.hooksPath is set, git ignores .git/hooks — warn so the operator is
    # not surprised by an inert hook, and tell them how to target the real dir.
    configured="$(git config --get core.hooksPath || true)"
    if [ -n "$configured" ]; then
        echo "commitward: WARNING core.hooksPath is set to '$configured'; git will NOT run" >&2
        echo "commitward: the repo-local hook being installed. To install into that path" >&2
        echo "commitward: instead, re-run: install-hook.sh '$configured'" >&2
    fi
fi
mkdir -p "$hooks_dir"
hook="$hooks_dir/commit-msg"

# Back up a pre-existing foreign hook once (never overwrite our own marker file,
# never clobber an existing backup).
if [ -e "$hook" ] && ! grep -q "$MARKER" "$hook" 2>/dev/null; then
    [ -e "$hook.pre-commitward" ] || cp "$hook" "$hook.pre-commitward"
fi

# Write atomically (temp file + mv) so an interrupted install can never leave a
# half-written, unparseable hook — git would treat that as a blocking failure,
# violating the fail-open guarantee.
tmp="$(mktemp "$hooks_dir/.commit-msg.XXXXXX")"
cat > "$tmp" <<'HOOK'
#!/usr/bin/env bash
# managed-by: commitward
# commitward HITL commit-msg hook. Fail-open by design: any problem (missing
# binary, git error, unreadable registry) allows the commit; it blocks only on a
# deliberate exit 2 — a fired, unacknowledged checkpoint. Disable with
# COMMITWARD_HITL=off. Acknowledge a fire with a `HITL-ACK: <name> <reason>`
# trailer in the commit message.
[ "${COMMITWARD_HITL:-on}" = "off" ] && exit 0
bin="$(command -v commitward 2>/dev/null || true)"
[ -z "$bin" ] && exit 0
"$bin" --cached --commit-msg-file "$1"
[ "$?" = "2" ] && exit 2
exit 0
HOOK
chmod +x "$tmp"
mv "$tmp" "$hook"
echo "commitward: installed commit-msg hook at $hook"
