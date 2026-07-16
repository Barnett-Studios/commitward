#!/usr/bin/env bash
# Test install-hook.sh: installs into a temp repo, is executable, idempotent,
# and backs up a pre-existing foreign hook once. No external test framework.
set -euo pipefail

# Hermetic: ignore the operator's global/system git config entirely, so a global
# core.hooksPath cannot redirect the installer at the user's real hooks dir.
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null

here="$(cd "$(dirname "$0")" && pwd)"
installer="$here/../install-hook.sh"
[ -f "$installer" ] || { echo "FAIL: installer not found at $installer"; exit 1; }

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT

fail() { echo "FAIL: $1"; exit 1; }

# ── Case 1: fresh install → hook exists, is executable, carries the marker ──
r1="$root/fresh"; mkdir -p "$r1"; ( cd "$r1" && git init -q )
( cd "$r1" && bash "$installer" >/dev/null )
hook="$r1/.git/hooks/commit-msg"
[ -x "$hook" ] || fail "hook not installed or not executable"
grep -q "managed-by: commitward" "$hook" || fail "marker missing from installed hook"

# ── Case 2: idempotent → second run leaves the hook byte-identical ──────────
sum1="$(shasum "$hook" | awk '{print $1}')"
( cd "$r1" && bash "$installer" >/dev/null )
sum2="$(shasum "$hook" | awk '{print $1}')"
[ "$sum1" = "$sum2" ] || fail "re-install changed the hook (not idempotent)"
# ...and a foreign backup is NOT created for our own hook
[ -e "$hook.pre-commitward" ] && fail "backed up our own hook on re-run"

# ── Case 3: pre-existing foreign hook is backed up once ─────────────────────
r2="$root/foreign"; mkdir -p "$r2"; ( cd "$r2" && git init -q )
mkdir -p "$r2/.git/hooks"
printf '#!/bin/sh\necho foreign\n' > "$r2/.git/hooks/commit-msg"
( cd "$r2" && bash "$installer" >/dev/null )
backup="$r2/.git/hooks/commit-msg.pre-commitward"
[ -e "$backup" ] || fail "foreign hook not backed up"
grep -q "foreign" "$backup" || fail "backup has wrong content"
grep -q "managed-by: commitward" "$r2/.git/hooks/commit-msg" || fail "commitward hook not installed over foreign"
# re-run does not overwrite the existing backup
( cd "$r2" && bash "$installer" >/dev/null )
grep -q "foreign" "$backup" || fail "backup clobbered on re-run"

echo "PASS: install-hook.sh (fresh, idempotent, foreign-backup)"
