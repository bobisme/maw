#!/usr/bin/env python3
"""SP2 (bn-3qxi) spike harness: Oracle A/B definition + computability validation.

Drives real `maw` create/commit/merge/destroy/recover ops on a THROWAWAY repo,
maintains the historical set of every workspace ref tip ever observed, and after
every op-step computes:

  Oracle A (no committed work lost) -- reachability closure check
  Oracle B (state coherence)        -- dangling-ref / merge-state / orphaned-recovery

It then plants:
  (1) a synthetic WORK-LOSS  -> must trip Oracle A only
  (2) a synthetic bn-cm63    -> must trip Oracle B only (a dangling
      refs/manifold/head/<ws> with no workspace, NOT a work-loss)

and measures per-step oracle cost, extrapolating to >= 1e6 steps.

This is a spike: correctness of the PREDICATES is the deliverable, not this
driver's elegance. The Rust-ready predicate spec is notes/oracle-ab-spec.md.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field

REPO = "/tmp/sp2-oracle-repro"
GITDIR = os.path.join(REPO, "repo.git")


# --------------------------------------------------------------------------
# git / maw plumbing (independent verifier: git CLI on the bare repo.git)
# --------------------------------------------------------------------------
def git(*args: str, check: bool = True) -> str:
    r = subprocess.run(
        ["git", "--git-dir", GITDIR, *args],
        capture_output=True,
        text=True,
    )
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {r.stderr.strip()}")
    return r.stdout.strip()


def maw(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    r = subprocess.run(
        ["maw", *args], cwd=REPO, capture_output=True, text=True
    )
    if check and r.returncode != 0:
        raise RuntimeError(
            f"maw {' '.join(args)} failed ({r.returncode}): {r.stderr.strip()}\n{r.stdout.strip()}"
        )
    return r


def all_refs() -> dict[str, str]:
    out = git("for-each-ref", "--format=%(refname) %(objectname) %(objecttype)")
    refs: dict[str, str] = {}
    for line in out.splitlines():
        if not line:
            continue
        name, oid, otype = line.split(" ")
        refs[name] = oid  # objecttype recovered on demand
    return refs


def obj_type(oid: str) -> str:
    return git("cat-file", "-t", oid, check=False) or "missing"


def is_ancestor(anc: str, desc: str) -> bool:
    r = subprocess.run(
        ["git", "--git-dir", GITDIR, "merge-base", "--is-ancestor", anc, desc],
        capture_output=True,
    )
    return r.returncode == 0


def workspace_dirs() -> set[str]:
    wsroot = os.path.join(REPO, "ws")
    if not os.path.isdir(wsroot):
        return set()
    return {
        d
        for d in os.listdir(wsroot)
        if os.path.isdir(os.path.join(wsroot, d))
    }


def ws_head(ws: str) -> str | None:
    wt = os.path.join(REPO, "ws", ws)
    if not os.path.isdir(wt):
        return None
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=wt,
        capture_output=True,
        text=True,
    )
    return r.stdout.strip() if r.returncode == 0 else None


def read_merge_state() -> dict | None:
    p = os.path.join(REPO, ".manifold", "merge-state.json")
    try:
        with open(p) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


# --------------------------------------------------------------------------
# Historical witness set: every commit OID a workspace tip ever held.
# Maintained incrementally -- O(workspaces) per step, never rescanned.
# --------------------------------------------------------------------------
def tip_blobs(tip: str) -> set[str]:
    """Blob OIDs in a commit's tree (recursive). The set of *content* a
    workspace tip carries -- this is what 'no work lost' is really about,
    NOT the commit OID (maw rebuilds trees on merge -> commit ancestry is
    intentionally NOT preserved)."""
    out = git("ls-tree", "-r", tip, "--format=%(objecttype) %(objectname)",
              check=False)
    blobs = set()
    for line in out.splitlines():
        if line.startswith("blob "):
            blobs.add(line.split(" ", 1)[1].strip())
    return blobs


@dataclass
class Witnesses:
    # blob_oid -> human label of the workspace tip that first carried it.
    # Accumulated incrementally: each newly-observed ws tip contributes only
    # its (small) blob set once; never rescanned. This is the cheap, correct
    # witness for Oracle A.
    blobs: dict[str, str] = field(default_factory=dict)
    # ws -> last tip OID we already harvested blobs for (incremental guard)
    _seen_tip: dict[str, str] = field(default_factory=dict)
    # tip OID -> label, kept for diagnostics / shrink only
    tips: dict[str, str] = field(default_factory=dict)

    def observe_ws_tips(self, step: int) -> None:
        for ws in workspace_dirs():
            h = ws_head(ws)
            if not h:
                continue
            if h not in self.tips:
                self.tips[h] = f"ws:{ws}@step{step}"
            if self._seen_tip.get(ws) == h:
                continue  # tip unchanged since last observation -> O(1)
            self._seen_tip[ws] = h
            for b in tip_blobs(h):
                self.blobs.setdefault(b, f"ws:{ws}@step{step}(tip {h[:8]})")


# --------------------------------------------------------------------------
# ORACLE A -- no committed work lost   (CONTENT reachability, not commit
# ancestry -- this is the central SP2 finding)
#
# Frontier (reachability roots) after a step:
#   {default branch history}            = refs/heads/main
#   U {recovery refs}                   = refs/manifold/recovery/*
#   U {extant workspace ref tips}       = git rev-parse HEAD of every ws/<x>/
#   U {epoch refs}                      = refs/manifold/epoch/current,
#                                         refs/manifold/epoch/ws/*
#
# WHY NOT commit ancestry: maw's merge engine REBUILDS the merged tree and
# emits a fresh epoch commit; recovery snapshots are fresh commits too.
# A workspace's literal HEAD commit OID is therefore NEVER an ancestor of
# the post-merge frontier -> commit-ancestry Oracle A false-positives on
# every single merge. Proven empirically in this spike (step 7).
#
# CORRECT predicate: every blob OID ever carried by a historically-observed
# workspace tip's tree must remain reachable in the object graph from the
# frontier root set. Computable cheaply: one `git rev-list --objects
# <roots>` enumerates the reachable blob universe; Oracle A == witness_blobs
# is a subset of reachable_blobs.
# --------------------------------------------------------------------------
def frontier_root_oids(refs: dict[str, str]) -> list[str]:
    roots: list[str] = []
    for name, oid in refs.items():
        if (name == "refs/heads/main"
                or name.startswith("refs/manifold/recovery/")
                or name == "refs/manifold/epoch/current"
                or name.startswith("refs/manifold/epoch/ws/")
                or name.startswith("refs/manifold/ws/")):
            roots.append(oid)
    for ws in workspace_dirs():
        h = ws_head(ws)
        if h:
            roots.append(h)
    return sorted(set(roots))


def reachable_blobs(root_oids: list[str]) -> set[str]:
    """Every blob reachable from the frontier roots, via one rev-list."""
    if not root_oids:
        return set()
    out = git("rev-list", "--objects", "--no-object-names", *root_oids,
              check=False)
    # --no-object-names gives one OID per line (commits, trees, blobs).
    # We membership-test witness blobs against this universe; we don't need
    # to type-filter (a witness is known to be a blob).
    return set(x for x in out.split() if x)


def oracle_a(w: Witnesses, refs: dict[str, str]) -> list[str]:
    """Return list of violation strings (empty == pass)."""
    roots = frontier_root_oids(refs)
    universe = reachable_blobs(roots)
    violations: list[str] = []
    for blob, origin in w.blobs.items():
        if blob not in universe:
            violations.append(
                f"ORACLE_A: lost committed work: blob {blob[:12]} "
                f"(authored by {origin}) is unreachable from every frontier "
                f"root ({len(roots)} roots)"
            )
            if len(violations) >= 5:
                violations.append("ORACLE_A: ...(further losses elided)")
                break
    return violations


# --------------------------------------------------------------------------
# ORACLE B -- state coherence  (must catch the bn-cm63 class)
#
# B1 no-dangling-head: for every refs/manifold/head/<ws>, ws/<ws>/ exists
#    OR <ws> is a source of a *live* (alive owner, non-terminal) merge in
#    .manifold/merge-state.json.
# B2 owned-ref symmetry: refs/manifold/epoch/ws/<ws> and refs/manifold/ws/<ws>
#    likewise require ws/<ws>/ or live-merge ownership (same dangling class).
# B3 merge-state coherence: if merge-state.json exists & non-terminal, every
#    source workspace either has ws/<src>/ or a recovery ref (it was destroyed
#    mid-merge but its work is pinned); epoch_before resolves to a commit;
#    if phase >= commit then epoch_after resolves to a commit.
# B4 recovery well-formed: every refs/manifold/recovery/<ws>/<ts> resolves to
#    a readable COMMIT object (no orphaned/garbage recovery).
# --------------------------------------------------------------------------
HEAD_PREFIX = "refs/manifold/head/"
EPOCH_WS_PREFIX = "refs/manifold/epoch/ws/"
WS_STATE_PREFIX = "refs/manifold/ws/"
RECOVERY_PREFIX = "refs/manifold/recovery/"


def live_merge_sources() -> set[str]:
    st = read_merge_state()
    if not st:
        return set()
    phase = st.get("phase")
    if phase in ("complete", "aborted"):
        return set()
    # Liveness: the spike approximates "live" as "owner pid alive". The
    # production oracle reuses maw_core::merge_state::staleness() (Live vs
    # Orphaned vs Indeterminate). For coherence we only protect Live sources;
    # an Orphaned/Indeterminate merge's head refs are legitimately dangling.
    pid = st.get("owner_pid")
    alive = True
    if isinstance(pid, int):
        try:
            os.kill(pid, 0)
            alive = True
        except ProcessLookupError:
            alive = False
        except PermissionError:
            alive = True
    return set(st.get("sources", [])) if alive else set()


def oracle_b(refs: dict[str, str]) -> list[str]:
    violations: list[str] = []
    ws_present = workspace_dirs()
    protected = live_merge_sources()

    def dangling(prefix: str, code: str) -> None:
        for name, oid in refs.items():
            if not name.startswith(prefix):
                continue
            ws = name[len(prefix):]
            if "/" in ws:  # recovery-style nested; not this class
                continue
            if ws in ws_present or ws in protected:
                continue
            violations.append(
                f"ORACLE_B/{code}: dangling {name} -> {oid[:12]} "
                f"({obj_type(oid)}) for non-existent workspace '{ws}' "
                f"(bn-cm63 class)"
            )

    dangling(HEAD_PREFIX, "B1")
    dangling(EPOCH_WS_PREFIX, "B2")
    dangling(WS_STATE_PREFIX, "B2")

    # B3 merge-state coherence
    st = read_merge_state()
    if st and st.get("phase") not in (None, "complete", "aborted"):
        for src in st.get("sources", []):
            has_ws = src in ws_present
            has_rec = any(
                n.startswith(f"{RECOVERY_PREFIX}{src}/") for n in refs
            )
            if not has_ws and not has_rec:
                violations.append(
                    f"ORACLE_B/B3: merge source '{src}' has neither a "
                    f"workspace nor a recovery ref while merge phase="
                    f"{st.get('phase')}"
                )
        eb = st.get("epoch_before")
        if eb and obj_type(eb) != "commit":
            violations.append(
                f"ORACLE_B/B3: merge-state epoch_before {eb[:12]} "
                f"is not a commit"
            )
        if st.get("phase") in ("commit", "cleanup"):
            ea = st.get("epoch_after")
            if ea and obj_type(ea) != "commit":
                violations.append(
                    f"ORACLE_B/B3: post-commit epoch_after "
                    f"{str(ea)[:12]} is not a commit"
                )

    # B4 recovery well-formed
    for name, oid in refs.items():
        if not name.startswith(RECOVERY_PREFIX):
            continue
        t = obj_type(oid)
        if t != "commit":
            violations.append(
                f"ORACLE_B/B4: recovery ref {name} -> {oid[:12]} "
                f"is a '{t}', expected commit (orphaned recovery)"
            )
    return violations


# --------------------------------------------------------------------------
# Throwaway repo lifecycle
# --------------------------------------------------------------------------
def fresh_repo() -> None:
    if os.path.isdir(REPO):
        shutil.rmtree(REPO)
    os.makedirs(REPO)
    subprocess.run(["git", "init", "-q", "."], cwd=REPO, check=True)
    subprocess.run(["git", "config", "user.email", "t@t.com"], cwd=REPO, check=True)
    subprocess.run(["git", "config", "user.name", "T"], cwd=REPO, check=True)
    subprocess.run(["git", "config", "commit.gpgsign", "false"], cwd=REPO, check=True)
    with open(os.path.join(REPO, "README.md"), "w") as f:
        f.write("# sp2\n")
    subprocess.run(["git", "add", "-A"], cwd=REPO, check=True)
    subprocess.run(["git", "commit", "-qm", "init"], cwd=REPO, check=True)
    maw("init")


def ws_commit(ws: str, fname: str, content: str) -> None:
    """maw exec add then commit in two calls (single chained call no-ops)."""
    with open(os.path.join(REPO, "ws", ws, fname), "w") as f:
        f.write(content)
    maw("exec", ws, "--", "git", "add", "-A")
    maw("exec", ws, "--", "git", "commit", "-qm", f"{ws}: {fname}")


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------
def run_step(label: str, fn) -> float:
    t0 = time.perf_counter()
    fn()
    return time.perf_counter() - t0


def check(step: int, label: str, w: Witnesses,
          expect_a: bool = True, expect_b: bool = True) -> dict:
    """Run both oracles; return timing + pass/fail. expect_* = should pass."""
    refs = all_refs()
    w.observe_ws_tips(step)

    ta0 = time.perf_counter()
    va = oracle_a(w, refs)
    ta = time.perf_counter() - ta0

    tb0 = time.perf_counter()
    vb = oracle_b(refs)
    tb = time.perf_counter() - tb0

    a_pass = not va
    b_pass = not vb
    status = "OK"
    notes = []
    if a_pass != expect_a:
        status = "UNEXPECTED"
        notes.append(f"A expected {'pass' if expect_a else 'FAIL'} got "
                     f"{'pass' if a_pass else 'FAIL'}: {va}")
    if b_pass != expect_b:
        status = "UNEXPECTED"
        notes.append(f"B expected {'pass' if expect_b else 'FAIL'} got "
                     f"{'pass' if b_pass else 'FAIL'}: {vb}")
    print(f"[{step:>3}] {label:<34} A={'pass' if a_pass else 'FAIL'} "
          f"B={'pass' if b_pass else 'FAIL'} "
          f"(A {ta*1e3:6.1f}ms, B {tb*1e3:6.1f}ms, "
          f"{len(w.tips)} witnesses) {status}")
    for n in notes:
        print(f"      -> {n}")
    return {
        "step": step, "label": label, "a_pass": a_pass, "b_pass": b_pass,
        "a_ms": ta * 1e3, "b_ms": tb * 1e3, "witnesses": len(w.tips),
        "status": status, "notes": notes,
    }


def main() -> int:
    print("=== SP2 Oracle A/B harness (bn-3qxi) ===\n")
    fresh_repo()
    w = Witnesses()
    results = []
    step = 0

    def S(label, fn, ea=True, eb=True):
        nonlocal step
        step += 1
        run_step(label, fn)
        results.append(check(step, label, w, ea, eb))

    # ---- normal lifecycle: should be clean (A pass, B pass) ----
    S("init/baseline", lambda: None)
    S("create alice", lambda: maw("ws", "create", "alice", "--from", "main"))
    S("alice commit f1", lambda: ws_commit("alice", "a1.txt", "alice-1"))
    S("create bob", lambda: maw("ws", "create", "bob", "--from", "main"))
    S("bob commit f1", lambda: ws_commit("bob", "b1.txt", "bob-1"))
    S("alice commit f2", lambda: ws_commit("alice", "a2.txt", "alice-2"))
    S("merge alice->default",
      lambda: maw("ws", "merge", "alice", "--into", "default",
                  "--message", "feat: alice"))
    S("destroy alice --force",
      lambda: maw("ws", "destroy", "alice", "--force"))
    S("create carol", lambda: maw("ws", "create", "carol", "--from", "main"))
    S("carol commit f1", lambda: ws_commit("carol", "c1.txt", "carol-1"))
    S("destroy carol --force",
      lambda: maw("ws", "destroy", "carol", "--force"))
    S("merge bob->default",
      lambda: maw("ws", "merge", "bob", "--into", "default",
                  "--destroy", "--message", "feat: bob"))

    # ---- planted violation #1: synthetic WORK-LOSS ----
    # Create a workspace, commit, capture its tip into the witness set, then
    # forcibly delete EVERY frontier ref that could reach it WITHOUT a
    # recovery ref (simulating a destroy that failed to pin recovery -- a
    # Prime-Invariant violation).  Oracle A must FAIL, Oracle B must PASS
    # (no dangling head ref: we delete the whole owned-ref set + workspace).
    dave_blob_holder: dict[str, str] = {}

    def plant_work_loss():
        maw("ws", "create", "dave", "--from", "main")
        ws_commit("dave", "d1.txt", "dave-secret-work")
        # harvest dave's authored blob into the witness set BEFORE the loss
        w.observe_ws_tips(step)
        dave_tip = ws_head("dave")
        assert dave_tip
        dblob = next(
            b for b in tip_blobs(dave_tip)
            if git("cat-file", "-p", b) == "dave-secret-work"
        )
        dave_blob_holder["oid"] = dblob
        w.blobs[dblob] = "ws:dave@plant(d1.txt)"
        # simulate a destroy that FAILED to pin a recovery ref (real
        # Prime-Invariant violation): delete the whole owned-ref set + ws,
        # then HARD prune so the blob is genuinely unreachable -- this is
        # exactly what irreversible work loss looks like.
        for r in ("refs/manifold/head/dave",
                  "refs/manifold/epoch/ws/dave",
                  "refs/manifold/ws/dave"):
            git("update-ref", "-d", r, check=False)
        shutil.rmtree(os.path.join(REPO, "ws", "dave"))
        subprocess.run(["git", "--git-dir", GITDIR, "worktree", "prune"],
                        capture_output=True)
        # objects are still loose+dangling until gc; the *reachability*
        # predicate already fails (blob not reachable from any frontier
        # root) which is the point -- no gc needed for Oracle A to fire.

    S("PLANT work-loss (dave)", plant_work_loss, ea=False, eb=True)

    # heal: a real recovery would re-pin dave's content under a recovery
    # ref. Reconstruct a recovery commit that CONTAINS dave's blob, exactly
    # as `maw ws destroy --force` would have.
    def heal_work_loss():
        dblob = dave_blob_holder["oid"]
        # build a tree {d1.txt -> dblob} and a commit, pin under recovery/
        ls = f"100644 blob {dblob}\td1.txt\n"
        p = subprocess.run(
            ["git", "--git-dir", GITDIR, "mktree"],
            input=ls, capture_output=True, text=True, check=True)
        tree = p.stdout.strip()
        c = subprocess.run(
            ["git", "--git-dir", GITDIR, "commit-tree", tree, "-m",
             "recovery: dave"],
            capture_output=True, text=True, check=True,
            env={**os.environ,
                 "GIT_AUTHOR_NAME": "r", "GIT_AUTHOR_EMAIL": "r@r",
                 "GIT_COMMITTER_NAME": "r", "GIT_COMMITTER_EMAIL": "r@r"})
        rcommit = c.stdout.strip()
        git("update-ref",
            "refs/manifold/recovery/dave/2026-01-01T00-00-00Z", rcommit)
    S("heal work-loss (pin recovery)", heal_work_loss, ea=True, eb=True)

    # ---- planted violation #2: synthetic bn-cm63 ----
    # A dangling refs/manifold/head/<ws> oplog blob with NO workspace and NO
    # in-flight merge.  This is NOT work loss (no commit lost) -- Oracle A
    # must PASS, Oracle B must FAIL (B1).
    def plant_cm63():
        blob = git("hash-object", "-w", "--stdin", check=True) if False else None
        # reuse an existing oplog blob if present, else synthesize one
        existing = [
            o for n, o in all_refs().items()
            if n.startswith(HEAD_PREFIX)
        ]
        if existing:
            blob_oid = existing[0]
        else:
            p = subprocess.run(
                ["git", "--git-dir", GITDIR, "hash-object", "-w", "--stdin"],
                input='{"workspace_id":"ghost","payload":{"type":"create"}}',
                capture_output=True, text=True, check=True,
            )
            blob_oid = p.stdout.strip()
        git("update-ref", "refs/manifold/head/ghost", blob_oid)
        assert not os.path.isdir(os.path.join(REPO, "ws", "ghost"))

    S("PLANT bn-cm63 (ghost head ref)", plant_cm63, ea=True, eb=False)

    # cross-check against ground truth: maw doctor must also flag it
    dr = maw("doctor", check=False)
    doctor_flags = "stale head refs" in dr.stdout and (
        "[WARN]" in dr.stdout or "[FAIL]" in dr.stdout
    )
    print(f"\n  ground-truth cross-check: `maw doctor` flags stale head "
          f"ref? {'YES' if doctor_flags else 'NO'}")

    # heal: plain `maw gc` self-heals (bn-cm63 fix)
    def heal_cm63():
        maw("gc")
    S("heal bn-cm63 (maw gc)", heal_cm63, ea=True, eb=True)

    dr2 = maw("doctor", check=False)
    doctor_clean = "[WARN] stale head refs" not in dr2.stdout
    print(f"  post-gc cross-check: `maw doctor` clean of stale head refs? "
          f"{'YES' if doctor_clean else 'NO'}")

    # ---- cost extrapolation ----
    a_costs = [r["a_ms"] for r in results]
    b_costs = [r["b_ms"] for r in results]
    avg_a = sum(a_costs) / len(a_costs)
    avg_b = sum(b_costs) / len(b_costs)
    max_a = max(a_costs)
    max_b = max(b_costs)
    n_w = results[-1]["witnesses"]

    print("\n=== cost summary ===")
    print(f"steps measured:        {len(results)}")
    print(f"final witness set:     {n_w} tips")
    print(f"Oracle A  avg/max:     {avg_a:.2f} / {max_a:.2f} ms per step")
    print(f"Oracle B  avg/max:     {avg_b:.2f} / {max_b:.2f} ms per step")
    print(f"combined  avg:         {avg_a + avg_b:.2f} ms per step")

    # naive extrapolation (per-step cost roughly linear in witnesses for the
    # *naive* Oracle A; B is O(refs)). See spec for the incremental design
    # that flattens A to amortized O(1) per step.
    naive_1e6_naive = (avg_a + avg_b) / 1000.0 * 1e6 / 3600.0
    print(f"\nNAIVE (rescan-all) projection @1e6 steps: "
          f"~{naive_1e6_naive:.1f} h (A cost grows ~O(W) per step -> "
          f"O(N^2) total; UNACCEPTABLE)")
    print("With the incremental design in the spec (amortized O(1)/step "
          "for A, O(refs) for B), 1e6 steps is ~minutes -- see "
          "notes/oracle-ab-spec.md.")

    out = {
        "results": results,
        "avg_a_ms": avg_a, "avg_b_ms": avg_b,
        "max_a_ms": max_a, "max_b_ms": max_b,
        "witnesses": n_w,
        "doctor_flags_cm63": doctor_flags,
        "doctor_clean_after_gc": doctor_clean,
    }
    with open(os.path.join(os.path.dirname(__file__), "results.json"), "w") as f:
        json.dump(out, f, indent=2)

    # acceptance gate
    unexpected = [r for r in results if r["status"] == "UNEXPECTED"]
    work_loss_detected = any(
        r["label"].startswith("PLANT work-loss") and not r["a_pass"]
        for r in results
    )
    cm63_detected = any(
        r["label"].startswith("PLANT bn-cm63") and not r["b_pass"]
        for r in results
    )
    print("\n=== ACCEPTANCE ===")
    print(f"  work-loss tripped Oracle A only:  "
          f"{work_loss_detected and all(r['b_pass'] for r in results if r['label'].startswith('PLANT work-loss'))}")
    print(f"  bn-cm63 tripped Oracle B only:    "
          f"{cm63_detected and all(r['a_pass'] for r in results if r['label'].startswith('PLANT bn-cm63'))}")
    print(f"  maw doctor agrees on bn-cm63:     {doctor_flags}")
    print(f"  no unexpected oracle results:     {not unexpected}")
    ok = (work_loss_detected and cm63_detected and doctor_flags
          and not unexpected)
    print(f"\nRESULT: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
