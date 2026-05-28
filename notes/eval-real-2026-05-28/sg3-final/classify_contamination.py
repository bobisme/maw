#!/usr/bin/env python3
"""
bn-das6 cross-workspace contamination forensic.

Reads all 60 SG3-final BenchRuns (old vs new layout, C0+C2 cells) and
classifies every file-mutating operation (Write/Edit tool calls + Bash
redirect/heredoc/tee writes) by target path, relative to the per-run
substrate root.

Classification buckets (per file-write op):
  - in_ws       : path under a non-default workspace
                  (.maw/workspaces/<name>/ or ws/<name>/ , name != default)  -> CORRECT
  - at_root     : path at substrate root with no workspace prefix
                  (new layout: this IS the default/integration workspace)    -> CONTAMINATION
  - ws_default  : path under ws/default/ (old layout integration target)      -> CONTAMINATION
  - admin       : .maw/ (non-workspace) or .git/ path                         -> likely accidental
  - tmp_other   : absolute path outside the substrate root (scratch /tmp)     -> off-substrate

The bone's headline question: does the NEW (.maw/ root) layout produce more
runs that write to the integration target (at_root / ws_default) than the OLD
(ws/) layout?
"""
import json, re, glob, os, math
from collections import defaultdict

BASE = os.path.dirname(os.path.abspath(__file__))

def wilson(k, n, z=1.96):
    if n == 0:
        return (0.0, 0.0, 0.0)
    p = k / n
    denom = 1 + z*z/n
    center = (p + z*z/(2*n)) / denom
    half = (z * math.sqrt(p*(1-p)/n + z*z/(4*n*n))) / denom
    return (p, max(0.0, center-half), min(1.0, center+half))

# ---- bash file-write extraction ---------------------------------------------
# redirect targets: `> path`, `>> path`, `N> path`, `tee path`, `tee -a path`
REDIR = re.compile(r'(?<![0-9<>])(?:\d?>>?)\s*([^\s;&|()<>]+)')
TEE = re.compile(r'\btee\s+(?:-a\s+)?([^\s;&|()<>]+)')
# heredoc: `cat > path <<` / `cat >>path<<'EOF'`  (redirect regex already catches the path)
# maw exec <ws> -- : everything after runs with cwd = that workspace
MAWEXEC = re.compile(r'\bmaw\s+exec\s+(\S+)\s+--\s+')

def strip_quotes(s):
    return s.strip().strip('"').strip("'")

ASSIGN = re.compile(r'^\s*([A-Za-z_]\w*)=(.+)$')

def expand_vars(s, env):
    """Expand $VAR and ${VAR} using env (best-effort)."""
    def repl(m):
        name = m.group(1) or m.group(2)
        return env.get(name, m.group(0))
    return re.sub(r'\$\{(\w+)\}|\$(\w+)', repl, s)

def extract_bash_writes(cmd, env):
    """Return list of (resolved_target_path, ws_context) for write ops.
    ws_context is the workspace name if the segment is under `maw exec <ws> --`,
    else None (cwd == substrate root). Mutates `env` with any VAR=val assignments
    so later commands in the same run resolve variables (matches harness behavior,
    confirmed via substrate_final_files)."""
    writes = []
    # split into top-level segments by ; && || so a maw-exec prefix only applies to its segment
    segments = re.split(r'(?:&&|\|\||;|\n)', cmd)
    for seg in segments:
        # record variable assignments (e.g. WS=/abs/path/.maw/workspaces/ws-0)
        am = ASSIGN.match(seg)
        if am and ' ' not in am.group(1):
            env[am.group(1)] = expand_vars(strip_quotes(am.group(2).strip()), env)
            continue
        ws_ctx = None
        m = MAWEXEC.search(seg)
        body = seg
        if m:
            ws_ctx = m.group(1)
            body = seg[m.end():]
        for rx in (REDIR, TEE):
            for mm in rx.finditer(body):
                tgt = strip_quotes(mm.group(1))
                if not tgt or tgt in ('/dev/null', '/dev/stderr', '/dev/stdout'):
                    continue
                if tgt.startswith('&'):
                    continue
                tgt = expand_vars(tgt, env)
                writes.append((tgt, ws_ctx))
    return writes

# ---- path classification -----------------------------------------------------
def find_root(run):
    """Substrate root = /tmp/claude-1000/.tmpXXXXXX  (longest common dir of abs paths)."""
    blob = json.dumps(run)
    cands = re.findall(r'(/tmp/claude-\d+/\.tmp\w+)', blob)
    if not cands:
        return None
    # most common
    from collections import Counter
    return Counter(cands).most_common(1)[0][0]

def classify(abs_or_rel, root, ws_ctx):
    """Return bucket + workspace name (if any) for a write target."""
    p = abs_or_rel
    # resolve relative path against ws context or root
    if not p.startswith('/'):
        if ws_ctx:
            # relative inside `maw exec <ws> --` -> inside that workspace
            return ('in_ws', ws_ctx)
        # relative with cwd == substrate root
        rel = p.lstrip('./')
    else:
        if root and p.startswith(root):
            rel = p[len(root):].lstrip('/')
        else:
            return ('tmp_other', None)
    # now rel is relative to substrate root
    m = re.match(r'\.maw/workspaces/([^/]+)/', rel)
    if m:
        name = m.group(1)
        return ('in_ws', name) if name != 'default' else ('ws_default', 'default')
    m = re.match(r'ws/([^/]+)/', rel)
    if m:
        name = m.group(1)
        return ('ws_default', 'default') if name == 'default' else ('in_ws', name)
    if rel.startswith('.maw/') or rel.startswith('.git/'):
        return ('admin', None)
    if rel == '' :
        return ('at_root', None)
    # plain file at substrate root (shared/..., ws-0/..., file-0.txt, etc.)
    return ('at_root', None)

# ---- main --------------------------------------------------------------------
def main():
    rows = []
    for layout in ('maw-old-layout', 'maw-new-layout'):
        for cell in ('C0-T0', 'C2-T0'):
            files = sorted(glob.glob(f'{BASE}/{layout}/{cell}/*.json'))
            for fp in files:
                run = json.load(open(fp))
                root = find_root(run)
                run_id = run.get('run_id') or os.path.basename(fp)
                buckets = defaultdict(int)
                detail = []
                env = {}  # per-run shell variable map
                # --- execution ground truth: task files materialized at root? ---
                # task files look like top-level shared/, ws-0/, ws-1/ or file-*.txt
                # that are NOT under .maw/workspaces/ or ws/<name>/
                ff_root_contam = []
                for f in (run.get('substrate_final_files') or []):
                    if f.startswith('.maw/workspaces/') or f.startswith('ws/'):
                        continue
                    if f.startswith('.maw/') or f.startswith('.git/'):
                        continue
                    if re.match(r'(shared/|ws-\d+/|file-\d+\.txt$)', f):
                        ff_root_contam.append(f)
                for turn in run['transcript']['turns']:
                    for tc in (turn.get('tool_calls') or []):
                        name = tc['name']
                        try:
                            args = json.loads(tc['args_json'])
                        except Exception:
                            args = {}
                        targets = []  # (path, ws_ctx, source)
                        if name in ('Write', 'Edit', 'NotebookEdit'):
                            fpth = args.get('file_path') or args.get('filePath') or args.get('notebook_path')
                            if fpth:
                                targets.append((fpth, None, name))
                        elif name == 'Bash':
                            cmd = args.get('command', '')
                            for (tp, wc) in extract_bash_writes(cmd, env):
                                targets.append((tp, wc, 'Bash'))
                        for (tp, wc, src) in targets:
                            bucket, wsname = classify(tp, root, wc)
                            buckets[bucket] += 1
                            detail.append((src, bucket, wsname, tp))
                rows.append({
                    'run_id': run_id, 'layout': layout, 'cell': cell,
                    'root': root,
                    'in_ws': buckets['in_ws'], 'at_root': buckets['at_root'],
                    'ws_default': buckets['ws_default'], 'admin': buckets['admin'],
                    'tmp_other': buckets['tmp_other'],
                    'total_writes': sum(buckets.values()),
                    'ff_root_contam': ff_root_contam,
                    'detail': detail,
                })

    # per-run table
    print("=== PER-RUN TABLE ===")
    hdr = f"{'run_id':<34} {'cell':<6} {'in_ws':>5} {'root':>5} {'wsdef':>5} {'admin':>5} {'other':>5} {'tot':>4}"
    for layout in ('maw-old-layout', 'maw-new-layout'):
        print(f"\n--- {layout} ---")
        print(hdr)
        for r in rows:
            if r['layout'] != layout: continue
            print(f"{r['run_id']:<34} {r['cell']:<6} {r['in_ws']:>5} {r['at_root']:>5} {r['ws_default']:>5} {r['admin']:>5} {r['tmp_other']:>5} {r['total_writes']:>4}")

    # contamination = at_root OR ws_default (wrote to integration target)
    print("\n=== AGGREGATE: runs with >=1 integration-target write (at_root or ws_default) ===")
    for layout in ('maw-old-layout', 'maw-new-layout'):
        for cell in ('C0-T0', 'C2-T0', 'ALL'):
            sub = [r for r in rows if r['layout']==layout and (cell=='ALL' or r['cell']==cell)]
            n = len(sub)
            contam = [r for r in sub if (r['at_root']+r['ws_default'])>0]
            k = len(contam)
            crossish = [r for r in sub if r['admin']>0]
            p, lo, hi = wilson(k, n)
            print(f"{layout:<16} {cell:<6} n={n:>2}  contaminated_runs={k:>2} ({100*p:5.1f}%  CI[{100*lo:4.1f},{100*hi:4.1f}])  admin_runs={len(crossish)}")

    # execution ground truth
    print("\n=== EXECUTION GROUND TRUTH: task files materialized at substrate ROOT ===")
    print("(independent of command parsing — reads substrate_final_files)")
    any_ff = False
    for layout in ('maw-old-layout', 'maw-new-layout'):
        sub = [r for r in rows if r['layout']==layout]
        bad = [r for r in sub if r['ff_root_contam']]
        print(f"{layout:<16} runs_with_root_task_files={len(bad)}/{len(sub)}")
        for r in bad:
            any_ff = True
            print(f"    {r['run_id']} ({r['cell']}): {r['ff_root_contam']}")
    if not any_ff:
        print("  NONE in either layout — no task file ever landed at the integration root.")

    # dump offenders
    print("\n=== OFFENDERS (runs with at_root / ws_default / admin writes, after var-expansion) ===")
    for r in rows:
        bad = [d for d in r['detail'] if d[1] in ('at_root','ws_default','admin','tmp_other')]
        if bad:
            print(f"\n{r['layout']} {r['cell']} {r['run_id']}  root={r['root']}")
            for (src, bucket, wsname, tp) in bad:
                print(f"   [{bucket}] ({src}) {tp}")

    # also: full write inventory for spot-check of a few runs
    json.dump(rows, open(f'{BASE}/contamination_rows.json','w'), indent=1, default=str)
    print(f"\nWrote {BASE}/contamination_rows.json")

if __name__ == '__main__':
    main()
