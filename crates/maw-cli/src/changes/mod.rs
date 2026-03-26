//! `maw changes` command group and change metadata helpers.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use tempfile::Builder;

use maw_git::GitRepo as _;

use crate::format::OutputFormat;
use crate::workspace::{MawConfig, repo_root};

pub mod store;

/// `maw changes` subcommands.
#[derive(Subcommand, Debug)]
pub enum ChangesCommands {
    /// Create a tracked change from an explicit source.
    ///
    /// Creates change metadata, a change branch, and a primary workspace.
    ///
    /// Examples:
    ///   maw changes create "ASANA-123 improve cache invalidation" --from main
    ///   maw changes create "ASANA-123 improve cache invalidation" --from origin/main
    #[command(verbatim_doc_comment)]
    Create(CreateArgs),

    /// List active changes.
    ///
    /// Example:
    ///   maw changes list
    #[command(verbatim_doc_comment)]
    List(ListArgs),

    /// Show detailed metadata for one change.
    ///
    /// Example:
    ///   maw changes show ch-1xr
    #[command(verbatim_doc_comment)]
    Show(ShowArgs),

    /// Create or update a GitHub pull request for a change.
    ///
    /// Idempotent: if an open PR already exists for head/base, maw adopts it.
    ///
    /// Examples:
    ///   maw changes pr ch-1xr --draft
    ///   maw changes pr ch-1xr --ready
    #[command(verbatim_doc_comment)]
    Pr(PrArgs),

    /// Sync a change branch with its source branch.
    ///
    /// Default mode merges source into change branch. Use --rebase for
    /// history-rewriting sync.
    #[command(verbatim_doc_comment)]
    Sync(SyncArgs),

    /// Close and archive a change.
    ///
    /// By default this requires the PR to be merged.
    #[command(verbatim_doc_comment)]
    Close(CloseArgs),
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Human title for the change.
    pub title: String,

    /// Source workspace, branch, revision, or remote/branch.
    #[arg(long)]
    pub from: String,

    /// Optional explicit change id (default: generated).
    #[arg(long)]
    pub id: Option<String>,

    /// Optional primary workspace name (default: change id).
    #[arg(long)]
    pub workspace: Option<String>,

    /// Optional tracker reference (example: asana:ASANA-123).
    #[arg(long)]
    pub tracker: Option<String>,

    /// Optional tracker URL.
    #[arg(long)]
    pub tracker_url: Option<String>,

    /// Output format: text, json, pretty.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json.
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Output format: text, json, pretty.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json.
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Change id.
    pub change_id: String,

    /// Output format: text, json, pretty.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json.
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct PrArgs {
    /// Change id.
    pub change_id: String,

    /// Mark PR as draft.
    #[arg(long, conflicts_with = "ready")]
    pub draft: bool,

    /// Mark PR as ready for review.
    #[arg(long, conflicts_with = "draft")]
    pub ready: bool,

    /// Optional PR title override.
    #[arg(long)]
    pub title: Option<String>,

    /// Read PR body from file.
    #[arg(long)]
    pub body_file: Option<String>,

    /// Optional base branch override.
    #[arg(long)]
    pub base: Option<String>,

    /// Output format: text, json, pretty.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json.
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Change id.
    pub change_id: String,

    /// Rebase change branch onto source branch.
    #[arg(long)]
    pub rebase: bool,

    /// Output format: text, json, pretty.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json.
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CloseArgs {
    /// Change id.
    pub change_id: String,

    /// Delete local change branch.
    #[arg(long)]
    pub delete_branch: bool,

    /// Also delete remote branch (requires --delete-branch).
    #[arg(long, requires = "delete_branch")]
    pub remote: bool,

    /// Close even if PR is not merged.
    #[arg(long)]
    pub force: bool,

    /// Output format: text, json, pretty.
    #[arg(long)]
    pub format: Option<OutputFormat>,

    /// Shorthand for --format json.
    #[arg(long, hide = true, conflicts_with = "format")]
    pub json: bool,
}

pub fn run(cmd: &ChangesCommands) -> Result<()> {
    match cmd {
        ChangesCommands::Close(args) => close_change(args),
        ChangesCommands::Create(args) => create_change(args),
        ChangesCommands::List(args) => list_changes(args),
        ChangesCommands::Pr(args) => pr_change(args),
        ChangesCommands::Show(args) => show_change(args),
        ChangesCommands::Sync(args) => sync_change(args),
    }
}

#[derive(Debug, Serialize)]
struct CloseEnvelope {
    change_id: String,
    archived_path: String,
    branch: String,
    local_branch_deleted: bool,
    remote_branch_deleted: bool,
    force: bool,
    advice: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct GhPrView {
    state: String,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct SyncEnvelope {
    change_id: String,
    branch: String,
    source: String,
    mode: String,
    fetched_remote: bool,
    old_head: String,
    new_head: String,
    warned_force_push: bool,
    advice: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PrEnvelope {
    change_id: String,
    head_branch: String,
    base_branch: String,
    number: u64,
    url: String,
    state: String,
    draft: bool,
    created: bool,
    adopted_existing: bool,
    advice: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPrSummary {
    number: u64,
    url: String,
    state: String,
    is_draft: bool,
}

#[derive(Debug, Serialize)]
struct ChangeListItem {
    change_id: String,
    title: String,
    state: String,
    branch: String,
    primary_workspace: String,
    pr_number: Option<u64>,
    pr_state: Option<String>,
    pr_draft: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ChangeListEnvelope {
    changes: Vec<ChangeListItem>,
    advice: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ChangeShowEnvelope {
    change: store::ChangeRecord,
    advice: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CreateEnvelope {
    change_id: String,
    title: String,
    branch: String,
    source: String,
    source_oid: String,
    fetched_remote: bool,
    primary_workspace: String,
    primary_workspace_path: String,
    advice: Vec<String>,
}

fn create_change(args: &CreateArgs) -> Result<()> {
    let root = repo_root()?;
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));
    let store = store::ChangesStore::open(&root);

    let source = resolve_create_source(&root, &args.from)?;
    let source_oid = git_rev_parse(&root, &source.resolved_ref)?;

    let change_id = if let Some(explicit_id) = &args.id {
        validate_change_id_for_create(explicit_id)?;
        if store.read_active_record(explicit_id)?.is_some() {
            bail!(
                "Change id '{}' already exists.\n  Next: choose another id or omit --id for auto-generation.",
                explicit_id
            );
        }
        explicit_id.clone()
    } else {
        generate_change_id(&store, &args.title)?
    };

    let branch = format!("feat/{}-{}", change_id, slugify_title(&args.title));
    ensure_branch_absent(&root, &branch)?;
    create_branch_from_oid(&root, &branch, &source_oid)?;

    let tracker = parse_tracker(args.tracker.as_deref(), args.tracker_url.as_deref());
    let primary_workspace = args.workspace.clone().unwrap_or_else(|| change_id.clone());
    let base_branch = infer_base_branch(&root, &args.from);

    let record = store::ChangeRecord {
        schema_version: 1,
        change_id: change_id.clone(),
        title: args.title.clone(),
        state: store::ChangeState::Open,
        created_at: crate::workspace::now_timestamp_iso8601(),
        source: store::ChangeSource {
            from: args.from.clone(),
            from_oid: source_oid.clone(),
        },
        git: store::ChangeGit {
            base_branch,
            change_branch: branch.clone(),
        },
        workspaces: store::ChangeWorkspaces {
            primary: primary_workspace.clone(),
            linked: vec![primary_workspace.clone()],
        },
        tracker,
        pr: None,
    };

    store.with_lock("changes create metadata", |locked| {
        let mut index = store.read_index()?;
        index.set_branch_mapping(&branch, &change_id);
        index.set_workspace_mapping(&primary_workspace, &change_id);
        locked.write_index(&index)?;
        locked.write_active_record(&record)
    })?;

    let create_primary_result = if format == OutputFormat::Json {
        crate::workspace::create::create_quiet(
            &primary_workspace,
            None,
            Some(&change_id),
            false,
            None,
            None,
        )
    } else {
        crate::workspace::create::create(&primary_workspace, None, Some(&change_id), false, None, None)
    };
    if let Err(err) = create_primary_result {
        rollback_change_create(&root, &store, &change_id, &branch)?;
        bail!(
            "Failed to create primary workspace '{}': {err}\n  Rollback applied: removed change metadata and branch '{}'.",
            primary_workspace,
            branch
        );
    }

    let workspace_path = root.join("ws").join(&primary_workspace);
    let advice = create_change_advice(&primary_workspace, &change_id);
    let envelope = CreateEnvelope {
        change_id: change_id.clone(),
        title: args.title.clone(),
        branch,
        source: source.resolved_ref,
        source_oid,
        fetched_remote: source.fetched_remote,
        primary_workspace: primary_workspace.clone(),
        primary_workspace_path: workspace_path.display().to_string(),
        advice,
    };

    if format == OutputFormat::Json {
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    println!("Change created: {}", envelope.change_id);
    println!("  Branch: {}", envelope.branch);
    println!("  Source: {}", envelope.source);
    println!("  Primary workspace: {}/", envelope.primary_workspace_path);
    println!("Next:");
    for command in &envelope.advice {
        println!("  {command}");
    }
    Ok(())
}

fn create_change_advice(primary_workspace: &str, change_id: &str) -> Vec<String> {
    if primary_workspace == change_id {
        return vec![
            format!("maw ws create --change {change_id} <agent-workspace>"),
            "maw exec <agent-workspace> -- git add -A && maw exec <agent-workspace> -- git commit -m \"...\"".to_owned(),
            format!("maw ws merge <agent-workspace> --into {change_id} --destroy"),
            format!("maw changes pr {change_id} --draft"),
        ];
    }

    vec![
        format!(
            "maw exec {primary_workspace} -- git add -A && maw exec {primary_workspace} -- git commit -m \"...\""
        ),
        format!("maw ws merge {primary_workspace} --into {change_id} --destroy"),
        format!("maw changes pr {change_id} --draft"),
    ]
}

fn validate_change_id_for_create(change_id: &str) -> Result<()> {
    if change_id.is_empty() {
        bail!("Change id cannot be empty");
    }
    if change_id
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
    {
        bail!(
            "Invalid change id '{}': only ASCII letters, digits, '-' and '_' are allowed",
            change_id
        );
    }
    Ok(())
}

fn generate_change_id(store: &store::ChangesStore, title: &str) -> Result<String> {
    for attempt in 0..1000_u32 {
        let material = if attempt == 0 {
            title.to_owned()
        } else {
            format!("{title}:{attempt}")
        };
        let suffix = terseid::hash(material.as_bytes(), 3);
        let candidate = format!("ch-{suffix}");
        if store.read_active_record(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    bail!("Failed to generate unique change id after 1000 attempts")
}

fn slugify_title(title: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "change".to_owned()
    } else {
        slug
    }
}

fn ensure_branch_absent(root: &Path, branch: &str) -> Result<()> {
    if has_ref(root, &format!("refs/heads/{branch}"))? {
        bail!(
            "Branch '{}' already exists.\n  To fix: choose a different change id/title.",
            branch
        );
    }
    Ok(())
}

fn create_branch_from_oid(root: &Path, branch: &str, source_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["branch", branch, source_oid])
        .current_dir(root)
        .output()
        .context("Failed to run git branch")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create branch '{}': {}", branch, stderr.trim());
    }
    Ok(())
}

fn rollback_change_create(
    root: &Path,
    store: &store::ChangesStore,
    change_id: &str,
    branch: &str,
) -> Result<()> {
    store.with_lock("changes create rollback", |locked| {
        let mut index = store.read_index()?;
        index.clear_mappings_for_change(change_id);
        locked.write_index(&index)?;
        locked.delete_active_record(change_id)
    })?;

    let _ = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(root)
        .output();
    Ok(())
}

fn parse_tracker(tracker: Option<&str>, tracker_url: Option<&str>) -> Option<store::ChangeTracker> {
    let mut parsed_provider = String::new();
    let mut parsed_id = String::new();
    if let Some(raw) = tracker {
        if let Some((provider, id)) = raw.split_once(':') {
            parsed_provider = provider.to_owned();
            parsed_id = id.to_owned();
        } else {
            parsed_id = raw.to_owned();
        }
    }

    if parsed_provider.is_empty() && parsed_id.is_empty() && tracker_url.is_none() {
        return None;
    }

    Some(store::ChangeTracker {
        provider: parsed_provider,
        id: parsed_id,
        url: tracker_url.unwrap_or_default().to_owned(),
    })
}

fn infer_base_branch(root: &Path, from: &str) -> String {
    if let Some((remote, branch)) = from.split_once('/')
        && !branch.is_empty()
    {
        match remote_exists(root, remote) {
            Ok(true) => return branch.to_owned(),
            Ok(false) | Err(_) => {}
        }
    }
    from.to_owned()
}

fn resolve_create_source(root: &Path, source_spec: &str) -> Result<ResolvedSource> {
    // Workspace source shorthand: if source_spec names an active workspace,
    // resolve to its current git HEAD commit OID.  The oplog ref
    // `refs/manifold/head/<workspace>` points to an operation blob, not a
    // commit, so we must not return it directly.
    let workspace_head_ref = maw_core::refs::workspace_head_ref(source_spec);
    if maw_core::refs::read_ref(root, &workspace_head_ref)
        .map_err(|e| anyhow::anyhow!("Failed to read workspace source ref: {e}"))?
        .is_some()
    {
        let ws_path = root.join("ws").join(source_spec);
        let repo = maw_git::GixRepo::open(&ws_path)
            .map_err(|e| anyhow::anyhow!("Failed to open workspace '{source_spec}': {e}"))?;
        let head_oid = repo
            .rev_parse("HEAD")
            .map_err(|e| anyhow::anyhow!("Failed to resolve HEAD of workspace '{source_spec}': {e}"))?;
        return Ok(ResolvedSource {
            resolved_ref: head_oid.to_string(),
            fetched_remote: false,
        });
    }

    // Delegate branch/rev/remote resolution to sync resolver.
    resolve_source_ref(root, source_spec)
}

fn list_changes(args: &ListArgs) -> Result<()> {
    let root = repo_root()?;
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));
    let store = store::ChangesStore::open(&root);

    let records = store.list_active_records()?;
    let items: Vec<ChangeListItem> = records
        .into_iter()
        .map(|record| ChangeListItem {
            change_id: record.change_id,
            title: record.title,
            state: format_change_state(&record.state),
            branch: record.git.change_branch,
            primary_workspace: record.workspaces.primary,
            pr_number: record.pr.as_ref().map(|pr| pr.number),
            pr_state: record.pr.as_ref().map(|pr| pr.state.clone()),
            pr_draft: record.pr.as_ref().map(|pr| pr.draft),
        })
        .collect();

    let envelope = ChangeListEnvelope {
        changes: items,
        advice: vec![
            "maw changes show <change-id>".to_owned(),
            "maw changes create \"<title>\" --from origin/main".to_owned(),
        ],
    };

    if format == OutputFormat::Json {
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    if envelope.changes.is_empty() {
        println!("No active changes.");
        println!("Next:");
        println!("  maw changes create \"<title>\" --from origin/main");
        return Ok(());
    }

    println!("Active changes: {}", envelope.changes.len());
    for change in &envelope.changes {
        let pr_summary = match (
            change.pr_number,
            change.pr_state.as_deref(),
            change.pr_draft,
        ) {
            (Some(number), Some(state), Some(true)) => {
                format!("PR #{number} {state} draft")
            }
            (Some(number), Some(state), _) => format!("PR #{number} {state}"),
            (Some(number), None, _) => format!("PR #{number}"),
            _ => "no PR".to_owned(),
        };

        println!("  - {}: {}", change.change_id, change.title);
        println!(
            "    state={} branch={} workspace={} {}",
            change.state, change.branch, change.primary_workspace, pr_summary
        );
    }
    println!("Next:");
    println!("  maw changes show <change-id>");
    Ok(())
}

fn show_change(args: &ShowArgs) -> Result<()> {
    let root = repo_root()?;
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));
    let store = store::ChangesStore::open(&root);

    let record = store.read_active_record(&args.change_id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Change '{}' not found in active changes.\n  Next: maw changes list",
            args.change_id
        )
    })?;

    let envelope = ChangeShowEnvelope {
        change: record,
        advice: vec![
            format!("maw changes pr {}", args.change_id),
            format!("maw changes sync {}", args.change_id),
            format!("maw changes close {}", args.change_id),
        ],
    };

    if format == OutputFormat::Json {
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    println!("Change: {}", envelope.change.change_id);
    println!("  Title:   {}", envelope.change.title);
    println!("  State:   {}", format_change_state(&envelope.change.state));
    println!("  Source:  {}", envelope.change.source.from);
    println!("  Branch:  {}", envelope.change.git.change_branch);
    println!("  Base:    {}", envelope.change.git.base_branch);
    println!(
        "  Primary workspace: {}",
        envelope.change.workspaces.primary
    );
    if envelope.change.workspaces.linked.is_empty() {
        println!("  Linked workspaces: none");
    } else {
        println!("  Linked workspaces:");
        for workspace in &envelope.change.workspaces.linked {
            println!("    - {workspace}");
        }
    }

    if let Some(tracker) = &envelope.change.tracker {
        println!("  Tracker: {}:{}", tracker.provider, tracker.id);
        if !tracker.url.is_empty() {
            println!("  Tracker URL: {}", tracker.url);
        }
    }

    if let Some(pr) = &envelope.change.pr {
        println!("  PR: #{} {} draft={}", pr.number, pr.state, pr.draft);
        if !pr.url.is_empty() {
            println!("  PR URL: {}", pr.url);
        }
    } else {
        println!("  PR: none");
    }

    println!("Next:");
    println!("  maw changes pr {}", envelope.change.change_id);
    println!("  maw changes sync {}", envelope.change.change_id);
    Ok(())
}

fn format_change_state(state: &store::ChangeState) -> String {
    match state {
        store::ChangeState::Open => "open".to_owned(),
        store::ChangeState::Review => "review".to_owned(),
        store::ChangeState::Merged => "merged".to_owned(),
        store::ChangeState::Closed => "closed".to_owned(),
        store::ChangeState::Aborted => "aborted".to_owned(),
    }
}

fn close_change(args: &CloseArgs) -> Result<()> {
    let root = repo_root()?;
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));
    let store = store::ChangesStore::open(&root);

    let mut record = store
        .read_active_record(&args.change_id)?
        .ok_or_else(|| anyhow::anyhow!(
            "Change '{}' not found in active changes.\n  Next: list known changes: maw changes list",
            args.change_id
        ))?;

    ensure_linked_workspaces_resolved(&root, &record, args.force)?;
    ensure_pr_merged_if_required(&root, &mut record, args.force)?;

    record.state = if is_pr_merged(&record) {
        store::ChangeState::Merged
    } else {
        store::ChangeState::Closed
    };

    let (local_deleted, remote_deleted) = delete_change_branch_if_requested(
        &root,
        &record.git.change_branch,
        args.delete_branch,
        args.remote,
        args.force,
    )?;

    let archive_stamp = crate::workspace::now_timestamp_iso8601();
    let archived_path = store.with_lock("changes close", |locked| {
        locked.write_active_record(&record)?;

        let mut index = store.read_index()?;
        index.clear_mappings_for_change(&record.change_id);
        locked.write_index(&index)?;

        locked
            .archive_active_record(&record.change_id, &archive_stamp)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Change '{}' disappeared before archive step.",
                    record.change_id
                )
            })
    })?;

    let envelope = CloseEnvelope {
        change_id: record.change_id.clone(),
        archived_path: archived_path.display().to_string(),
        branch: record.git.change_branch.clone(),
        local_branch_deleted: local_deleted,
        remote_branch_deleted: remote_deleted,
        force: args.force,
        advice: vec!["maw changes list".to_owned()],
    };

    if format == OutputFormat::Json {
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    println!("Change closed: {}", envelope.change_id);
    println!("  Archived: {}", envelope.archived_path);
    println!("  Branch:   {}", envelope.branch);
    println!(
        "  Deleted:  local={} remote={}",
        envelope.local_branch_deleted, envelope.remote_branch_deleted
    );
    println!("Next:");
    println!("  maw changes list");
    Ok(())
}

fn ensure_linked_workspaces_resolved(
    root: &Path,
    record: &store::ChangeRecord,
    force: bool,
) -> Result<()> {
    let existing: Vec<String> = record
        .workspaces
        .linked
        .iter()
        .filter(|workspace| root.join("ws").join(workspace).exists())
        .cloned()
        .collect();

    if existing.is_empty() || force {
        return Ok(());
    }

    let joined = existing.join(", ");
    bail!(
        "Cannot close change '{}' while linked workspaces still exist: {}\n  To fix: merge or destroy these workspaces, or force close: maw changes close {} --force",
        record.change_id,
        joined,
        record.change_id
    );
}

fn ensure_pr_merged_if_required(
    root: &Path,
    record: &mut store::ChangeRecord,
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }

    let pr = record.pr.as_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot close change '{}' without linked PR metadata.\n  To fix: run maw changes pr {} or force close: maw changes close {} --force",
            record.change_id,
            record.change_id,
            record.change_id
        )
    })?;

    if pr.state.eq_ignore_ascii_case("merged") {
        return Ok(());
    }

    let view = gh_pr_view(root, pr.number)?;
    if view.merged_at.is_some() {
        pr.state = "merged".to_owned();
        return Ok(());
    }

    bail!(
        "Cannot close change '{}': PR #{} is not merged (state: {}).\n  To fix: merge the PR first, or force close: maw changes close {} --force",
        record.change_id,
        pr.number,
        view.state,
        record.change_id
    );
}

fn is_pr_merged(record: &store::ChangeRecord) -> bool {
    record
        .pr
        .as_ref()
        .is_some_and(|pr| pr.state.eq_ignore_ascii_case("merged"))
}

fn gh_pr_view(root: &Path, pr_number: u64) -> Result<GhPrView> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "state,mergedAt",
        ])
        .current_dir(root)
        .output()
        .context("Failed to run gh pr view")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to query PR #{} via gh: {}\n  To fix: ensure gh is authenticated, or force close: maw changes close <id> --force",
            pr_number,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let view: GhPrView = serde_json::from_str(&stdout)
        .with_context(|| format!("Failed to parse gh output for PR #{pr_number}"))?;
    Ok(view)
}

fn delete_change_branch_if_requested(
    root: &Path,
    branch: &str,
    delete_branch: bool,
    delete_remote: bool,
    force: bool,
) -> Result<(bool, bool)> {
    if !delete_branch {
        return Ok((false, false));
    }

    let config = MawConfig::load(root)?;
    if branch == config.branch() {
        bail!(
            "Refusing to delete configured trunk branch '{}'.",
            config.branch()
        );
    }

    let local_flag = if force { "-D" } else { "-d" };
    let local = Command::new("git")
        .args(["branch", local_flag, branch])
        .current_dir(root)
        .output()
        .context("Failed to run git branch delete")?;
    if !local.status.success() {
        let stderr = String::from_utf8_lossy(&local.stderr);
        bail!(
            "Failed to delete local branch '{}': {}\n  To fix: inspect branch state, or re-run with --force",
            branch,
            stderr.trim()
        );
    }

    if !delete_remote {
        return Ok((true, false));
    }

    let remote = Command::new("git")
        .args(["push", "origin", "--delete", branch])
        .current_dir(root)
        .output()
        .context("Failed to run remote branch delete")?;
    if !remote.status.success() {
        let stderr = String::from_utf8_lossy(&remote.stderr);
        bail!(
            "Local branch '{}' deleted, but remote delete failed: {}\n  To fix: retry remote delete with: git -C {} push origin --delete {}",
            branch,
            stderr.trim(),
            root.display(),
            branch
        );
    }

    Ok((true, true))
}

fn pr_change(args: &PrArgs) -> Result<()> {
    let root = repo_root()?;
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));
    let store = store::ChangesStore::open(&root);

    let mut record = store
        .read_active_record(&args.change_id)?
        .ok_or_else(|| anyhow::anyhow!(
            "Change '{}' not found in active changes.\n  Next: list known changes: maw changes list",
            args.change_id
        ))?;

    let head_branch = record.git.change_branch.trim().to_owned();
    if head_branch.is_empty() {
        bail!(
            "Change '{}' has no configured change branch.\n  To fix: repair metadata and retry.",
            record.change_id
        );
    }

    ensure_local_branch_exists(&root, &head_branch)?;

    let base_branch = if let Some(base) = &args.base {
        base.clone()
    } else if !record.git.base_branch.trim().is_empty() {
        record.git.base_branch.trim().to_owned()
    } else {
        "main".to_owned()
    };

    push_change_branch(&root, &head_branch)?;

    let mut pr = find_open_pr(&root, &head_branch, &base_branch)?;
    let mut created = false;

    if pr.is_none() {
        create_pr(&root, &head_branch, &base_branch, args)?;
        pr = find_open_pr(&root, &head_branch, &base_branch)?;
        created = true;
    }

    let mut pr = pr.ok_or_else(|| {
        anyhow::anyhow!(
            "Could not find an open PR for change '{}' after create/adopt flow.",
            record.change_id
        )
    })?;

    apply_pr_edits(&root, pr.number, args)?;
    apply_pr_draft_toggle(&root, &mut pr, args)?;

    record.pr = Some(store::ChangePr {
        number: pr.number,
        url: pr.url.clone(),
        state: pr.state.to_lowercase(),
        draft: pr.is_draft,
    });

    store.with_lock("changes pr metadata", |locked| {
        locked.write_active_record(&record)
    })?;

    let envelope = PrEnvelope {
        change_id: record.change_id,
        head_branch,
        base_branch,
        number: pr.number,
        url: pr.url,
        state: pr.state,
        draft: pr.is_draft,
        created,
        adopted_existing: !created,
        advice: vec![format!("maw changes show {}", args.change_id)],
    };

    if format == OutputFormat::Json {
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    println!("Change PR ready: {}", envelope.change_id);
    println!("  PR:      #{}", envelope.number);
    println!("  URL:     {}", envelope.url);
    println!("  State:   {} (draft={})", envelope.state, envelope.draft);
    println!(
        "  Branch:  {} -> {}",
        envelope.head_branch, envelope.base_branch
    );
    println!(
        "  Action:  {}",
        if envelope.created {
            "created"
        } else {
            "adopted existing"
        }
    );
    println!("Next:");
    println!("  maw changes show {}", args.change_id);
    Ok(())
}

fn ensure_local_branch_exists(root: &Path, branch: &str) -> Result<()> {
    let branch_ref = format!("refs/heads/{branch}");
    if has_ref(root, &branch_ref)? {
        return Ok(());
    }
    bail!(
        "Local branch '{}' does not exist.\n  To fix: create the change branch first.",
        branch
    );
}

fn push_change_branch(root: &Path, head_branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["push", "-u", "origin", head_branch])
        .current_dir(root)
        .output()
        .context("Failed to run git push for change branch")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to push branch '{}' to origin: {}",
            head_branch,
            stderr.trim()
        );
    }
    Ok(())
}

fn find_open_pr(root: &Path, head_branch: &str, base_branch: &str) -> Result<Option<GhPrSummary>> {
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            head_branch,
            "--base",
            base_branch,
            "--state",
            "open",
            "--json",
            "number,url,state,isDraft",
        ])
        .current_dir(root)
        .output()
        .context("Failed to run gh pr list")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to list PRs with gh: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut prs: Vec<GhPrSummary> =
        serde_json::from_str(&stdout).context("Failed to parse gh pr list output")?;
    prs.sort_by_key(|pr| pr.number);
    Ok(prs.into_iter().next())
}

fn create_pr(root: &Path, head_branch: &str, base_branch: &str, args: &PrArgs) -> Result<()> {
    let mut command = Command::new("gh");
    command
        .arg("pr")
        .arg("create")
        .arg("--head")
        .arg(head_branch)
        .arg("--base")
        .arg(base_branch);

    if args.draft {
        command.arg("--draft");
    }
    if let Some(title) = &args.title {
        command.arg("--title").arg(title);
    }
    if let Some(body_file) = &args.body_file {
        command.arg("--body-file").arg(body_file);
    }
    if args.title.is_none() && args.body_file.is_none() {
        command.arg("--fill");
    }

    let output = command
        .current_dir(root)
        .output()
        .context("Failed to run gh pr create")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create PR with gh: {}", stderr.trim());
    }
    Ok(())
}

fn apply_pr_edits(root: &Path, pr_number: u64, args: &PrArgs) -> Result<()> {
    if args.title.is_none() && args.body_file.is_none() && args.base.is_none() {
        return Ok(());
    }

    let mut command = Command::new("gh");
    command.arg("pr").arg("edit").arg(pr_number.to_string());
    if let Some(title) = &args.title {
        command.arg("--title").arg(title);
    }
    if let Some(body_file) = &args.body_file {
        command.arg("--body-file").arg(body_file);
    }
    if let Some(base) = &args.base {
        command.arg("--base").arg(base);
    }

    let output = command
        .current_dir(root)
        .output()
        .context("Failed to run gh pr edit")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to edit PR #{}: {}", pr_number, stderr.trim());
    }
    Ok(())
}

fn apply_pr_draft_toggle(root: &Path, pr: &mut GhPrSummary, args: &PrArgs) -> Result<()> {
    if args.ready && pr.is_draft {
        let output = Command::new("gh")
            .args(["pr", "ready", &pr.number.to_string()])
            .current_dir(root)
            .output()
            .context("Failed to run gh pr ready")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to mark PR #{} ready: {}", pr.number, stderr.trim());
        }
        pr.is_draft = false;
    }

    if args.draft && !pr.is_draft {
        let output = Command::new("gh")
            .args(["pr", "ready", "--undo", &pr.number.to_string()])
            .current_dir(root)
            .output()
            .context("Failed to run gh pr ready --undo")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Failed to convert PR #{} to draft: {}",
                pr.number,
                stderr.trim()
            );
        }
        pr.is_draft = true;
    }

    Ok(())
}

fn sync_change(args: &SyncArgs) -> Result<()> {
    let root = repo_root()?;
    let format = OutputFormat::resolve(OutputFormat::with_json_flag(args.format, args.json));
    let store = store::ChangesStore::open(&root);

    let record = store
        .read_active_record(&args.change_id)?
        .ok_or_else(|| anyhow::anyhow!(
            "Change '{}' not found in active changes.\n  Next: list known changes: maw changes list",
            args.change_id
        ))?;

    let change_branch = record.git.change_branch.trim().to_owned();
    if change_branch.is_empty() {
        bail!(
            "Change '{}' has no configured change branch.\n  To fix: repair metadata and retry.",
            record.change_id
        );
    }

    let source_spec = if !record.source.from.trim().is_empty() {
        record.source.from.trim().to_owned()
    } else if !record.git.base_branch.trim().is_empty() {
        record.git.base_branch.trim().to_owned()
    } else {
        bail!(
            "Change '{}' has no source branch in metadata.\n  To fix: update change metadata, then retry.",
            record.change_id
        );
    };

    let source = resolve_source_ref(&root, &source_spec)?;
    let branch_ref = format!("refs/heads/{change_branch}");
    let old_head = git_rev_parse(&root, &branch_ref)?;
    let published_on_origin = has_ref(&root, &format!("refs/remotes/origin/{change_branch}"))?;

    let tmp_parent = root.join(".manifold").join("tmp");
    fs::create_dir_all(&tmp_parent)
        .with_context(|| format!("Failed to create temp dir: {}", tmp_parent.display()))?;
    let temp_worktree = Builder::new()
        .prefix("changes-sync-")
        .tempdir_in(&tmp_parent)
        .with_context(|| {
            format!(
                "Failed to create temp worktree under {}",
                tmp_parent.display()
            )
        })?;
    let temp_path = temp_worktree.path().to_path_buf();

    add_detached_worktree(&root, &temp_path, &old_head)?;

    let sync_result = if args.rebase {
        git_output(&temp_path, &["rebase", &source.resolved_ref])
    } else {
        git_output(&temp_path, &["merge", "--no-edit", &source.resolved_ref])
    };

    let output = match sync_result {
        Ok(output) => output,
        Err(err) => {
            cleanup_temp_worktree(&root, &temp_path);
            return Err(err);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_temp_worktree(&root, &temp_path);
        if args.rebase {
            bail!(
                "Change sync (rebase) failed for '{}': {}\n  To fix: resolve conflicts manually and retry sync.",
                record.change_id,
                stderr.trim()
            );
        }
        bail!(
            "Change sync (merge) failed for '{}': {}\n  To fix: resolve conflicts manually and retry sync.",
            record.change_id,
            stderr.trim()
        );
    }

    let new_head = git_rev_parse(&temp_path, "HEAD")?;
    update_ref_cas(&root, &branch_ref, &new_head, &old_head)?;
    cleanup_temp_worktree(&root, &temp_path);

    let warned_force_push = args.rebase && published_on_origin && old_head != new_head;

    let envelope = SyncEnvelope {
        change_id: record.change_id,
        branch: change_branch,
        source: source.resolved_ref,
        mode: if args.rebase {
            "rebase".to_owned()
        } else {
            "merge".to_owned()
        },
        fetched_remote: source.fetched_remote,
        old_head,
        new_head,
        warned_force_push,
        advice: if warned_force_push {
            vec!["git push --force-with-lease origin <change-branch>".to_owned()]
        } else {
            vec!["maw changes pr <change-id>".to_owned()]
        },
    };

    if format == OutputFormat::Json {
        println!("{}", format.serialize(&envelope)?);
        return Ok(());
    }

    println!("Change synced: {}", envelope.change_id);
    println!("  Branch: {}", envelope.branch);
    println!("  Source: {}", envelope.source);
    println!("  Mode:   {}", envelope.mode);
    println!("  Head:   {} -> {}", envelope.old_head, envelope.new_head);
    if warned_force_push {
        println!(
            "WARNING: rebase rewrote a published branch; next push should use --force-with-lease."
        );
        println!("Next:");
        println!(
            "  git -C {} push --force-with-lease origin {}",
            root.display(),
            envelope.branch
        );
    } else {
        println!("Next:");
        println!("  maw changes pr {}", envelope.change_id);
    }
    Ok(())
}

struct ResolvedSource {
    resolved_ref: String,
    fetched_remote: bool,
}

fn resolve_source_ref(root: &Path, source_spec: &str) -> Result<ResolvedSource> {
    if let Some((remote, branch)) = source_spec.split_once('/')
        && !remote.is_empty()
        && !branch.is_empty()
        && remote_exists(root, remote)?
    {
        let fetch = Command::new("git")
            .args(["fetch", remote, branch, "--no-tags", "--quiet"])
            .current_dir(root)
            .output()
            .context("Failed to run git fetch for sync source")?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            bail!(
                "Failed to fetch sync source '{}': {}",
                source_spec,
                stderr.trim()
            );
        }
        return Ok(ResolvedSource {
            resolved_ref: source_spec.to_owned(),
            fetched_remote: true,
        });
    }

    Ok(ResolvedSource {
        resolved_ref: source_spec.to_owned(),
        fetched_remote: false,
    })
}

fn remote_exists(root: &Path, remote: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(root)
        .output()
        .context("Failed to run git remote get-url")?;
    Ok(output.status.success())
}

fn add_detached_worktree(root: &Path, temp_path: &Path, start_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &temp_path.display().to_string(),
            start_oid,
        ])
        .current_dir(root)
        .output()
        .context("Failed to run git worktree add")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to create temporary sync worktree: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn cleanup_temp_worktree(root: &Path, temp_path: &Path) {
    let _ = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &temp_path.display().to_string(),
        ])
        .current_dir(root)
        .output();
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))
}

fn git_rev_parse(cwd: &Path, rev: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to run git rev-parse {rev}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to resolve '{rev}': {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn has_ref(cwd: &Path, git_ref: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", git_ref])
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to check ref '{git_ref}'"))?;
    Ok(output.status.success())
}

fn update_ref_cas(cwd: &Path, git_ref: &str, new_oid: &str, old_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["update-ref", git_ref, new_oid, old_oid])
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to update ref '{git_ref}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to update branch ref '{}': {}\n  To fix: retry sync after refreshing refs.",
            git_ref,
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct CwdGuard {
        previous: PathBuf,
    }

    impl CwdGuard {
        fn enter(path: &Path) -> Self {
            let previous = std::env::current_dir().expect("read current dir");
            std::env::set_current_dir(path).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    struct GhBinaryGuard {
        gh_path: PathBuf,
        backup_path: Option<PathBuf>,
    }

    impl GhBinaryGuard {
        fn install(script: &str) -> Self {
            let gh_path = PathBuf::from("/home/bob/bin/gh");
            if let Some(parent) = gh_path.parent() {
                fs::create_dir_all(parent).expect("create /home/bob/bin");
            }

            let backup_path = if gh_path.exists() {
                let backup = PathBuf::from(format!("{}.maw-test-backup", gh_path.display()));
                fs::rename(&gh_path, &backup).expect("backup existing gh binary");
                Some(backup)
            } else {
                None
            };

            fs::write(&gh_path, script).expect("write gh stub");
            let mut perms = fs::metadata(&gh_path).expect("gh metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&gh_path, perms).expect("chmod gh stub");

            Self {
                gh_path,
                backup_path,
            }
        }
    }

    impl Drop for GhBinaryGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.gh_path);
            if let Some(backup) = &self.backup_path {
                let _ = fs::rename(backup, &self.gh_path);
            }
        }
    }

    struct TestRepo {
        _tmp: TempDir,
        root: PathBuf,
    }

    struct GhStub {
        log_path: PathBuf,
        _gh_guard: GhBinaryGuard,
    }

    impl TestRepo {
        fn new(with_origin_remote: bool) -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().to_path_buf();
            setup_git_repo(&root);

            {
                let _lock = test_lock().lock().unwrap_or_else(|e| e.into_inner());
                let _cwd = CwdGuard::enter(&root);
                crate::v2_init::run().expect("maw init should succeed in test repo");
            }

            if with_origin_remote {
                let remote = root.join("origin.git");
                run_git(&root, &["init", "--bare", &remote.display().to_string()]);
                run_git(
                    &root,
                    &["remote", "add", "origin", &remote.display().to_string()],
                );
                run_git(&root, &["push", "-u", "origin", "main"]);
            }

            Self { _tmp: tmp, root }
        }
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_status_ok(root: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn setup_git_repo(root: &Path) {
        run_git(root, &["init"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("README.md"), "hello\n").expect("write readme");
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-m", "initial"]);
    }

    fn with_repo_cwd<T>(repo: &TestRepo, f: impl FnOnce(&Path) -> T) -> T {
        let _lock = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _cwd = CwdGuard::enter(&repo.root);
        f(&repo.root)
    }

    fn commit_in_default_workspace(root: &Path, message: &str, content: &str) {
        let default_ws = root.join("ws/default");
        fs::write(default_ws.join("README.md"), content).expect("write default readme");
        run_git(&default_ws, &["add", "README.md"]);
        run_git(&default_ws, &["commit", "-m", message]);
    }

    fn install_gh_stub(root: &Path, first_list_json: &str, second_list_json: &str) -> GhStub {
        let list1 = root.join("gh-list-1.json");
        let list2 = root.join("gh-list-2.json");
        let log = root.join("gh.log");
        let count = root.join("gh.count");

        fs::write(&list1, first_list_json).expect("write gh list response #1");
        fs::write(&list2, second_list_json).expect("write gh list response #2");
        fs::write(&log, "").expect("init gh log");
        fs::write(&count, "0").expect("init gh count");

        let script = format!(
            r#"#!/bin/sh
set -eu
LOG='{}'
COUNT='{}'
LIST1='{}'
LIST2='{}'
echo "$@" >> "$LOG"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  c=$(cat "$COUNT")
  c=$((c+1))
  echo "$c" > "$COUNT"
  if [ "$c" -eq 1 ]; then
    cat "$LIST1"
  else
    cat "$LIST2"
  fi
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "ready" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '{{"state":"MERGED","mergedAt":"2026-01-01T00:00:00Z"}}\n'
  exit 0
fi
echo "unsupported gh stub args: $*" >&2
exit 1
"#,
            log.display(),
            count.display(),
            list1.display(),
            list2.display()
        );

        let gh_guard = GhBinaryGuard::install(&script);

        GhStub {
            log_path: log,
            _gh_guard: gh_guard,
        }
    }

    #[test]
    fn create_change_persists_metadata_and_primary_workspace() {
        let repo = TestRepo::new(false);

        with_repo_cwd(&repo, |root| {
            let args = CreateArgs {
                title: "ASANA-1 add feature".to_string(),
                from: "main".to_string(),
                id: Some("ch-create".to_string()),
                workspace: Some("create-ws".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            };

            create_change(&args).expect("create change");

            let store = store::ChangesStore::open(root);
            let record = store
                .read_active_record("ch-create")
                .expect("read active")
                .expect("record present");
            assert_eq!(record.change_id, "ch-create");
            assert_eq!(record.source.from, "main");
            assert_eq!(record.workspaces.primary, "create-ws");
            assert!(record.workspaces.linked.contains(&"create-ws".to_string()));

            let index = store.read_index().expect("read index");
            assert_eq!(index.change_for_workspace("create-ws"), Some("ch-create"));
            assert_eq!(
                index.change_for_branch(&record.git.change_branch),
                Some("ch-create")
            );

            assert!(
                root.join("ws/create-ws").exists(),
                "primary workspace exists"
            );
        });
    }

    #[test]
    fn ws_create_change_binding_updates_index_and_change_linked_workspaces() {
        let repo = TestRepo::new(false);

        with_repo_cwd(&repo, |root| {
            create_change(&CreateArgs {
                title: "ASANA-2 binding".to_string(),
                from: "main".to_string(),
                id: Some("ch-bind".to_string()),
                workspace: Some("bind-primary".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            })
            .expect("create change");

            crate::workspace::create::create("bind-extra", None, Some("ch-bind"), false, None, None)
                .expect("create bound workspace");

            let store = store::ChangesStore::open(root);
            let record = store
                .read_active_record("ch-bind")
                .expect("read active")
                .expect("record present");
            assert!(record.workspaces.linked.contains(&"bind-extra".to_string()));

            let index = store.read_index().expect("read index");
            assert_eq!(index.change_for_workspace("bind-extra"), Some("ch-bind"));
        });
    }

    #[test]
    fn sync_change_merge_mode_advances_change_branch() {
        let repo = TestRepo::new(false);

        with_repo_cwd(&repo, |root| {
            create_change(&CreateArgs {
                title: "ASANA-3 sync".to_string(),
                from: "main".to_string(),
                id: Some("ch-sync".to_string()),
                workspace: Some("sync-ws".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            })
            .expect("create change");

            let store = store::ChangesStore::open(root);
            let record = store
                .read_active_record("ch-sync")
                .expect("read active")
                .expect("record present");
            let branch_ref = format!("refs/heads/{}", record.git.change_branch);
            let old_head = git_rev_parse(root, &branch_ref).expect("old branch head");

            commit_in_default_workspace(root, "main update", "sync update\n");
            let main_head = git_rev_parse(root, "refs/heads/main").expect("main head");

            sync_change(&SyncArgs {
                change_id: "ch-sync".to_string(),
                rebase: false,
                format: None,
                json: false,
            })
            .expect("sync change");

            let new_head = git_rev_parse(root, &branch_ref).expect("new branch head");
            assert_ne!(old_head, new_head, "sync should advance branch head");
            assert!(
                git_status_ok(
                    root,
                    &["merge-base", "--is-ancestor", &main_head, &new_head]
                ),
                "synced branch should contain latest main commit"
            );
        });
    }

    #[test]
    fn sync_change_rebase_mode_advances_change_branch() {
        let repo = TestRepo::new(false);

        with_repo_cwd(&repo, |root| {
            create_change(&CreateArgs {
                title: "ASANA-3b rebase".to_string(),
                from: "main".to_string(),
                id: Some("ch-rebase".to_string()),
                workspace: Some("rebase-ws".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            })
            .expect("create change");

            let store = store::ChangesStore::open(root);
            let record = store
                .read_active_record("ch-rebase")
                .expect("read active")
                .expect("record present");
            let branch_ref = format!("refs/heads/{}", record.git.change_branch);
            let old_head = git_rev_parse(root, &branch_ref).expect("old branch head");

            commit_in_default_workspace(root, "main update for rebase", "rebase update\n");
            let main_head = git_rev_parse(root, "refs/heads/main").expect("main head");

            sync_change(&SyncArgs {
                change_id: "ch-rebase".to_string(),
                rebase: true,
                format: None,
                json: false,
            })
            .expect("sync change with rebase");

            let new_head = git_rev_parse(root, &branch_ref).expect("new branch head");
            assert_ne!(old_head, new_head, "rebase should advance branch head");
            assert!(
                git_status_ok(
                    root,
                    &["merge-base", "--is-ancestor", &main_head, &new_head]
                ),
                "rebased branch should contain latest main commit"
            );
        });
    }

    #[test]
    fn close_change_archives_record_and_cleans_index_and_branch() {
        let repo = TestRepo::new(false);

        with_repo_cwd(&repo, |root| {
            create_change(&CreateArgs {
                title: "ASANA-4 close".to_string(),
                from: "main".to_string(),
                id: Some("ch-close".to_string()),
                workspace: Some("close-ws".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            })
            .expect("create change");

            crate::workspace::create::destroy("close-ws", false, false)
                .expect("destroy linked workspace before close");

            let store = store::ChangesStore::open(root);
            let mut record = store
                .read_active_record("ch-close")
                .expect("read active")
                .expect("record present");
            record.pr = Some(store::ChangePr {
                number: 77,
                url: "https://example.test/pr/77".to_string(),
                state: "merged".to_string(),
                draft: false,
            });
            let branch = record.git.change_branch.clone();
            store
                .with_lock("tests write merged state", |locked| {
                    locked.write_active_record(&record)
                })
                .expect("write merged pr metadata");

            close_change(&CloseArgs {
                change_id: "ch-close".to_string(),
                delete_branch: true,
                remote: false,
                force: false,
                format: None,
                json: false,
            })
            .expect("close change");

            assert!(
                store
                    .read_active_record("ch-close")
                    .expect("read active")
                    .is_none(),
                "active record should be removed"
            );

            let index = store.read_index().expect("read index");
            assert!(
                index.change_for_workspace("close-ws").is_none(),
                "workspace mapping should be removed"
            );
            assert!(
                index.change_for_branch(&branch).is_none(),
                "branch mapping should be removed"
            );
            assert!(
                !has_ref(root, &format!("refs/heads/{branch}")).expect("check branch ref"),
                "local branch should be deleted"
            );

            let archive_entries: Vec<String> = fs::read_dir(store.archive_dir())
                .expect("read archive dir")
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect();
            assert!(
                archive_entries
                    .iter()
                    .any(|entry| entry.contains("ch-close") && entry.ends_with(".toml")),
                "archive should contain closed change record"
            );
        });
    }

    #[test]
    fn close_change_does_not_archive_when_branch_delete_fails() {
        let repo = TestRepo::new(false);

        with_repo_cwd(&repo, |root| {
            create_change(&CreateArgs {
                title: "ASANA-4b close branch delete fails".to_string(),
                from: "main".to_string(),
                id: Some("ch-close-fail".to_string()),
                workspace: Some("close-fail-ws".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            })
            .expect("create change");

            let store = store::ChangesStore::open(root);
            let mut record = store
                .read_active_record("ch-close-fail")
                .expect("read active")
                .expect("record present");
            record.pr = Some(store::ChangePr {
                number: 78,
                url: "https://example.test/pr/78".to_string(),
                state: "merged".to_string(),
                draft: false,
            });
            record.git.change_branch = "main".to_string();
            store
                .with_lock("tests write merged state", |locked| {
                    locked.write_active_record(&record)
                })
                .expect("write merged pr metadata");

            let err = close_change(&CloseArgs {
                change_id: "ch-close-fail".to_string(),
                delete_branch: true,
                remote: false,
                force: true,
                format: None,
                json: false,
            })
            .expect_err("branch delete should fail for trunk branch");
            assert!(
                err.to_string()
                    .contains("Refusing to delete configured trunk branch"),
                "unexpected error: {err}"
            );

            assert!(
                store
                    .read_active_record("ch-close-fail")
                    .expect("read active")
                    .is_some(),
                "change should remain active when close fails"
            );
        });
    }

    #[test]
    fn pr_change_adopts_existing_pr_without_creating_new_one() {
        let repo = TestRepo::new(true);

        with_repo_cwd(&repo, |root| {
            create_change(&CreateArgs {
                title: "ASANA-5 pr".to_string(),
                from: "main".to_string(),
                id: Some("ch-pr".to_string()),
                workspace: Some("pr-ws".to_string()),
                tracker: None,
                tracker_url: None,
                format: None,
                json: false,
            })
            .expect("create change");

            let gh_stub = install_gh_stub(
                root,
                "[{\"number\":42,\"url\":\"https://example.test/pr/42\",\"state\":\"OPEN\",\"isDraft\":true}]\n",
                "[{\"number\":42,\"url\":\"https://example.test/pr/42\",\"state\":\"OPEN\",\"isDraft\":true}]\n",
            );

            pr_change(&PrArgs {
                change_id: "ch-pr".to_string(),
                draft: false,
                ready: false,
                title: None,
                body_file: None,
                base: None,
                format: None,
                json: false,
            })
            .expect("run pr command");

            let store = store::ChangesStore::open(root);
            let record = store
                .read_active_record("ch-pr")
                .expect("read active")
                .expect("record present");
            let pr = record.pr.expect("pr metadata should be stored");
            assert_eq!(pr.number, 42);
            assert_eq!(pr.url, "https://example.test/pr/42");

            let log_content = fs::read_to_string(&gh_stub.log_path).expect("read gh log");
            assert!(
                log_content.lines().any(|line| line.starts_with("pr list")),
                "gh list should be called"
            );
            assert!(
                !log_content
                    .lines()
                    .any(|line| line.starts_with("pr create")),
                "gh create should not be called when existing PR is found"
            );
        });
    }
}
