#!/usr/bin/env bash
# Consolidated-layout invariant torture harness (no LLM, deterministic).
#
# Drives the full consolidated `.maw/` lifecycle end-to-end and ASSERTS the
# Prime Invariant + structural invariants at every destructive step:
#
#   - .git is a real, non-corrupt repo after every destroy/merge/migrate
#     (this is the bn-3bkn regression guard — `ws merge --destroy` once
#      gutted the git dir; that must never silently happen again)
#   - HEAD is always reachable; `git fsck` reports no missing/broken objects
#   - every destroyed workspace leaves a recovery ref (Prime Invariant)
#   - merged work actually lands at root (integration, not just no-loss)
#   - no conflict markers ever leak into tracked files at root
#
# Exit code: 0 = all invariants held; 1 = at least one violation (details above).
# Re-runnable. Each scenario gets its own fresh /tmp sandbox.
#
# Usage:
#   notes/consolidated-sim.sh                # run all scenarios
#   notes/consolidated-sim.sh --keep         # keep sandboxes for inspection
#   notes/consolidated-sim.sh greenfield     # run a single scenario by name
#
# Scenarios: greenfield  lifecycle  recover  migrate  post-migrate-merge

set -uo pipefail   # NOTE: deliberately NOT -e — assertions handle failures
                   # and we never want a masked exit code (the original
                   # bn-3bkn investigation was confused by a grep|pipe mask).

MAW="${MAW_BIN:-maw}"
KEEP=0
SANDBOXES=()
PASS=0
FAIL=0
FAILED_CHECKS=()

# ---- output helpers ---------------------------------------------------------
c_red()   { printf '\033[31m%s\033[0m' "$1"; }
c_grn()   { printf '\033[32m%s\033[0m' "$1"; }
c_dim()   { printf '\033[2m%s\033[0m' "$1"; }
hdr()     { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

ok()   { PASS=$((PASS+1)); printf '  %s %s\n' "$(c_grn ok)" "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILED_CHECKS+=("$1"); printf '  %s %s\n' "$(c_red FAIL)" "$1"; }
warn() { printf '  %s %s\n' "$(printf '\033[33mwarn\033[0m')" "$1"; }   # known gap, not a failure

# ---- sandbox lifecycle ------------------------------------------------------
mksandbox() {
  local d
  d="$(mktemp -d "/tmp/maw-csim-$1.XXXX")"
  SANDBOXES+=("$d")
  printf '%s' "$d"
}

cleanup() {
  if [[ "$KEEP" -eq 1 ]]; then
    printf '\n%s\n' "$(c_dim "kept sandboxes:")"
    for s in "${SANDBOXES[@]}"; do printf '  %s\n' "$s"; done
  else
    for s in "${SANDBOXES[@]}"; do rm -rf "$s"; done
  fi
}
trap cleanup EXIT

git_id() {  # configure a deterministic git identity in $PWD
  git config user.name "Sim Bot"
  git config user.email sim@local
  git config commit.gpgsign false
  git config tag.gpgsign false
}

# ---- invariant assertions ---------------------------------------------------
# All take an explicit repo dir so they work on root or workspace dirs.

assert_git_intact() {  # <dir> <label>
  # Layout-agnostic: .git may be a real dir (consolidated / normal) OR a
  # gitfile pointing at repo.git (v2 bare). Either is fine — the bn-3bkn
  # "gutted" failure is caught by rev-parse/HEAD/fsck below, which fail hard
  # when the git dir is destroyed regardless of layout.
  local dir="$1" label="$2"
  if [[ ! -e "$dir/.git" ]]; then
    bad "$label: .git is MISSING entirely (gutted?)"; return
  fi
  if ! git -C "$dir" rev-parse --git-dir >/dev/null 2>&1; then
    bad "$label: rev-parse --git-dir fails (corrupt)"; return
  fi
  if ! git -C "$dir" rev-parse HEAD >/dev/null 2>&1; then
    bad "$label: HEAD is not reachable"; return
  fi
  local fsck
  fsck="$(git -C "$dir" fsck --connectivity-only 2>&1)"
  if grep -Eq 'missing|broken|corrupt' <<<"$fsck"; then
    bad "$label: fsck reports damage:\n$fsck"; return
  fi
  ok "$label: .git intact (HEAD reachable, fsck clean)"
}

assert_not_bare() {  # <dir> <label>
  local dir="$1" label="$2"
  if [[ "$(git -C "$dir" rev-parse --is-bare-repository 2>&1)" == "false" ]]; then
    ok "$label: repo is non-bare (normal .git)"
  else
    bad "$label: repo is bare (expected consolidated non-bare)"
  fi
}

assert_consolidated_shape() {  # <dir> <label>
  local dir="$1" label="$2"
  [[ -d "$dir/.maw/workspaces" ]] && ok "$label: .maw/workspaces/ exists" \
                                  || bad "$label: .maw/workspaces/ missing"
  [[ ! -e "$dir/repo.git" ]] && ok "$label: no repo.git holdover" \
                             || bad "$label: repo.git still present (should be normal .git)"
}

assert_file_contains() {  # <file> <pattern> <label>
  if [[ -f "$1" ]] && grep -q "$2" "$1"; then
    ok "$3"
  else
    bad "$3 (file=$1 pattern=$2)"
  fi
}

assert_file_absent() {  # <file> <label>
  [[ ! -e "$1" ]] && ok "$2" || bad "$2 (still present: $1)"
}

assert_recovery_ref() {  # <dir> <name> <label>
  local dir="$1" name="$2" label="$3"
  if git -C "$dir" for-each-ref "refs/manifold/recovery/$name/" 2>/dev/null | grep -q .; then
    ok "$label"
  else
    bad "$label (no refs/manifold/recovery/$name/*)"
  fi
}

assert_no_markers_at_root() {  # <dir> <label>
  local dir="$1" label="$2" hits
  hits="$(grep -rIn -e '^<<<<<<<' -e '^>>>>>>>' -e '^=======$' "$dir" \
            --exclude-dir=.git --exclude-dir=.maw 2>/dev/null || true)"
  if [[ -z "$hits" ]]; then
    ok "$label"
  else
    bad "$label — markers leaked:\n$hits"
  fi
}

# =============================================================================
# Scenario 1: greenfield consolidated init
# =============================================================================
scn_greenfield() {
  hdr "greenfield: empty dir -> maw init -> consolidated"
  local sb; sb="$(mksandbox greenfield)"
  ( cd "$sb" && "$MAW" init >/dev/null 2>&1 )
  assert_not_bare "$sb" "greenfield"
  assert_consolidated_shape "$sb" "greenfield"
  assert_git_intact "$sb" "greenfield"
}

# =============================================================================
# Scenario 2: full lifecycle incl. epoch sync + conflict-as-data
# =============================================================================
scn_lifecycle() {
  hdr "lifecycle: 2 overlapping workspaces, merge, auto-rebase conflict, resolve"
  local sb; sb="$(mksandbox lifecycle)"
  (
    cd "$sb"
    "$MAW" init >/dev/null 2>&1
    git_id
    printf 'l1\nl2\nl3\n' > shared.txt
    printf 'fn main(){}\n' > main.rs
    git add -A && git commit -qm "seed source"
    # Direct-to-default commit diverges from epoch -> must sync before workspaces.
    "$MAW" epoch sync >/dev/null 2>&1
    "$MAW" ws create alice --from main >/dev/null 2>&1
    "$MAW" ws create bob   --from main >/dev/null 2>&1
    printf 'l1\nALICE\nl3\n' > .maw/workspaces/alice/shared.txt
    "$MAW" exec alice -- git commit -qam "alice edits l2"
    printf 'l1\nBOB\nl3\n'   > .maw/workspaces/bob/shared.txt
    "$MAW" exec bob   -- git commit -qam "bob edits l2"
    # merge alice -> advances epoch, auto-rebases bob (conflict-as-data)
    "$MAW" ws merge alice --into default --destroy --message "feat: alice" >/dev/null 2>&1
  )
  assert_git_intact "$sb" "lifecycle/post-merge-alice"
  assert_file_contains "$sb/shared.txt" "ALICE" "lifecycle: alice's edit landed at root"
  assert_recovery_ref  "$sb" alice "lifecycle: alice recovery ref pinned"

  # bob should now be conflicted (auto-rebase recorded a conflict, not aborted)
  if "$MAW" --version >/dev/null 2>&1 && ( cd "$sb" && "$MAW" ws list 2>&1 | grep -q 'bob.*conflicted' ); then
    ok "lifecycle: bob is conflicted-as-data (rebase did not abort)"
  else
    bad "lifecycle: bob not in expected conflicted state"
  fi

  (
    cd "$sb"
    "$MAW" ws resolve bob --keep both >/dev/null 2>&1   # auto-commits resolution
    "$MAW" ws merge bob --into default --destroy --message "feat: bob" >/dev/null 2>&1
  )
  assert_git_intact "$sb" "lifecycle/post-merge-bob"
  assert_file_contains "$sb/shared.txt" "ALICE" "lifecycle: alice still present after bob merge"
  assert_file_contains "$sb/shared.txt" "BOB"   "lifecycle: bob's edit landed at root"
  assert_recovery_ref  "$sb" bob "lifecycle: bob recovery ref pinned"
  assert_no_markers_at_root "$sb" "lifecycle: no conflict markers leaked to root"
}

# =============================================================================
# Scenario 3: destroy --force -> recover --to (Prime Invariant)
# =============================================================================
scn_recover() {
  hdr "recover: destroy --force then recover --to a new workspace"
  local sb; sb="$(mksandbox recover)"
  (
    cd "$sb"
    "$MAW" init >/dev/null 2>&1
    git_id
    printf 'fn main(){}\n' > main.rs
    git add -A && git commit -qm "seed"
    "$MAW" epoch sync >/dev/null 2>&1
    "$MAW" ws create carol --from main >/dev/null 2>&1
    printf 'pub fn precious(){}\n' > .maw/workspaces/carol/precious.rs
    "$MAW" exec carol -- git add -A
    "$MAW" exec carol -- git commit -qm "feat: precious work"
    "$MAW" ws destroy carol --force >/dev/null 2>&1
  )
  assert_git_intact "$sb" "recover/post-destroy"
  assert_recovery_ref "$sb" carol "recover: carol recovery ref pinned"
  assert_file_absent "$sb/.maw/workspaces/carol" "recover: carol workspace removed"
  ( cd "$sb" && "$MAW" ws recover carol --to carol2 >/dev/null 2>&1 )
  assert_file_contains "$sb/.maw/workspaces/carol2/precious.rs" "precious" \
    "recover: precious work restored into carol2 (Prime Invariant)"
  assert_git_intact "$sb" "recover/post-restore"
}

# =============================================================================
# Scenario 4: migrate v2 bare -> consolidated (dirty-refuse + --allow-dirty)
# =============================================================================
scn_migrate() {
  hdr "migrate: v2 bare -> consolidated (refuse dirty, then --allow-dirty)"
  local sb; sb="$(mksandbox migrate)"
  (
    cd "$sb"
    git init -b main -q
    git_id
    printf 'fn main(){}\n' > main.rs
    printf 'l1\nl2\n' > shared.txt
    git add -A && git commit -qm "seed"
    "$MAW" init >/dev/null 2>&1          # brownfield existing repo -> v2 bare
  )
  # confirm we actually got v2 bare to migrate from
  if [[ "$(git -C "$sb" rev-parse --is-bare-repository 2>&1)" == "true" ]]; then
    ok "migrate: starting layout is v2 bare"
  else
    bad "migrate: expected v2 bare start, got non-bare — scenario assumptions off"
  fi
  ( cd "$sb" && "$MAW" ws create dev --from main >/dev/null 2>&1
    printf 'fn helper(){}\n' > ws/dev/helper.rs
    "$MAW" exec dev -- git add -A
    "$MAW" exec dev -- git commit -qm "feat: helper" )

  # make default dirty -> migrate must refuse
  printf 'dirty\n' >> "$sb/ws/default/shared.txt"
  local out rc
  out="$( cd "$sb" && "$MAW" migrate 2>&1 )"; rc=$?
  if [[ $rc -ne 0 ]] && grep -qi 'dirty\|allow-dirty\|uncommitted' <<<"$out"; then
    ok "migrate: refused on dirty default (rc=$rc, actionable message)"
  else
    bad "migrate: did NOT refuse on dirty default (rc=$rc)"
  fi
  assert_git_intact "$sb" "migrate/post-refuse"

  # now allow-dirty (the dropped uncommitted change is captured to a recovery ref)
  ( cd "$sb" && "$MAW" migrate --allow-dirty >/dev/null 2>&1 )
  assert_not_bare "$sb" "migrate/post"
  assert_consolidated_shape "$sb" "migrate/post"
  assert_git_intact "$sb" "migrate/post"
  assert_file_contains "$sb/main.rs" "fn main" "migrate: source materialized at root"
  # dev was *relocated*, not destroyed — its committed work must survive in place.
  assert_file_contains "$sb/.maw/workspaces/dev/helper.rs" "fn helper" \
    "migrate: dev's committed work survived relocation (Prime Invariant)"
  # The dropped uncommitted default change must be captured to a recovery ref.
  assert_recovery_ref "$sb" default \
    "migrate: dirty default captured to recovery ref (Prime Invariant)"
  # bn-sdv4 (split from bn-2ksp): migrate's own output says "list via maw ws
  # recover" — so `maw ws recover` MUST surface migration recovery refs, not
  # just destroy records. This is the Prime-Invariant discoverability
  # guarantee: if the advertised path can't find preserved work, the tool
  # lies. Now a hard assertion.
  if ( cd "$sb" && "$MAW" ws recover 2>&1 | grep -qi 'default' ); then
    ok "migrate: maw ws recover surfaces the migration recovery ref"
  else
    bad "migrate: maw ws recover does NOT surface migration recovery ref (bn-sdv4) — recoverable only via git for-each-ref refs/manifold/recovery/ + maw ws recover --ref"
  fi
}

# =============================================================================
# Scenario 5: bn-3bkn regression guard — merge --destroy AFTER migrate
# =============================================================================
scn_post_migrate_merge() {
  hdr "post-migrate-merge: bn-3bkn guard — ws merge --destroy on a migrated repo"
  local sb; sb="$(mksandbox postmig)"
  (
    cd "$sb"
    git init -b main -q
    git_id
    printf 'fn main(){}\n' > main.rs
    git add -A && git commit -qm "seed"
    "$MAW" init >/dev/null 2>&1            # v2 bare
    "$MAW" migrate >/dev/null 2>&1         # -> consolidated
    "$MAW" epoch sync >/dev/null 2>&1
    "$MAW" ws create eve --from main >/dev/null 2>&1
    printf 'pub fn eve(){}\n' > .maw/workspaces/eve/eve.rs
    "$MAW" exec eve -- git add -A
    "$MAW" exec eve -- git commit -qm "feat: eve"
    # THE scary operation that gutted .git pre-pre.2:
    "$MAW" ws merge eve --into default --destroy --message "feat: eve" >/dev/null 2>&1
  )
  assert_git_intact "$sb" "post-migrate-merge/AFTER DESTROY (bn-3bkn guard)"
  assert_consolidated_shape "$sb" "post-migrate-merge"
  assert_file_contains "$sb/eve.rs" "fn eve" "post-migrate-merge: eve's work landed at root"
  assert_recovery_ref "$sb" eve "post-migrate-merge: eve recovery ref pinned"
}

# ---- driver -----------------------------------------------------------------
ALL=(greenfield lifecycle recover migrate post-migrate-merge)
declare -A FN=(
  [greenfield]=scn_greenfield
  [lifecycle]=scn_lifecycle
  [recover]=scn_recover
  [migrate]=scn_migrate
  [post-migrate-merge]=scn_post_migrate_merge
)

REQUESTED=()
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) REQUESTED+=("$arg") ;;
  esac
done
[[ ${#REQUESTED[@]} -eq 0 ]] && REQUESTED=("${ALL[@]}")

printf '%s\n' "$(c_dim "maw binary: $("$MAW" --version 2>/dev/null | head -1)")"
for name in "${REQUESTED[@]}"; do
  fn="${FN[$name]:-}"
  if [[ -z "$fn" ]]; then
    printf '%s unknown scenario: %s\n' "$(c_red ERROR)" "$name" >&2; exit 2
  fi
  "$fn"
done

printf '\n\033[1m== summary ==\033[0m\n'
printf '  passed: %s   failed: %s\n' "$(c_grn "$PASS")" "$( [[ $FAIL -gt 0 ]] && c_red "$FAIL" || c_grn 0 )"
if [[ $FAIL -gt 0 ]]; then
  printf '\n%s\n' "$(c_red "INVARIANT VIOLATIONS:")"
  for f in "${FAILED_CHECKS[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
printf '%s\n' "$(c_grn "all invariants held")"
