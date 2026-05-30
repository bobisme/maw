#!/usr/bin/env bash
# bn-2ksp regression: `maw migrate` must refuse on a dirty default workspace
# (unless --allow-dirty), and the recovery hint it prints must actually work.
# Uses the `maw` on PATH. Exits non-zero on any failed expectation.
set -u
SB=/tmp/maw-2ksp-repro; rm -rf "$SB"; mkdir -p "$SB"; cd "$SB" || exit 1
git init -q .; git config user.email t@t.t; git config user.name t
echo orig > tracked.txt; git add -A; git commit -qm init
maw init --legacy-ws >/dev/null 2>&1
echo 'UNCOMMITTED EDIT' >> ws/default/tracked.txt
echo 'NEWUNTRACKED' > ws/default/new.txt

fail() { echo "FAIL: $1"; exit 1; }

# 1) dirty + no flag => REFUSE, tree + layout untouched.
if maw migrate >/dev/null 2>&1; then fail "migrate should refuse on dirty default"; fi
grep -q 'UNCOMMITTED EDIT' ws/default/tracked.txt || fail "refusal must leave working tree intact"
[ -d ws ] || fail "refusal must leave v2 layout intact"
echo "ok: refused on dirty, tree+layout intact"

# 2) --allow-dirty => proceeds; printed recovery hint actually restores the work.
out=$(maw migrate --allow-dirty 2>&1)
[ -d .maw ] || fail "--allow-dirty should complete migration"
cmd=$(printf '%s\n' "$out" | grep -oE 'maw ws recover --ref [^ ]+ --to default-prev' | head -1)
[ -n "$cmd" ] || fail "migrate must print a working --ref recovery hint"
eval "$cmd" >/dev/null 2>&1 || fail "the printed recovery command must succeed"
grep -q 'UNCOMMITTED EDIT' .maw/workspaces/default-prev/tracked.txt || fail "recovery must restore the edit"
[ -f .maw/workspaces/default-prev/new.txt ] || fail "recovery must restore the untracked file"
echo "ok: --allow-dirty proceeds; printed recovery hint restores edit + untracked"

# 3) clean tree => migrates without the flag.
SB2=/tmp/maw-2ksp-repro2; rm -rf "$SB2"; mkdir -p "$SB2"; cd "$SB2"
git init -q .; git config user.email t@t.t; git config user.name t
echo x > f.txt; git add -A; git commit -qm init
maw init --legacy-ws >/dev/null 2>&1
maw migrate >/dev/null 2>&1 || fail "clean migrate should succeed without --allow-dirty"
[ -d .maw ] || fail "clean migrate should produce consolidated layout"
echo "ok: clean tree migrates without the flag"

echo "ALL OK (bn-2ksp)"
