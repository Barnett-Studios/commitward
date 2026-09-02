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
warn_hooks_path=0
if [ -z "$hooks_dir" ]; then
    hooks_dir="$(git rev-parse --git-dir)/hooks"
    warn_hooks_path=1
fi
mkdir -p "$hooks_dir"
hook="$hooks_dir/commit-msg"

# If core.hooksPath points SOMEWHERE ELSE, git ignores the dir we install into — warn so
# the operator is not surprised by an inert hook, and name the real dir.
#
# SET is not DIFFERENT (commitward#23). The condition was `[ -n "$configured" ]`, so a repo
# whose core.hooksPath is `.git/hooks` — the very directory being installed into — got
# "git will NOT run the repo-local hook being installed" about a hook git does run, and a
# remedy prescribing the path already in use. Both paths are resolved before comparing, so
# it is directories being compared and not spellings; git resolves a relative hooksPath
# from the top level, and so does this.
if [ "$warn_hooks_path" = 1 ]; then
    configured="$(git config --get core.hooksPath || true)"
    if [ -n "$configured" ]; then
        case "$configured" in
            /*) configured_abs="$configured" ;;
            *)  configured_abs="$(git rev-parse --show-toplevel)/$configured" ;;
        esac
        configured_real="$(cd "$configured_abs" 2>/dev/null && pwd -P || printf '%s' "$configured_abs")"
        hooks_real="$(cd "$hooks_dir" && pwd -P)"
        if [ "$configured_real" != "$hooks_real" ]; then
            echo "commitward: WARNING core.hooksPath is set to '$configured'; git will NOT run" >&2
            echo "commitward: the repo-local hook being installed. To install into that path" >&2
            echo "commitward: instead, re-run: install-hook.sh '$configured'" >&2
        fi
    fi
fi

# Back up a pre-existing foreign hook once (never overwrite our own marker file,
# never clobber an existing backup).
replaced_foreign=0
if [ -e "$hook" ] && ! grep -q "$MARKER" "$hook" 2>/dev/null; then
    [ -e "$hook.pre-commitward" ] || cp "$hook" "$hook.pre-commitward"
    replaced_foreign=1
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

# Say what just happened, at the one moment the operator can act on it (commitward#23).
# Both facts were known here and neither was reported: the install printed one
# unconditional success line while the repo went from one enforced commit-msg policy to
# none.

# The backup is a file, not a chain — nothing ever executes it again. Chaining a foreign
# hook is its own hazard and is deliberately not done; what is not deliberate is the
# operator finding out from a dot-file in .git/hooks.
if [ "$replaced_foreign" = 1 ]; then
    echo "commitward: replaced an existing commit-msg hook; the previous one is saved at" >&2
    echo "commitward: $hook.pre-commitward and will NO LONGER RUN — re-wire it yourself if" >&2
    echo "commitward: its policy still applies" >&2
fi

# Fail-open is right at RUNTIME — a gate that breaks the build because its binary is
# missing would be worse. It is not right at INSTALL time, where the same expression the
# hook uses can be evaluated and reported. Still exit 0: the installer must not require the
# binary, only say when it is not there.
if ! command -v commitward >/dev/null 2>&1; then
    echo "commitward: NOTE commitward is not on PATH, so this hook will ALLOW EVERY COMMIT" >&2
    echo "commitward: until you install it:  brew install commitward  |  cargo install commitward" >&2
fi
