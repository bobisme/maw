#!/usr/bin/env bash
# bn-3bkn reproduction: does a maw ws op gut the git dir on consolidated layout?
set +e
SB=/tmp/maw-repro-sb
rm -rf "$SB"; mkdir -p "$SB"; cd "$SB" || exit 1

snap() {
  echo "----- SNAPSHOT: $1 -----"
  echo ".git: $( [ -f .git ] && echo "file -> $(cat .git)" || ([ -d .git ] && echo dir || echo absent) )"
  for gd in repo.git .git; do
    if [ -d "$gd" ]; then
      echo "  $gd/ entries: $(ls -1 "$gd" 2>/dev/null | tr '\n' ' ')"
      echo "    HEAD=$( [ -f "$gd/HEAD" ] && echo yes || echo NO )  config=$( [ -f "$gd/config" ] && echo yes || echo NO )  packed-refs=$( [ -f "$gd/packed-refs" ] && echo yes || echo NO )  refs/heads=$( [ -d "$gd/refs/heads" ] && echo yes || echo NO )  worktrees=$( [ -d "$gd/worktrees" ] && echo yes || echo NO )"
    fi
  done
  echo "  git rev-parse HEAD: $(git rev-parse --short HEAD 2>&1 | head -1)"
}

echo "===== STEP 0: v2 maw repo ====="
git init -q .
git config user.email t@t.t; git config user.name t
mkdir -p crates; echo 'fn main(){}' > crates/main.rs; echo 'hi' > README.md
git add -A; git commit -qm "init"
maw init --legacy-ws >/dev/null 2>&1
maw ws create seed --from main >/dev/null 2>&1   # establish epoch if needed
maw ws destroy seed >/dev/null 2>&1
snap "after v2 init"

echo "===== STEP 1: migrate to consolidated ====="
maw migrate >/tmp/maw-repro-migrate.log 2>&1; echo "migrate exit: $?"
tail -3 /tmp/maw-repro-migrate.log
snap "after migrate"

echo "===== STEP 2: ws create ====="
maw ws create wsa --from main >/tmp/maw-repro-create.log 2>&1; echo "create exit: $?"
tail -2 /tmp/maw-repro-create.log
snap "after ws create"

echo "===== STEP 3: edit + commit in workspace ====="
WSP=$(ls -d "$SB"/.maw/workspaces/wsa 2>/dev/null)
echo "wsp=$WSP"
[ -n "$WSP" ] && echo 'change' >> "$WSP/README.md"
maw exec wsa -- git add -A >/dev/null 2>&1
maw exec wsa -- git commit -qm "edit" >/dev/null 2>&1
snap "after ws commit"

echo "===== STEP 4: ws merge --destroy (PRIME SUSPECT) ====="
strace -f -y -e trace=unlink,unlinkat,rmdir,rename,renameat,renameat2 maw ws merge wsa --into default --destroy --message "feat: edit" >/tmp/maw-repro-merge.log 2>/tmp/maw-repro-strace.log; echo "merge exit: $?"
echo "--- syscalls touching repo.git (the deletions) ---"
grep -E 'repo\.git' /tmp/maw-repro-strace.log | grep -E 'unlink|rmdir|rename' | grep -Ev 'objects|/lfs/' | head -40
cat /tmp/maw-repro-merge.log
snap "after ws merge --destroy"

echo "===== DONE ====="
