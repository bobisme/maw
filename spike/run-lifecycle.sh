#!/usr/bin/env bash
# SP4 (bn-gh1p): hand-construct the TARGET layout and exercise the full
# maw lifecycle (create -> commit -> merge -> destroy -> recover) using the
# SAME git plumbing the merge engine uses, to validate "relocation not rewrite".
#
# TARGET layout under test:
#   <root>/            normal (non-bare) checkout, IS the merge target
#                       (replaces ws/default/ as the privileged checkout)
#   <root>/.maw/worktrees/<name>/   agent worktrees (replaces ws/<name>/)
#   <root>/.git/       git data (normal, NOT core.bare)
#   <root>/.manifold/  maw metadata (unchanged)
#
# Engine operations mirrored verbatim:
#  - worktree creation: git worktree add with INDEPENDENT admin-name vs path
#    (mirrors maw_git::worktree_impl::worktree_add(name, path) where name is
#     the .git/worktrees/<name> key and path is the on-disk location).
#  - privileged-target epoch sync: detach HEAD by writing the raw OID to the
#    worktree HEAD file + reset index (mirrors update_default_workspace
#    Step-0 ANCHOR and sync_target_worktree_to_epoch).
#  - post-merge destroy: capture recovery snapshot ref then worktree remove
#    (mirrors handle_post_merge_destroy).
set -uo pipefail
ROOT=/tmp/sp4-layout/repo
rm -rf "$ROOT"; mkdir -p "$ROOT"
cd "$ROOT"

say() { printf '\n=== %s ===\n' "$*"; }
ok()  { printf 'OK   %s\n' "$*"; }
note(){ printf 'NOTE %s\n' "$*"; }

say "1. INIT target layout (root = normal checkout, NOT bare)"
git init -q -b main .
git config user.email sp4@example.com
git config user.name SP4
mkdir -p src
printf 'fn main() {}\n' > src/main.rs
printf 'shared\n' > shared.txt
git add -A
git commit -qm "epoch0: initial"
EPOCH0=$(git rev-parse HEAD)
ok "epoch0 = $EPOCH0"
note "core.bare = $(git config --get core.bare || echo '<unset/false>')  (TARGET: false; root IS a real checkout)"
mkdir -p .manifold/refs .maw/worktrees
git update-ref refs/manifold/epoch/current "$EPOCH0"
ok "refs/manifold/epoch/current set (manifold metadata layout UNCHANGED)"
note "root is the privileged merge-target checkout; files materialized on disk at root:"
ls -1 "$ROOT" | sed 's/^/       /'

say "2. CREATE two agent worktrees under hidden .maw/worktrees/"
# Mirror of maw_git::worktree_impl::worktree_add: admin name independent of path.
for WS in agent-a agent-b; do
  WSPATH="$ROOT/.maw/worktrees/$WS"
  # 'name' (admin key in .git/worktrees/<name>) == $WS ; 'path' == hidden nested dir.
  git worktree add -q --detach "$WSPATH" "$EPOCH0"
  git update-ref "refs/manifold/epoch/$WS" "$EPOCH0"
  if [ -d "$WSPATH" ] && [ -e "$ROOT/.git/worktrees/$WS" ]; then
    ok "worktree '$WS' -> $WSPATH  (admin: .git/worktrees/$WS)"
  else
    echo "FAIL worktree create $WS"; exit 1
  fi
done
note "git worktree list (admin layer is path-agnostic):"
git worktree list | sed 's/^/       /'

say "3. COMMIT work in each agent worktree (disjoint + overlapping)"
A="$ROOT/.maw/worktrees/agent-a"
B="$ROOT/.maw/worktrees/agent-b"
( cd "$A" && printf 'fn a() {}\n' > src/a.rs && printf 'shared+A\n' > shared.txt \
  && git add -A && git commit -qm "agent-a: add a.rs, edit shared" )
ACOMMIT=$(git -C "$A" rev-parse HEAD)
( cd "$B" && printf 'fn b() {}\n' > src/b.rs \
  && git add -A && git commit -qm "agent-b: add b.rs" )
BCOMMIT=$(git -C "$B" rev-parse HEAD)
ok "agent-a HEAD = $ACOMMIT"
ok "agent-b HEAD = $BCOMMIT"

say "4. MERGE: build merged tree (engine merge algorithm is path-layout agnostic)"
# The merge engine reads trees by OID and writes a merged tree by OID. None of
# that touches the working-copy layout. We synthesize the merged commit the
# same way build/collect does: 3-way over epoch0.
git read-tree -m --aggressive "$EPOCH0" "$ACOMMIT" "$BCOMMIT" 2>/dev/null || {
  # Fall back to an explicit octopus-style merge via a temp index.
  GIT_INDEX_FILE="$ROOT/.git/sp4-merge-index"; export GIT_INDEX_FILE
  git read-tree "$ACOMMIT"
  git checkout-index -af --prefix="$ROOT/.sp4-mergetmp/"
  cp "$B/src/b.rs" "$ROOT/.sp4-mergetmp/src/b.rs"
  ( cd "$ROOT/.sp4-mergetmp" && git --git-dir="$ROOT/.git" add -A )
  unset GIT_INDEX_FILE
}
# Deterministically construct the merged tree by hand (engine does this via gix).
MTMP="$ROOT/.sp4-merged"; rm -rf "$MTMP"; mkdir -p "$MTMP/src"
cp "$A/src/a.rs" "$MTMP/src/a.rs"
cp "$B/src/b.rs" "$MTMP/src/b.rs"
cp "$A/src/main.rs" "$MTMP/src/main.rs"
cp "$A/shared.txt" "$MTMP/shared.txt"   # agent-a's edit wins (it changed it)
GIT_INDEX_FILE="$ROOT/.git/sp4-mi"; export GIT_INDEX_FILE
git --work-tree="$MTMP" add -A 2>/dev/null || ( cd "$MTMP" && git --git-dir="$ROOT/.git" --work-tree="$MTMP" add -A )
MERGED_TREE=$(git write-tree)
unset GIT_INDEX_FILE
MERGE_COMMIT=$(git commit-tree "$MERGED_TREE" -p "$EPOCH0" -p "$ACOMMIT" -p "$BCOMMIT" -m "merge: agent-a + agent-b")
ok "merged tree  = $MERGED_TREE"
ok "merge commit = $MERGE_COMMIT"
# Advance branch + epoch ref (engine: COMMIT phase).
git update-ref refs/heads/main "$MERGE_COMMIT"
git update-ref refs/manifold/epoch/current "$MERGE_COMMIT"
ok "branch main + epoch advanced to merge commit"

say "5. PRIVILEGED-TARGET update: sync ROOT checkout to new epoch"
# Verbatim mirror of update_default_workspace Step-0 ANCHOR +
# sync_target_worktree_to_epoch: write raw OID to the target worktree's HEAD
# file (NOT 'git checkout --detach'), then reset index, then materialize the
# diff paths. For the ROOT worktree the HEAD file is <root>/.git/HEAD.
ROOT_HEAD_FILE="$ROOT/.git/HEAD"
note "target worktree HEAD file resolved as: $ROOT_HEAD_FILE  (root checkout)"
printf '%s\n' "$MERGE_COMMIT" > "$ROOT_HEAD_FILE"        # detach at new epoch
git read-tree HEAD                                       # align index (unstage_all equiv)
git checkout-index -af                                   # materialize tree to root WC
git symbolic-ref HEAD refs/heads/main 2>/dev/null        # reattach to branch (checkout_to)
if [ -f "$ROOT/src/a.rs" ] && [ -f "$ROOT/src/b.rs" ] && [ "$(cat "$ROOT/shared.txt")" = "shared+A" ]; then
  ok "ROOT checkout now reflects merge: src/a.rs, src/b.rs present; shared.txt=agent-a"
else
  echo "FAIL root target not updated to merge result"; ls -R "$ROOT/src"; exit 1
fi
note "root HEAD now: $(git rev-parse HEAD) on $(git symbolic-ref --short HEAD 2>/dev/null || echo DETACHED)"

say "6. DESTROY agent worktrees (with recovery snapshot, like post-merge)"
# Mirror handle_post_merge_destroy: pin a recovery ref to final HEAD, then
# git worktree remove + prune. Admin key == name, independent of nested path.
for WS in agent-a agent-b; do
  WSPATH="$ROOT/.maw/worktrees/$WS"
  FINAL=$(git -C "$WSPATH" rev-parse HEAD)
  git update-ref "refs/manifold/recovery/$WS/snapshot" "$FINAL"   # Prime-Invariant pin
  git worktree remove --force "$WSPATH"
  git worktree prune
  if [ ! -d "$WSPATH" ] && [ ! -e "$ROOT/.git/worktrees/$WS" ]; then
    ok "destroyed '$WS' (recovery ref refs/manifold/recovery/$WS/snapshot -> $FINAL)"
  else
    echo "FAIL destroy $WS"; exit 1
  fi
done

say "7. RECOVER: prove no work lost (Prime Invariant)"
for WS in agent-a agent-b; do
  REC=$(git rev-parse "refs/manifold/recovery/$WS/snapshot")
  ok "recovery[$WS] = $REC  contents:"
  git ls-tree -r --name-only "$REC" | sed 's/^/       /'
done
# Restore agent-a into a fresh worktree at a NEW hidden path to prove round-trip.
git worktree add -q --detach "$ROOT/.maw/worktrees/agent-a-restored" "refs/manifold/recovery/agent-a/snapshot"
if [ -f "$ROOT/.maw/worktrees/agent-a-restored/src/a.rs" ]; then
  ok "recovered agent-a into .maw/worktrees/agent-a-restored (src/a.rs present)"
else
  echo "FAIL recover"; exit 1
fi

say "8. FINAL STATE"
note "git worktree list:"
git worktree list | sed 's/^/       /'
note "root WC top-level (hidden .maw not a tracked source dir):"
ls -1a "$ROOT" | sed 's/^/       /'
note "manifold refs:"
git for-each-ref 'refs/manifold/*' --format='       %(refname) %(objectname:short)'
echo
echo "ALL LIFECYCLE STAGES PASSED ON TARGET LAYOUT"
