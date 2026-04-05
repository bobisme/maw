//! Pre-push LFS upload step for `maw push` (bn-14gb).
//!
//! Walks the commits being pushed, collects LFS pointer blobs from their
//! trees, and uploads the backing objects to the remote's LFS server via
//! the Batch API before the git refs are pushed.
//!
//! Feature-gated on `lfs`. If the feature is disabled, [`run`] is a no-op.

#![cfg(feature = "lfs")]

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use maw_git::{EntryMode, GitOid, GitRepo as _, GixRepo, TreeEntry};
use tracing::{debug, instrument};

/// Upload any LFS objects referenced by commits that are about to be pushed.
///
/// - `root`: repo root (worktree or main checkout).
/// - `branch`: the branch name being pushed (no `refs/heads/` prefix).
/// - `remote`: remote name (typically `"origin"`).
///
/// Reads config `lfs.push_before_git_push`: if set to `"false"`, returns
/// Ok immediately. Otherwise walks commits between `refs/remotes/<remote>/<branch>`
/// and `refs/heads/<branch>`, collects LFS pointer oids from their trees,
/// and uploads them via the Batch API. Returns an error if any upload fails.
#[instrument(skip_all, fields(branch = %branch, remote = %remote))]
pub fn run(root: &Path, branch: &str, remote: &str) -> Result<()> {
    let repo = GixRepo::open(root)
        .map_err(|e| anyhow::anyhow!("failed to open repo at {}: {e}", root.display()))?;

    // Respect the opt-out config key.
    if let Ok(Some(v)) = repo.read_config("lfs.push_before_git_push") {
        if v.eq_ignore_ascii_case("false") || v == "0" {
            debug!("lfs.push_before_git_push=false — skipping LFS pre-push upload");
            return Ok(());
        }
    }

    // Resolve local tip and (if present) remote tracking branch.
    let branch_ref = format!("refs/heads/{branch}");
    let remote_ref = format!("refs/remotes/{remote}/{branch}");

    let local_tip = match repo.rev_parse_opt(&branch_ref) {
        Ok(Some(oid)) => oid,
        Ok(None) => {
            debug!("no local branch {branch_ref}; nothing to upload");
            return Ok(());
        }
        Err(e) => bail!("failed to resolve {branch_ref}: {e}"),
    };

    let remote_tip = repo.rev_parse_opt(&remote_ref).ok().flatten();

    // Collect commits on local not on remote. For a new branch, walk all history.
    let commits = commits_to_push(&repo, local_tip, remote_tip)?;
    if commits.is_empty() {
        debug!("no new commits; skipping LFS upload");
        return Ok(());
    }

    // Collect LFS objects from the trees of each commit.
    let objects = collect_lfs_objects(&repo, &commits)?;
    if objects.is_empty() {
        debug!("no LFS objects in pushed commits");
        return Ok(());
    }

    // Resolve remote URL.
    let url_key = format!("remote.{remote}.url");
    let remote_url = repo
        .read_config(&url_key)
        .map_err(|e| anyhow::anyhow!("failed to read {url_key}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("remote '{remote}' has no url configured"))?;

    // Build BatchClient + store. Credential provider reads env + netrc.
    let creds = maw_lfs::CredentialProvider::from_env_and_netrc()
        .context("failed to initialize LFS credential provider")?;
    let mut client = maw_lfs::BatchClient::new(&remote_url, creds)
        .map_err(|e| anyhow::anyhow!("failed to construct LFS batch client: {e}"))?;

    // Open the LFS object store (lives under the common git dir).
    let store = maw_lfs::Store::open(repo.common_dir())
        .map_err(|e| anyhow::anyhow!("failed to open LFS object store: {e}"))?;

    let specs: Vec<maw_lfs::batch::ObjectSpec> = objects
        .iter()
        .map(|(oid, size)| maw_lfs::batch::ObjectSpec {
            oid: *oid,
            size: *size,
        })
        .collect();

    let n = specs.len();
    println!("Uploading {n} LFS object(s) to {remote}...");
    let report = client
        .upload(&specs, &store)
        .map_err(|e| anyhow::anyhow!("LFS batch upload failed: {e}"))?;

    if !report.failed.is_empty() {
        let details = report
            .failed
            .iter()
            .take(5)
            .map(|(oid, msg)| format!("  {oid}: {msg}"))
            .collect::<Vec<_>>()
            .join("\n");
        let more = if report.failed.len() > 5 {
            format!("\n  ... and {} more", report.failed.len() - 5)
        } else {
            String::new()
        };
        bail!(
            "LFS upload failed for {} of {n} object(s):\n{details}{more}",
            report.failed.len()
        );
    }

    println!("  Uploaded {} LFS object(s).", report.succeeded.len());
    Ok(())
}

/// Return the commit OIDs present in `local` but not in `remote`, capped at
/// a large sensible limit. For a new branch (`remote` is None), walks the
/// full history.
fn commits_to_push(
    repo: &GixRepo,
    local: GitOid,
    remote: Option<GitOid>,
) -> Result<Vec<GitOid>> {
    // Build a simple traversal: BFS from local, stop at remote tip (or at any
    // ancestor of it). This is O(new-commits) and doesn't need full graph
    // rev-list semantics — we just need the set of commits whose trees we
    // might have to scan.
    const MAX_COMMITS: usize = 10_000;
    let mut out = Vec::new();
    let mut seen: HashSet<GitOid> = HashSet::new();
    let mut queue: Vec<GitOid> = vec![local];

    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        // Stop traversal when we reach a commit that is an ancestor of the
        // remote tip (already known to the remote). Using is_ancestor here
        // short-circuits cleanly.
        if let Some(r) = remote {
            if oid == r {
                continue;
            }
            // For non-identical oids, only test ancestry — this is cheap in gix.
            match repo.is_ancestor(oid, r) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "is_ancestor check failed for {oid}..{r}: {e}"
                    ));
                }
            }
        }
        out.push(oid);
        if out.len() > MAX_COMMITS {
            bail!(
                "refusing to walk more than {MAX_COMMITS} commits during LFS pre-push scan"
            );
        }
        let commit = repo
            .read_commit(oid)
            .map_err(|e| anyhow::anyhow!("failed to read commit {oid}: {e}"))?;
        for p in commit.parents {
            if !seen.contains(&p) {
                queue.push(p);
            }
        }
    }

    Ok(out)
}

/// Walk each commit's tree and return a deduped set of `(sha256, size)` pairs
/// for blobs that are LFS pointers at LFS-tracked paths.
fn collect_lfs_objects(
    repo: &GixRepo,
    commits: &[GitOid],
) -> Result<Vec<([u8; 32], u64)>> {
    // Dedupe by (sha256, size).
    let mut out: HashMap<[u8; 32], u64> = HashMap::new();

    for commit_oid in commits {
        let commit = repo
            .read_commit(*commit_oid)
            .map_err(|e| anyhow::anyhow!("failed to read commit {commit_oid}: {e}"))?;
        collect_from_tree(repo, commit.tree_oid, &mut out)?;
    }

    Ok(out.into_iter().collect())
}

/// For a given tree, build an attrs matcher from all `.gitattributes` blobs
/// within it and walk blobs; for each blob at an LFS-tracked path, parse the
/// pointer and add `(oid, size)` to `out`.
fn collect_from_tree(
    repo: &GixRepo,
    tree_oid: GitOid,
    out: &mut HashMap<[u8; 32], u64>,
) -> Result<()> {
    // Phase 1: gather all blobs (path, blob_oid) AND .gitattributes contents.
    let mut blobs: Vec<(String, GitOid)> = Vec::new();
    let mut attrs_entries: Vec<(String, Vec<u8>)> = Vec::new();
    walk_tree(repo, tree_oid, "", &mut blobs, &mut attrs_entries)?;

    if attrs_entries.is_empty() {
        // No .gitattributes anywhere → no LFS tracking. Short-circuit.
        return Ok(());
    }

    let matcher = maw_lfs::AttrsMatcher::from_entries(attrs_entries)
        .map_err(|e| anyhow::anyhow!("failed to build attrs matcher: {e}"))?;

    for (path, blob_oid) in blobs {
        if !matcher.is_lfs(&path) {
            continue;
        }
        let bytes = repo
            .read_blob(blob_oid)
            .map_err(|e| anyhow::anyhow!("failed to read blob {blob_oid}: {e}"))?;
        if !maw_lfs::looks_like_pointer(&bytes) {
            // Not a pointer — maw may have committed real bytes, or this file
            // predates LFS tracking. Skip; nothing to upload.
            continue;
        }
        let Ok(pointer) = maw_lfs::Pointer::parse(&bytes) else {
            continue;
        };
        out.entry(pointer.oid).or_insert(pointer.size);
    }
    Ok(())
}

/// Recursively walks a tree, collecting file blob paths/oids and pulling out
/// `.gitattributes` files keyed by their directory prefix (with trailing slash).
fn walk_tree(
    repo: &GixRepo,
    tree_oid: GitOid,
    prefix: &str,
    blobs: &mut Vec<(String, GitOid)>,
    attrs_entries: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let entries: Vec<TreeEntry> = repo
        .read_tree(tree_oid)
        .map_err(|e| anyhow::anyhow!("failed to read tree {tree_oid}: {e}"))?;
    for entry in entries {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}{}", entry.name)
        };
        match entry.mode {
            EntryMode::Tree => {
                let sub_prefix = format!("{path}/");
                walk_tree(repo, entry.oid, &sub_prefix, blobs, attrs_entries)?;
            }
            EntryMode::Blob | EntryMode::BlobExecutable => {
                if entry.name == ".gitattributes" {
                    let bytes = repo.read_blob(entry.oid).map_err(|e| {
                        anyhow::anyhow!("failed to read .gitattributes blob {}: {e}", entry.oid)
                    })?;
                    // Directory prefix (with trailing slash, or empty at root).
                    attrs_entries.push((prefix.to_string(), bytes));
                } else {
                    blobs.push((path, entry.oid));
                }
            }
            EntryMode::Link | EntryMode::Commit => {
                // symlinks and submodules can't be LFS
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn run_git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn setup_repo_with_lfs_pointer() -> (tempfile::TempDir, GitOid) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-b", "main"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "t@t.test"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        fs::write(
            root.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        // A valid LFS pointer blob (sha256 of "hello world\n", size 12).
        let pointer = "version https://git-lfs.github.com/spec/v1\n\
            oid sha256:a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447\n\
            size 12\n";
        fs::write(root.join("data.bin"), pointer).unwrap();
        fs::write(root.join("readme.txt"), "not lfs\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "add lfs file"]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let head: GitOid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap();
        (dir, head)
    }

    #[test]
    fn collect_from_tree_finds_pointer_with_correct_oid_and_size() {
        let (dir, head) = setup_repo_with_lfs_pointer();
        let repo = GixRepo::open(dir.path()).unwrap();
        let commit = repo.read_commit(head).unwrap();
        let mut out = HashMap::new();
        collect_from_tree(&repo, commit.tree_oid, &mut out).unwrap();
        assert_eq!(out.len(), 1, "expected one pointer, got {out:?}");
        // sha256 of "hello world\n" is a9489...
        let expected_oid_hex = "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";
        let (oid, size) = out.into_iter().next().unwrap();
        let oid_hex: String = oid.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(oid_hex, expected_oid_hex);
        assert_eq!(size, 12);
    }

    #[test]
    fn collect_from_tree_skips_non_lfs_paths() {
        let (dir, head) = setup_repo_with_lfs_pointer();
        let repo = GixRepo::open(dir.path()).unwrap();
        let commit = repo.read_commit(head).unwrap();
        let mut out = HashMap::new();
        collect_from_tree(&repo, commit.tree_oid, &mut out).unwrap();
        // Only data.bin (LFS) — readme.txt is not matched by filter=lfs
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn collect_from_tree_empty_when_no_gitattributes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-b", "main"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "t@t.test"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("x.txt"), "plain\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "init"]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let head: GitOid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap();
        let repo = GixRepo::open(dir.path()).unwrap();
        let commit = repo.read_commit(head).unwrap();
        let mut found = HashMap::new();
        collect_from_tree(&repo, commit.tree_oid, &mut found).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn collect_dedupes_same_pointer_across_commits() {
        let (dir, head1) = setup_repo_with_lfs_pointer();
        let root = dir.path();
        // Second commit with an unrelated edit; data.bin unchanged.
        fs::write(root.join("readme.txt"), "updated\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "update readme"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let head2: GitOid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap();
        let repo = GixRepo::open(root).unwrap();
        let objs = collect_lfs_objects(&repo, &[head1, head2]).unwrap();
        assert_eq!(objs.len(), 1);
    }

    #[test]
    fn commits_to_push_new_branch_walks_full_history() {
        let (dir, head) = setup_repo_with_lfs_pointer();
        let repo = GixRepo::open(dir.path()).unwrap();
        let commits = commits_to_push(&repo, head, None).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0], head);
    }

    #[test]
    fn commits_to_push_stops_at_remote_ancestor() {
        let (dir, head1) = setup_repo_with_lfs_pointer();
        let root = dir.path();
        fs::write(root.join("readme.txt"), "v2\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "v2"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let head2: GitOid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap();
        let repo = GixRepo::open(root).unwrap();
        // Remote is at head1; local is at head2. Should return only [head2].
        let commits = commits_to_push(&repo, head2, Some(head1)).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0], head2);
    }
}
