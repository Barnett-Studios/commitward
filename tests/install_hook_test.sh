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

# ── Case 3b: the SECOND foreign hook is kept too (commitward#31) ───────────
# The old guard was `[ -e "$hook.pre-commitward" ] || cp …`, so once a backup existed the
# copy was skipped — while `replaced_foreign` was still set, the hook still overwritten, and
# the run still reported success naming a backup path that held the FIRST hook. `.git/hooks/`
# is not under version control, so a hand-written second hook was simply gone.
#
# Reached by any ordinary sequence where something writes `commit-msg` after commitward has
# been installed once and the installer is re-run — husky, pre-commit, lefthook, or a person.
r2b="$root/foreign-twice"; mkdir -p "$r2b"; ( cd "$r2b" && git init -q )
mkdir -p "$r2b/.git/hooks"
h2b="$r2b/.git/hooks/commit-msg"

printf '#!/bin/sh\n# FIRSTHOOK\ngrep -q "TICKET-" "$1" || exit 1\n' > "$h2b"
chmod +x "$h2b"
( cd "$r2b" && bash "$installer" >/dev/null 2>&1 )
grep -q "FIRSTHOOK" "$h2b.pre-commitward" || fail "3b: first foreign hook not backed up"

printf '#!/bin/sh\n# SECONDHOOK\ngrep -q "JIRA-" "$1" || exit 1\n' > "$h2b"
chmod +x "$h2b"
out3b="$( cd "$r2b" && bash "$installer" 2>&1 )"

# The content must survive SOMEWHERE under the repo — the assertion is about not destroying
# it, not about the name, so a different rotation scheme still passes.
if ! grep -rq "SECONDHOOK" "$r2b"; then
  fail "3b: the second foreign hook was destroyed with no copy kept"
fi
# The first must still be there too: rotating must not mean overwriting.
grep -rq "FIRSTHOOK" "$r2b" || fail "3b: rotating the backup destroyed the first hook"

# And the message must name the path that actually holds the hook just replaced. Naming a
# path that holds someone else's hook is the one thing that stops an operator noticing.
named="$(printf '%s\n' "$out3b" | sed -n 's/^commitward: \(.*commit-msg\.pre-commitward[^ ]*\).*/\1/p' | head -1)"
[ -n "$named" ] || fail "3b: the installer named no backup path for the replaced hook"
# The installer names the path as the operator would use it — relative to the repo they ran
# it in — so resolve it against that repo before reading it.
case "$named" in /*) named_abs="$named" ;; *) named_abs="$r2b/$named" ;; esac
grep -q "SECONDHOOK" "$named_abs" \
  || fail "3b: the message names $named, which does not hold the hook just replaced"

# A THIRD foreign hook must be kept too. A fixed second name (`.1`) passes everything above
# while clobbering the second hook's backup on the third install — found by mutation, not by
# reasoning about it.
printf '#!/bin/sh\n# THIRDHOOK\ngrep -q "OPS-" "$1" || exit 1\n' > "$h2b"
chmod +x "$h2b"
( cd "$r2b" && bash "$installer" >/dev/null 2>&1 )
for want in FIRSTHOOK SECONDHOOK THIRDHOOK; do
  grep -rq "$want" "$r2b" || fail "3b: $want was destroyed by a later install"
done

# CONTROL — the message on the FIRST replacement still names a file holding that hook, so
# this is not satisfied by an installer that reports nothing at all.
named1="$(printf '%s\n' "$( cd "$root" && true )" )"
r2c="$root/foreign-once"; mkdir -p "$r2c"; ( cd "$r2c" && git init -q )
mkdir -p "$r2c/.git/hooks"
printf '#!/bin/sh\n# ONLYHOOK\n' > "$r2c/.git/hooks/commit-msg"; chmod +x "$r2c/.git/hooks/commit-msg"
out3c="$( cd "$r2c" && bash "$installer" 2>&1 )"
named1="$(printf '%s\n' "$out3c" | sed -n 's/^commitward: \(.*commit-msg\.pre-commitward[^ ]*\).*/\1/p' | head -1)"
[ -n "$named1" ] || fail "3b control: no backup path named on the first replacement"
case "$named1" in /*) named1_abs="$named1" ;; *) named1_abs="$r2c/$named1" ;; esac
grep -q "ONLYHOOK" "$named1_abs" || fail "3b control: first-replacement message names the wrong file"

# ── Case 4: the binary is absent → say so, and still exit 0 ────────────────
# commitward#23. The README's first and self-described "most common" install is the hook
# form; the binary is installed in the NEXT section, framed as a different way to use the
# tool rather than a prerequisite. A reader who follows the order gets a hook whose
# `command -v commitward` is empty — fail-open by design, so every commit is allowed — and
# an installer whose last line is `installed`. The installer knows: `command -v commitward`
# is the exact expression it writes INTO the hook.
r3="$root/nobin"; mkdir -p "$r3"; ( cd "$r3" && git init -q )
minimal_path=/usr/bin:/bin
PATH="$minimal_path" command -v commitward >/dev/null 2>&1 \
  && fail "case 4 is vacuous on this host: commitward is on $minimal_path"
out4="$( cd "$r3" && PATH="$minimal_path" bash "$installer" 2>&1 )" \
  || fail "the installer must still exit 0 when the binary is absent"
printf '%s' "$out4" | grep -q "not on PATH" \
  || fail "an absent binary must be reported at install time: $out4"
printf '%s' "$out4" | grep -qE "cargo install commitward|brew install commitward" \
  || fail "the absent-binary message must name the fix: $out4"

# ── Case 5: control — the binary IS present, so the notice must not fire ────
# Without this, an installer that printed the warning unconditionally passes case 4.
stubdir="$root/stub"; mkdir -p "$stubdir"
printf '#!/bin/sh\nexit 0\n' > "$stubdir/commitward"; chmod +x "$stubdir/commitward"
r4="$root/withbin"; mkdir -p "$r4"; ( cd "$r4" && git init -q )
out5="$( cd "$r4" && PATH="$stubdir:$minimal_path" bash "$installer" 2>&1 )"
printf '%s' "$out5" | grep -q "not on PATH" \
  && fail "the absent-binary notice fired with the binary on PATH: $out5"

# ── Case 6: replacing a foreign hook is announced, with the backup path ─────
# The backup is a file, not a chain: nothing ever runs `commit-msg.pre-commitward` again,
# so the operator's previous policy is OFF from this moment. That is a deliberate design
# choice, and it is the operator's to know about at the moment it happens.
r5="$root/foreign2"; mkdir -p "$r5"; ( cd "$r5" && git init -q )
mkdir -p "$r5/.git/hooks"
printf '#!/bin/sh\necho foreign\n' > "$r5/.git/hooks/commit-msg"
out6="$( cd "$r5" && PATH="$stubdir:$minimal_path" bash "$installer" 2>&1 )"
printf '%s' "$out6" | grep -q "pre-commitward" \
  || fail "replacing a foreign hook must be reported, with the backup path: $out6"
printf '%s' "$out6" | grep -qE "replaced|no longer run" \
  || fail "the message must say the previous hook no longer runs: $out6"

# ── Case 7: control — a fresh install replaced nothing and must not say so ──
out7="$( cd "$r4" && PATH="$stubdir:$minimal_path" bash "$installer" 2>&1 )"
printf '%s' "$out7" | grep -q "pre-commitward" \
  && fail "a fresh install claimed to have replaced a hook: $out7"

# ── Case 8: core.hooksPath pointing at the dir being installed into ─────────
# commitward#23, secondary. The condition was `[ -n "$configured" ]` — set, not
# DIFFERENT — so the installer warned "git will NOT run the repo-local hook being
# installed" about the very directory it was installing into, and prescribed re-running
# with the path it had just used. A HITL gate telling an operator "this will not run" when
# it will is the wrong direction to be wrong in.
r6="$root/samepath"; mkdir -p "$r6"; ( cd "$r6" && git init -q )
( cd "$r6" && git config core.hooksPath .git/hooks )
out8="$( cd "$r6" && PATH="$stubdir:$minimal_path" bash "$installer" 2>&1 )"
printf '%s' "$out8" | grep -q "will NOT run" \
  && fail "the hooksPath warning fired for the directory being installed into: $out8"

# ── Case 9: control — a genuinely different hooksPath must still warn ───────
r7="$root/otherpath"; mkdir -p "$r7"; ( cd "$r7" && git init -q )
mkdir -p "$r7/elsewhere"
( cd "$r7" && git config core.hooksPath elsewhere )
out9="$( cd "$r7" && PATH="$stubdir:$minimal_path" bash "$installer" 2>&1 )"
printf '%s' "$out9" | grep -q "will NOT run" \
  || fail "a hooksPath pointing somewhere else must still warn: $out9"

echo "PASS: install-hook.sh (fresh, idempotent, foreign-backup, absent-binary notice,"
echo "      foreign-replacement notice, hooksPath warning fires only when it is true)"
