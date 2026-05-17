#!/usr/bin/env python3
"""SP2 cost-scaling probe (bn-3qxi).

The harness measures absolute per-step cost on a tiny repo. To extrapolate
honestly to >=1e6 steps we must know HOW each oracle's cost scales:

  Oracle A naive   : `git rev-list --objects <frontier>` -> O(reachable
                     objects) = O(total history). Grows with N. BAD.
  Oracle A increm. : maintain a live reachable-blob set; per step only
                     diff the objects newly (un)reachable due to the step's
                     ref moves -> amortized O(delta) ~ O(1). GOOD.
  Oracle B         : O(#refs + merge-state size). #refs is bounded by
                     extant workspaces + recovery refs (GC'd) -> ~O(1)
                     w.r.t. step count. GOOD as-is.

This probe grows a repo's history and measures the naive Oracle-A primitive
(`git rev-list --objects` of HEAD) at increasing history depth to confirm
the O(history) growth, justifying the incremental design mandated for T1.3.
"""
import os
import shutil
import subprocess
import time

R = "/tmp/sp2-cost"


def g(*a, **k):
    return subprocess.run(["git", "-C", R, *a], capture_output=True,
                          text=True, **k)


def main():
    if os.path.isdir(R):
        shutil.rmtree(R)
    os.makedirs(R)
    subprocess.run(["git", "init", "-q", R], check=True)
    g("config", "user.email", "t@t")
    g("config", "user.name", "t")
    g("config", "commit.gpgsign", "false")

    depths = [100, 500, 1000, 2000, 4000, 8000]
    print(f"{'commits':>8} {'rev-list --objects ms':>22} "
          f"{'ms / 1k commits':>16}")
    made = 0
    for target in depths:
        while made < target:
            with open(os.path.join(R, "f.txt"), "w") as fh:
                fh.write(f"rev {made}\n")
            g("add", "-A")
            g("commit", "-qm", f"c{made}")
            made += 1
        # warm + measure
        g("rev-list", "--objects", "--no-object-names", "HEAD")
        n, t0 = 5, time.perf_counter()
        for _ in range(n):
            g("rev-list", "--objects", "--no-object-names", "HEAD")
        ms = (time.perf_counter() - t0) / n * 1e3
        print(f"{made:>8} {ms:>22.2f} {ms / made * 1000:>16.3f}")

    print("\nObservation: rev-list cost rises ~linearly with history depth.")
    print("=> Naive Oracle A per-step cost is O(history) -> O(N^2) over a")
    print("   run.  Mandate incremental reachable-set maintenance for T1.3")
    print("   (see notes/oracle-ab-spec.md 'Computability' section).")


if __name__ == "__main__":
    main()
