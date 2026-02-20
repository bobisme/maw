//! AST-aware merge layer using tree-sitter (§6.2).
//!
//! This module implements the AST-aware merge step of the merge pipeline:
//! hash equality → diff3 → shifted-code → **AST merge** → agent resolution.
//!
//! When diff3 reports a conflict, the AST merge layer:
//! 1. Parses base + all variant files into ASTs using tree-sitter
//! 2. Extracts top-level items (functions, structs, classes, etc.)
//! 3. Computes edit scripts: which items changed in each variant
//! 4. Checks for conflicts:
//!    - Same item modified differently → conflict (with `AstNode` regions)
//!    - Independent items → merge cleanly
//! 5. Acyclic: reconstructs the merged file by splicing item content
//! 6. Cyclic/overlapping: emits `ConflictAtoms` with `AstNode` regions
//!
//! # Supported languages
//!
//! Initially supports Rust, Python, and TypeScript. Languages are detected
//! from file extensions and must be enabled via config.
//!
//! # Determinism guarantee
//!
//! For the same set of inputs, AST merge always produces the same result:
//! - Items are matched by (kind, name) pairs
//! - Conflicts are sorted by base byte position
//! - New items are appended in variant order (lexicographic by workspace ID)

use std::collections::BTreeMap;
use std::path::Path;

use tree_sitter::{Language, Parser, Tree};

use crate::model::conflict::{
    AtomEdit, ConflictAtom, ConflictReason, Region, SemanticConflictExplanation,
};
use crate::model::types::WorkspaceId;

// ---------------------------------------------------------------------------
// Language detection and configuration
// ---------------------------------------------------------------------------

/// Languages supported by the AST merge layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AstLanguage {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
}

impl AstLanguage {
    /// Detect language from file extension.
    ///
    /// Returns `None` for unsupported or unrecognized extensions.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    /// Get the tree-sitter `Language` for this language.
    fn tree_sitter_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// Node kinds that represent top-level named items for this language.
    const fn named_item_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "function_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "impl_item",
                "const_item",
                "static_item",
                "type_item",
                "mod_item",
                "macro_definition",
            ],
            Self::Python => &[
                "function_definition",
                "class_definition",
                "decorated_definition",
            ],
            Self::TypeScript | Self::JavaScript => &[
                "function_declaration",
                "class_declaration",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
                "method_definition",
            ],
            Self::Go => &[
                "function_declaration",
                "method_declaration",
                "type_declaration",
            ],
        }
    }

    /// Field name used to extract the identifier from a named item node.
    ///
    /// Returns the tree-sitter field name (e.g., "name" for most items,
    /// "type" for impl blocks).
    fn name_field(self, node_kind: &str) -> &'static str {
        match (self, node_kind) {
            (Self::Rust, "impl_item") => "type",
            _ => "name",
        }
    }

    #[must_use]
    const fn from_config_language(lang: crate::config::AstConfigLanguage) -> Self {
        use crate::config::AstConfigLanguage;
        match lang {
            AstConfigLanguage::Rust => Self::Rust,
            AstConfigLanguage::Python => Self::Python,
            AstConfigLanguage::TypeScript => Self::TypeScript,
            AstConfigLanguage::JavaScript => Self::JavaScript,
            AstConfigLanguage::Go => Self::Go,
        }
    }

    #[must_use]
    const fn pack_languages(pack: crate::config::AstLanguagePack) -> &'static [Self] {
        use crate::config::AstLanguagePack;
        match pack {
            AstLanguagePack::Core => &[Self::Rust, Self::Python, Self::TypeScript],
            AstLanguagePack::Web => &[Self::TypeScript, Self::JavaScript],
            AstLanguagePack::Backend => &[Self::Rust, Self::Go, Self::Python],
        }
    }
}

impl std::fmt::Display for AstLanguage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rust => write!(f, "rust"),
            Self::Python => write!(f, "python"),
            Self::TypeScript => write!(f, "typescript"),
            Self::JavaScript => write!(f, "javascript"),
            Self::Go => write!(f, "go"),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for AST-aware merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstMergeConfig {
    /// Languages for which AST merge is enabled.
    /// Empty = disabled for all languages.
    pub enabled_languages: Vec<AstLanguage>,
    /// Maximum semantic false-positive budget in percent.
    pub semantic_false_positive_budget_pct: u8,
    /// Minimum confidence required for semantic-specific conflict reasons.
    pub semantic_min_confidence: u8,
}

impl Default for AstMergeConfig {
    fn default() -> Self {
        Self {
            enabled_languages: Vec::new(),
            semantic_false_positive_budget_pct: 5,
            semantic_min_confidence: 70,
        }
    }
}

impl AstMergeConfig {
    /// Check if AST merge is enabled for a given file path.
    #[must_use]
    pub fn is_enabled_for(&self, path: &Path) -> Option<AstLanguage> {
        let lang = AstLanguage::from_path(path)?;
        if self.enabled_languages.contains(&lang) {
            Some(lang)
        } else {
            None
        }
    }

    /// Create a config with all supported languages enabled.
    #[must_use]
    pub fn all_languages() -> Self {
        Self {
            enabled_languages: vec![
                AstLanguage::Rust,
                AstLanguage::Python,
                AstLanguage::TypeScript,
                AstLanguage::JavaScript,
                AstLanguage::Go,
            ],
            semantic_false_positive_budget_pct: 5,
            semantic_min_confidence: 70,
        }
    }

    /// Create from the TOML config representation.
    #[must_use]
    pub fn from_config(config: &crate::config::AstConfig) -> Self {
        let mut enabled_languages: Vec<AstLanguage> = config
            .languages
            .iter()
            .copied()
            .map(AstLanguage::from_config_language)
            .collect();

        for pack in config.packs.iter().copied() {
            enabled_languages.extend_from_slice(AstLanguage::pack_languages(pack));
        }

        enabled_languages.sort();
        enabled_languages.dedup();

        Self {
            enabled_languages,
            semantic_false_positive_budget_pct: config.semantic_false_positive_budget_pct,
            semantic_min_confidence: config.semantic_min_confidence,
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level item extraction
// ---------------------------------------------------------------------------

/// A top-level item extracted from a parsed AST.
///
/// Items are identified by their (kind, name) pair. Items without a name
/// (e.g., anonymous impl blocks with complex types) use a position-based
/// identity fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopLevelItem {
    /// Tree-sitter node kind (e.g., "`function_item`", "`class_definition`").
    pub kind: String,
    /// Item name if extractable (function name, struct name, etc.).
    pub name: Option<String>,
    /// Byte range in the source: [`start_byte`, `end_byte`).
    pub start_byte: usize,
    pub end_byte: usize,
    /// The item's source content (the bytes within [`start_byte`, `end_byte`)).
    pub content: Vec<u8>,
}

impl TopLevelItem {
    /// A stable identity key for matching items across versions.
    ///
    /// Named items use (kind, name). Unnamed items use (kind, position-index).
    fn identity_key(&self, index: usize) -> ItemKey {
        self.name.as_ref().map_or_else(
            || ItemKey::Positional {
                kind: self.kind.clone(),
                index,
            },
            |name| ItemKey::Named {
                kind: self.kind.clone(),
                name: name.clone(),
            },
        )
    }
}

/// Identity key for matching items across base and variant ASTs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ItemKey {
    Named { kind: String, name: String },
    Positional { kind: String, index: usize },
}

impl std::fmt::Display for ItemKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named { kind, name } => write!(f, "{kind} `{name}`"),
            Self::Positional { kind, index } => write!(f, "{kind} #{index}"),
        }
    }
}

/// Parse a source file and extract top-level items.
fn parse_and_extract(
    source: &[u8],
    lang: AstLanguage,
) -> Result<(Tree, Vec<TopLevelItem>), AstMergeError> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .map_err(|e| AstMergeError::ParserSetup(format!("{e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or(AstMergeError::ParseFailed)?;

    let root = tree.root_node();
    let named_kinds = lang.named_item_kinds();
    let mut items = Vec::new();

    let child_count = root.child_count();
    for i in 0..child_count {
        let Some(child) = root.child(i) else { continue };
        let kind = child.kind();

        if !named_kinds.contains(&kind) {
            continue;
        }

        // Extract the item name from the appropriate field.
        let name_field = lang.name_field(kind);
        let name = child.child_by_field_name(name_field).map(|n| {
            std::str::from_utf8(&source[n.start_byte()..n.end_byte()])
                .unwrap_or("")
                .to_owned()
        });

        let start = child.start_byte();
        let end = child.end_byte();

        items.push(TopLevelItem {
            kind: kind.to_owned(),
            name,
            start_byte: start,
            end_byte: end,
            content: source[start..end].to_vec(),
        });
    }

    Ok((tree, items))
}

// ---------------------------------------------------------------------------
// Edit script computation
// ---------------------------------------------------------------------------

/// A change to a top-level item between base and a variant.
#[derive(Clone, Debug)]
enum ItemChange {
    /// Item was modified (content differs).
    Modified {
        key: ItemKey,
        base_item: TopLevelItem,
        variant_item: TopLevelItem,
    },
    /// Item was added in the variant (not present in base).
    Added {
        key: ItemKey,
        variant_item: TopLevelItem,
    },
    /// Item was deleted in the variant (present in base, absent in variant).
    Deleted {
        key: ItemKey,
        base_item: TopLevelItem,
    },
}

/// Compute the edit script between base items and variant items.
fn compute_edit_script(
    base_items: &[TopLevelItem],
    variant_items: &[TopLevelItem],
) -> Vec<ItemChange> {
    // Build keyed maps for lookup.
    let base_map: BTreeMap<ItemKey, &TopLevelItem> = base_items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.identity_key(i), item))
        .collect();

    let variant_map: BTreeMap<ItemKey, &TopLevelItem> = variant_items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.identity_key(i), item))
        .collect();

    let mut changes = Vec::new();

    // Check for modified and deleted items.
    for (key, base_item) in &base_map {
        match variant_map.get(key) {
            Some(variant_item) => {
                if base_item.content != variant_item.content {
                    changes.push(ItemChange::Modified {
                        key: key.clone(),
                        base_item: (*base_item).clone(),
                        variant_item: (*variant_item).clone(),
                    });
                }
            }
            None => {
                changes.push(ItemChange::Deleted {
                    key: key.clone(),
                    base_item: (*base_item).clone(),
                });
            }
        }
    }

    // Check for added items.
    for (key, variant_item) in &variant_map {
        if !base_map.contains_key(key) {
            changes.push(ItemChange::Added {
                key: key.clone(),
                variant_item: (*variant_item).clone(),
            });
        }
    }

    changes
}

// ---------------------------------------------------------------------------
// Constraint-based merge
// ---------------------------------------------------------------------------

/// The result of an AST merge attempt.
#[derive(Clone, Debug)]
pub enum AstMergeResult {
    /// All changes were in independent AST nodes — merged cleanly.
    Clean(Vec<u8>),
    /// Conflicts were found at the AST level.
    Conflict { atoms: Vec<ConflictAtom> },
    /// The file could not be parsed or AST merge is not applicable.
    Unsupported,
}

/// Errors from AST merge operations.
#[derive(Debug)]
pub enum AstMergeError {
    /// Failed to set up the tree-sitter parser.
    ParserSetup(String),
    /// tree-sitter failed to parse the file.
    ParseFailed,
}

impl std::fmt::Display for AstMergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParserSetup(msg) => write!(f, "parser setup failed: {msg}"),
            Self::ParseFailed => write!(f, "tree-sitter failed to parse file"),
        }
    }
}

impl std::error::Error for AstMergeError {}

/// A constraint on a single item from one variant.
#[derive(Clone, Debug)]
struct ItemConstraint {
    workspace_id: WorkspaceId,
    change: ItemChange,
}

/// Attempt AST-aware merge on a set of variants.
///
/// This is the main entry point. It:
/// 1. Parses base + all variants
/// 2. Computes per-variant edit scripts
/// 3. Checks for conflicting edits on the same item
/// 4. If clean: reconstructs the merged file
/// 5. If conflicts: returns structured `ConflictAtoms` with `AstNode` regions
///
/// Returns `AstMergeResult::Unsupported` if files can't be parsed.
#[must_use]
pub fn try_ast_merge(
    base: &[u8],
    variants: &[(WorkspaceId, Vec<u8>)],
    lang: AstLanguage,
) -> AstMergeResult {
    try_ast_merge_with_config(base, variants, lang, &AstMergeConfig::default())
}

/// Attempt AST-aware merge with semantic conflict tuning from config.
#[must_use]
pub fn try_ast_merge_with_config(
    base: &[u8],
    variants: &[(WorkspaceId, Vec<u8>)],
    lang: AstLanguage,
    config: &AstMergeConfig,
) -> AstMergeResult {
    // Parse base.
    let Ok((_base_tree, base_items)) = parse_and_extract(base, lang) else {
        return AstMergeResult::Unsupported;
    };

    // If no top-level items were found, AST merge can't help.
    if base_items.is_empty() {
        return AstMergeResult::Unsupported;
    }

    // Parse variants and compute edit scripts.
    let mut all_constraints: Vec<ItemConstraint> = Vec::new();

    for (ws_id, variant_content) in variants {
        let Ok((_variant_tree, variant_items)) = parse_and_extract(variant_content, lang) else {
            return AstMergeResult::Unsupported;
        };

        let edit_script = compute_edit_script(&base_items, &variant_items);
        for change in edit_script {
            all_constraints.push(ItemConstraint {
                workspace_id: ws_id.clone(),
                change,
            });
        }
    }

    // If no constraints (no changes at AST level), all variants are identical
    // at the item level. This shouldn't normally happen since diff3 already
    // handled identical content, but handle gracefully.
    if all_constraints.is_empty() {
        return AstMergeResult::Clean(base.to_vec());
    }

    // Group constraints by item key.
    let mut constraints_by_item: BTreeMap<ItemKey, Vec<&ItemConstraint>> = BTreeMap::new();
    for constraint in &all_constraints {
        let key = match &constraint.change {
            ItemChange::Modified { key, .. }
            | ItemChange::Added { key, .. }
            | ItemChange::Deleted { key, .. } => key.clone(),
        };
        constraints_by_item.entry(key).or_default().push(constraint);
    }

    // Check for conflicts: same item modified by multiple workspaces differently.
    let mut conflict_atoms: Vec<ConflictAtom> = Vec::new();
    let mut resolutions: BTreeMap<ItemKey, &ItemConstraint> = BTreeMap::new();

    for (key, constraints) in &constraints_by_item {
        if constraints.len() == 1 {
            // Single workspace changed this item — no conflict.
            resolutions.insert(key.clone(), constraints[0]);
            continue;
        }

        // Multiple workspaces touched the same item.
        // Check if they all made the same change.
        if all_same_change(constraints) {
            resolutions.insert(key.clone(), constraints[0]);
            continue;
        }

        // Check for modify/delete conflicts.
        let has_delete = constraints
            .iter()
            .any(|c| matches!(&c.change, ItemChange::Deleted { .. }));
        let has_modify = constraints
            .iter()
            .any(|c| matches!(&c.change, ItemChange::Modified { .. }));

        if has_delete && has_modify {
            // Modify/delete at AST level — conflict.
            let atom = build_modify_delete_atom(key, constraints, lang, config);
            conflict_atoms.push(atom);
            continue;
        }

        // Multiple different modifications to the same item — conflict.
        let atom = build_conflict_atom(key, constraints, lang, config);
        conflict_atoms.push(atom);
    }

    if !conflict_atoms.is_empty() {
        // Sort atoms by base byte position for determinism.
        conflict_atoms.sort_by_key(|a| match &a.base_region {
            Region::AstNode { start_byte, .. } => *start_byte,
            Region::Lines { start, .. } => *start,
            Region::WholeFile => 0,
        });
        return AstMergeResult::Conflict {
            atoms: conflict_atoms,
        };
    }

    // All changes are to disjoint items — reconstruct the merged file.
    AstMergeResult::Clean(reconstruct_merged_file(
        base,
        &base_items,
        &resolutions,
        variants,
    ))
}

/// Check if all constraints make the same effective change.
fn all_same_change(constraints: &[&ItemConstraint]) -> bool {
    if constraints.len() <= 1 {
        return true;
    }

    let first_content = change_content(&constraints[0].change);
    constraints[1..]
        .iter()
        .all(|c| change_content(&c.change) == first_content)
}

/// Extract the effective content from a change for comparison.
fn change_content(change: &ItemChange) -> Option<&[u8]> {
    match change {
        ItemChange::Modified { variant_item, .. } | ItemChange::Added { variant_item, .. } => {
            Some(&variant_item.content)
        }
        ItemChange::Deleted { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Conflict atom construction
// ---------------------------------------------------------------------------

fn build_conflict_atom(
    key: &ItemKey,
    constraints: &[&ItemConstraint],
    lang: AstLanguage,
    config: &AstMergeConfig,
) -> ConflictAtom {
    let base_region = conflict_base_region(constraints);
    let edits = conflict_edits(constraints);

    let (reason, semantic) = classify_semantic_conflict(key, constraints, lang, config, false);
    ConflictAtom::new(base_region, edits, reason).with_semantic(semantic)
}

fn build_modify_delete_atom(
    key: &ItemKey,
    constraints: &[&ItemConstraint],
    lang: AstLanguage,
    config: &AstMergeConfig,
) -> ConflictAtom {
    let base_region = conflict_base_region(constraints);
    let edits = conflict_edits(constraints);
    let (reason, semantic) = classify_semantic_conflict(key, constraints, lang, config, true);
    ConflictAtom::new(base_region, edits, reason).with_semantic(semantic)
}

fn narrow_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn conflict_base_region(constraints: &[&ItemConstraint]) -> Region {
    constraints
        .iter()
        .map(|c| match &c.change {
            ItemChange::Modified { base_item, .. } | ItemChange::Deleted { base_item, .. } => {
                Region::ast_node(
                    &base_item.kind,
                    base_item.name.clone(),
                    narrow_u32(base_item.start_byte),
                    narrow_u32(base_item.end_byte),
                )
            }
            ItemChange::Added { variant_item, .. } => Region::ast_node(
                &variant_item.kind,
                variant_item.name.clone(),
                narrow_u32(variant_item.start_byte),
                narrow_u32(variant_item.end_byte),
            ),
        })
        .next()
        .unwrap_or(Region::whole_file())
}

fn conflict_edits(constraints: &[&ItemConstraint]) -> Vec<AtomEdit> {
    constraints
        .iter()
        .map(|c| {
            let (region, content) = match &c.change {
                ItemChange::Modified { variant_item, .. }
                | ItemChange::Added { variant_item, .. } => (
                    Region::ast_node(
                        &variant_item.kind,
                        variant_item.name.clone(),
                        narrow_u32(variant_item.start_byte),
                        narrow_u32(variant_item.end_byte),
                    ),
                    String::from_utf8_lossy(&variant_item.content).to_string(),
                ),
                ItemChange::Deleted { base_item, .. } => (
                    Region::ast_node(
                        &base_item.kind,
                        base_item.name.clone(),
                        narrow_u32(base_item.start_byte),
                        narrow_u32(base_item.end_byte),
                    ),
                    String::new(),
                ),
            };
            AtomEdit::new(c.workspace_id.to_string(), region, content)
        })
        .collect()
}

fn classify_semantic_conflict(
    key: &ItemKey,
    constraints: &[&ItemConstraint],
    lang: AstLanguage,
    config: &AstMergeConfig,
    force_symbol_lifecycle: bool,
) -> (ConflictReason, SemanticConflictExplanation) {
    let workspaces = constraints
        .iter()
        .map(|c| c.workspace_id.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let has_delete = constraints
        .iter()
        .any(|c| matches!(c.change, ItemChange::Deleted { .. }));
    let has_add = constraints
        .iter()
        .any(|c| matches!(c.change, ItemChange::Added { .. }));

    let has_modified = constraints
        .iter()
        .any(|c| matches!(c.change, ItemChange::Modified { .. }));

    let signatures = constraints
        .iter()
        .filter_map(|c| match &c.change {
            ItemChange::Modified { variant_item, .. } | ItemChange::Added { variant_item, .. } => {
                extract_signature(variant_item, lang)
            }
            ItemChange::Deleted { .. } => None,
        })
        .collect::<std::collections::BTreeSet<_>>();

    let (rule, confidence, raw_reason) = if force_symbol_lifecycle || (has_delete && has_modified) {
        (
            "symbol_lifecycle",
            92,
            ConflictReason::symbol_lifecycle(format!(
                "{key}: symbol lifecycle diverged across [{workspaces}] (modify/delete mismatch)"
            )),
        )
    } else if signatures.len() > 1 {
        (
            "signature_drift",
            86,
            ConflictReason::signature_drift(format!(
                "{key}: signature drift detected across [{workspaces}]"
            )),
        )
    } else if has_add || has_modified {
        (
            "incompatible_api_edits",
            74,
            ConflictReason::incompatible_api_edits(format!(
                "{key}: incompatible API-level edits across [{workspaces}]"
            )),
        )
    } else {
        (
            "same_ast_node_modified",
            65,
            ConflictReason::same_ast_node(format!(
                "{key} modified by {} workspaces: [{workspaces}]",
                constraints.len(),
            )),
        )
    };

    let max_false_positive_budget = config.semantic_false_positive_budget_pct.clamp(0, 40);
    let budget_gate = 100_u8.saturating_sub(max_false_positive_budget * 2);
    let effective_min_confidence = config.semantic_min_confidence.max(budget_gate);

    let (reason, rationale) = if confidence >= effective_min_confidence {
        (
            raw_reason,
            format!("semantic rule `{rule}` passed confidence gate"),
        )
    } else {
        (
            ConflictReason::same_ast_node(format!(
                "{key}: semantic classifier below threshold ({confidence} < {effective_min_confidence})"
            )),
            format!("downgraded `{rule}` due to confidence gate"),
        )
    };

    (
        reason,
        SemanticConflictExplanation::new(
            rule,
            confidence,
            rationale,
            vec![
                format!("workspaces={workspaces}"),
                format!("signature_variants={}", signatures.len()),
                format!("effective_min_confidence={effective_min_confidence}"),
            ],
        ),
    )
}

fn extract_signature(item: &TopLevelItem, lang: AstLanguage) -> Option<String> {
    let text = String::from_utf8_lossy(&item.content);
    let first_line = text.lines().next()?.trim();
    let signature = match lang {
        AstLanguage::Rust => first_line
            .strip_prefix("pub ")
            .or(Some(first_line))
            .unwrap_or(first_line)
            .split('{')
            .next()
            .unwrap_or(first_line)
            .trim()
            .to_string(),
        AstLanguage::Python => first_line
            .strip_suffix(':')
            .unwrap_or(first_line)
            .to_string(),
        AstLanguage::TypeScript | AstLanguage::JavaScript | AstLanguage::Go => first_line
            .split('{')
            .next()
            .unwrap_or(first_line)
            .trim()
            .to_string(),
    };

    if signature.is_empty() {
        None
    } else {
        Some(signature)
    }
}

// ---------------------------------------------------------------------------
// File reconstruction
// ---------------------------------------------------------------------------

/// Reconstruct the merged file from base + resolved item changes.
///
/// Strategy:
/// 1. Walk through the base file in byte order
/// 2. For each base item, check if it was changed:
///    - Modified → substitute the variant's content
///    - Deleted → skip the item (preserve inter-item gaps)
///    - Unchanged → keep base content
/// 3. After all base items, append any added items
fn reconstruct_merged_file(
    base: &[u8],
    base_items: &[TopLevelItem],
    resolutions: &BTreeMap<ItemKey, &ItemConstraint>,
    variants: &[(WorkspaceId, Vec<u8>)],
) -> Vec<u8> {
    let mut result = Vec::with_capacity(base.len());
    let mut cursor = 0_usize;

    // Process base items in order (they're already sorted by position
    // since we extracted them in tree order).
    for (i, base_item) in base_items.iter().enumerate() {
        let key = base_item.identity_key(i);

        // Copy inter-item gap (whitespace, comments, imports, etc.)
        // from cursor to start of this item.
        if base_item.start_byte > cursor {
            result.extend_from_slice(&base[cursor..base_item.start_byte]);
        }

        match resolutions.get(&key) {
            Some(constraint) => match &constraint.change {
                ItemChange::Modified { variant_item, .. } => {
                    result.extend_from_slice(&variant_item.content);
                }
                ItemChange::Deleted { .. } => {
                    // Skip this item entirely. Also skip trailing whitespace
                    // up to the next non-whitespace or next item.
                    let skip_end = skip_trailing_whitespace(base, base_item.end_byte);
                    cursor = skip_end;
                    continue;
                }
                ItemChange::Added { .. } => {
                    // Shouldn't happen for a base item resolution, but handle gracefully.
                    result.extend_from_slice(&base_item.content);
                }
            },
            None => {
                // Item unchanged — keep base content.
                result.extend_from_slice(&base_item.content);
            }
        }

        cursor = base_item.end_byte;
    }

    // Copy any trailing content after the last item.
    if cursor < base.len() {
        result.extend_from_slice(&base[cursor..]);
    }

    // Append items added by variants (not present in base).
    let mut added_items: Vec<(&WorkspaceId, &TopLevelItem)> = Vec::new();
    for constraint in resolutions.values() {
        if let ItemChange::Added { variant_item, .. } = &constraint.change {
            added_items.push((&constraint.workspace_id, variant_item));
        }
    }

    // Also collect added items from variants directly for items that aren't
    // in the resolutions (shouldn't happen, but defensive).
    // Sort by workspace ID then position for determinism.
    added_items.sort_by(|a, b| {
        a.0.cmp(b.0)
            .then_with(|| a.1.start_byte.cmp(&b.1.start_byte))
    });

    for (_ws_id, item) in &added_items {
        // Ensure there's a newline before the added item.
        if !result.is_empty() && !result.ends_with(b"\n") {
            result.push(b'\n');
        }
        if !result.is_empty() && !result.ends_with(b"\n\n") {
            result.push(b'\n');
        }
        result.extend_from_slice(&item.content);
        result.push(b'\n');
    }

    // Find added items that were resolved from each variant
    // and also ensure no items from variants that we should include were missed.
    // (The above loop handles all added items from resolutions.)
    let _ = variants; // Used indirectly through resolutions

    result
}

/// Skip trailing whitespace (spaces, tabs, newlines) after a byte position.
fn skip_trailing_whitespace(source: &[u8], from: usize) -> usize {
    let mut pos = from;
    while pos < source.len() && matches!(source[pos], b' ' | b'\t' | b'\n' | b'\r') {
        pos += 1;
    }
    pos
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::WorkspaceId;

    fn ws(name: &str) -> WorkspaceId {
        WorkspaceId::new(name).unwrap()
    }

    // -----------------------------------------------------------------------
    // Language detection
    // -----------------------------------------------------------------------

    #[test]
    fn detect_rust_from_extension() {
        assert_eq!(
            AstLanguage::from_path(Path::new("src/main.rs")),
            Some(AstLanguage::Rust)
        );
    }

    #[test]
    fn detect_python_from_extension() {
        assert_eq!(
            AstLanguage::from_path(Path::new("app/views.py")),
            Some(AstLanguage::Python)
        );
    }

    #[test]
    fn detect_typescript_from_extension() {
        assert_eq!(
            AstLanguage::from_path(Path::new("src/index.ts")),
            Some(AstLanguage::TypeScript)
        );
        assert_eq!(
            AstLanguage::from_path(Path::new("src/App.tsx")),
            Some(AstLanguage::TypeScript)
        );
    }

    #[test]
    fn detect_javascript_and_go_from_extension() {
        assert_eq!(
            AstLanguage::from_path(Path::new("web/app.js")),
            Some(AstLanguage::JavaScript)
        );
        assert_eq!(
            AstLanguage::from_path(Path::new("cmd/main.go")),
            Some(AstLanguage::Go)
        );
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert_eq!(AstLanguage::from_path(Path::new("data.json")), None);
        assert_eq!(AstLanguage::from_path(Path::new("README.md")), None);
        assert_eq!(AstLanguage::from_path(Path::new("Makefile")), None);
    }

    // -----------------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_rust_file_extracts_items() {
        let source = br#"
fn hello() {
    println!("hello");
}

struct Point {
    x: f64,
    y: f64,
}

fn goodbye() {
    println!("goodbye");
}
"#;
        let (_tree, items) = parse_and_extract(source, AstLanguage::Rust).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, "function_item");
        assert_eq!(items[0].name.as_deref(), Some("hello"));
        assert_eq!(items[1].kind, "struct_item");
        assert_eq!(items[1].name.as_deref(), Some("Point"));
        assert_eq!(items[2].kind, "function_item");
        assert_eq!(items[2].name.as_deref(), Some("goodbye"));
    }

    #[test]
    fn parse_python_file_extracts_items() {
        let source = br#"
def hello():
    print("hello")

class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

def goodbye():
    print("goodbye")
"#;
        let (_tree, items) = parse_and_extract(source, AstLanguage::Python).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, "function_definition");
        assert_eq!(items[0].name.as_deref(), Some("hello"));
        assert_eq!(items[1].kind, "class_definition");
        assert_eq!(items[1].name.as_deref(), Some("Point"));
        assert_eq!(items[2].kind, "function_definition");
        assert_eq!(items[2].name.as_deref(), Some("goodbye"));
    }

    #[test]
    fn parse_typescript_file_extracts_items() {
        let source = br#"
function hello(): void {
    console.log("hello");
}

class Point {
    constructor(public x: number, public y: number) {}
}

function goodbye(): void {
    console.log("goodbye");
}
"#;
        let (_tree, items) = parse_and_extract(source, AstLanguage::TypeScript).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, "function_declaration");
        assert_eq!(items[0].name.as_deref(), Some("hello"));
        assert_eq!(items[1].kind, "class_declaration");
        assert_eq!(items[1].name.as_deref(), Some("Point"));
        assert_eq!(items[2].kind, "function_declaration");
        assert_eq!(items[2].name.as_deref(), Some("goodbye"));
    }

    #[test]
    fn parse_javascript_file_extracts_items() {
        let source = br"
function hello() {
    return 1;
}

class Point {
    value() { return 1; }
}
";
        let (_tree, items) = parse_and_extract(source, AstLanguage::JavaScript).unwrap();
        assert!(!items.is_empty());
        assert_eq!(items[0].kind, "function_declaration");
        assert_eq!(items[0].name.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_go_file_extracts_items() {
        let source = br"
package main

func hello() int {
    return 1
}

type Point struct {
    x int
}
";
        let (_tree, items) = parse_and_extract(source, AstLanguage::Go).unwrap();
        assert!(!items.is_empty());
        assert_eq!(items[0].kind, "function_declaration");
        assert_eq!(items[0].name.as_deref(), Some("hello"));
    }

    // -----------------------------------------------------------------------
    // Edit script computation
    // -----------------------------------------------------------------------

    #[test]
    fn edit_script_detects_modification() {
        let base = b"fn foo() { 1 }\nfn bar() { 2 }\n";
        let variant = b"fn foo() { 42 }\nfn bar() { 2 }\n";

        let (_, base_items) = parse_and_extract(base, AstLanguage::Rust).unwrap();
        let (_, variant_items) = parse_and_extract(variant, AstLanguage::Rust).unwrap();
        let changes = compute_edit_script(&base_items, &variant_items);

        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], ItemChange::Modified { key, .. }
            if matches!(key, ItemKey::Named { name, .. } if name == "foo")));
    }

    #[test]
    fn edit_script_detects_addition() {
        let base = b"fn foo() { 1 }\n";
        let variant = b"fn foo() { 1 }\nfn bar() { 2 }\n";

        let (_, base_items) = parse_and_extract(base, AstLanguage::Rust).unwrap();
        let (_, variant_items) = parse_and_extract(variant, AstLanguage::Rust).unwrap();
        let changes = compute_edit_script(&base_items, &variant_items);

        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], ItemChange::Added { key, .. }
            if matches!(key, ItemKey::Named { name, .. } if name == "bar")));
    }

    #[test]
    fn edit_script_detects_deletion() {
        let base = b"fn foo() { 1 }\nfn bar() { 2 }\n";
        let variant = b"fn foo() { 1 }\n";

        let (_, base_items) = parse_and_extract(base, AstLanguage::Rust).unwrap();
        let (_, variant_items) = parse_and_extract(variant, AstLanguage::Rust).unwrap();
        let changes = compute_edit_script(&base_items, &variant_items);

        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], ItemChange::Deleted { key, .. }
            if matches!(key, ItemKey::Named { name, .. } if name == "bar")));
    }

    // -----------------------------------------------------------------------
    // AST merge: clean merge (different functions modified)
    // -----------------------------------------------------------------------

    #[test]
    fn different_functions_merge_cleanly_rust() {
        let base = b"fn foo() {\n    1\n}\n\nfn bar() {\n    2\n}\n";
        let variant_a = b"fn foo() {\n    42\n}\n\nfn bar() {\n    2\n}\n";
        let variant_b = b"fn foo() {\n    1\n}\n\nfn bar() {\n    99\n}\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::Rust);
        match result {
            AstMergeResult::Clean(merged) => {
                let merged_str = std::str::from_utf8(&merged).unwrap();
                assert!(merged_str.contains("42"), "should have ws-a's foo change");
                assert!(merged_str.contains("99"), "should have ws-b's bar change");
                assert!(
                    !merged_str.contains("    1\n"),
                    "should not have base foo body"
                );
                assert!(
                    !merged_str.contains("    2\n"),
                    "should not have base bar body"
                );
            }
            other => panic!("expected clean merge, got: {other:?}"),
        }
    }

    #[test]
    fn different_functions_merge_cleanly_python() {
        let base = b"def foo():\n    return 1\n\ndef bar():\n    return 2\n";
        let variant_a = b"def foo():\n    return 42\n\ndef bar():\n    return 2\n";
        let variant_b = b"def foo():\n    return 1\n\ndef bar():\n    return 99\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::Python);
        match result {
            AstMergeResult::Clean(merged) => {
                let merged_str = std::str::from_utf8(&merged).unwrap();
                assert!(merged_str.contains("42"), "should have ws-a's foo change");
                assert!(merged_str.contains("99"), "should have ws-b's bar change");
            }
            other => panic!("expected clean merge, got: {other:?}"),
        }
    }

    #[test]
    fn different_functions_merge_cleanly_typescript() {
        let base =
            b"function foo(): number {\n    return 1;\n}\n\nfunction bar(): number {\n    return 2;\n}\n";
        let variant_a =
            b"function foo(): number {\n    return 42;\n}\n\nfunction bar(): number {\n    return 2;\n}\n";
        let variant_b =
            b"function foo(): number {\n    return 1;\n}\n\nfunction bar(): number {\n    return 99;\n}\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::TypeScript);
        match result {
            AstMergeResult::Clean(merged) => {
                let merged_str = std::str::from_utf8(&merged).unwrap();
                assert!(merged_str.contains("42"), "should have ws-a's foo change");
                assert!(merged_str.contains("99"), "should have ws-b's bar change");
            }
            other => panic!("expected clean merge, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // AST merge: conflict (same function modified)
    // -----------------------------------------------------------------------

    #[test]
    fn same_function_produces_conflict_rust() {
        let base = b"fn process() {\n    do_work();\n}\n";
        let variant_a = b"fn process() {\n    do_work_v1();\n}\n";
        let variant_b = b"fn process() {\n    do_work_v2();\n}\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::Rust);
        match result {
            AstMergeResult::Conflict { atoms } => {
                assert_eq!(atoms.len(), 1, "expected 1 conflict atom");
                let atom = &atoms[0];

                // Should be an AstNode region.
                assert!(
                    matches!(&atom.base_region, Region::AstNode { node_kind, name, .. }
                        if node_kind == "function_item" && name.as_deref() == Some("process")),
                    "expected AstNode region for function_item `process`, got: {:?}",
                    atom.base_region
                );

                // Should have edits from both workspaces.
                assert_eq!(atom.edits.len(), 2);
                let ws_names: Vec<&str> = atom.edits.iter().map(|e| e.workspace.as_str()).collect();
                assert!(ws_names.contains(&"ws-a"));
                assert!(ws_names.contains(&"ws-b"));

                // Reason should be SameAstNodeModified.
                assert_eq!(atom.reason.variant_name(), "same_ast_node_modified");
            }
            other => panic!("expected conflict, got: {other:?}"),
        }
    }

    #[test]
    fn same_function_produces_conflict_python() {
        let base = b"def process():\n    do_work()\n";
        let variant_a = b"def process():\n    do_work_v1()\n";
        let variant_b = b"def process():\n    do_work_v2()\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::Python);
        assert!(
            matches!(result, AstMergeResult::Conflict { ref atoms } if atoms.len() == 1),
            "expected 1 conflict atom, got: {result:?}"
        );
    }

    #[test]
    fn signature_drift_is_reported_with_semantic_metadata() {
        let base = b"fn process(input: i32) -> i32 {\n    input\n}\n";
        let variant_a = b"fn process(input: i32, flag: bool) -> i32 {\n    if flag { input + 1 } else { input }\n}\n";
        let variant_b = b"fn process(input: i32) -> String {\n    input.to_string()\n}\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];
        let config = AstMergeConfig {
            enabled_languages: vec![AstLanguage::Rust],
            semantic_false_positive_budget_pct: 15,
            semantic_min_confidence: 70,
        };

        let result = try_ast_merge_with_config(base, &variants, AstLanguage::Rust, &config);
        match result {
            AstMergeResult::Conflict { atoms } => {
                assert_eq!(atoms.len(), 1);
                assert_eq!(atoms[0].reason.variant_name(), "signature_drift");
                let semantic = atoms[0].semantic.as_ref().expect("semantic metadata");
                assert_eq!(semantic.rule, "signature_drift");
                assert!(semantic.confidence >= 80);
            }
            other => panic!("expected conflict, got: {other:?}"),
        }
    }

    #[test]
    fn strict_budget_downgrades_low_confidence_semantic_rule() {
        let base = b"fn process() {\n    old();\n}\n";
        let variant_a = b"fn process() {\n    new_a();\n}\n";
        let variant_b = b"fn process() {\n    new_b();\n}\n";

        let variants = vec![
            (ws("ws-a"), variant_a.to_vec()),
            (ws("ws-b"), variant_b.to_vec()),
        ];
        let config = AstMergeConfig {
            enabled_languages: vec![AstLanguage::Rust],
            semantic_false_positive_budget_pct: 2,
            semantic_min_confidence: 90,
        };

        let result = try_ast_merge_with_config(base, &variants, AstLanguage::Rust, &config);
        match result {
            AstMergeResult::Conflict { atoms } => {
                assert_eq!(atoms.len(), 1);
                assert_eq!(atoms[0].reason.variant_name(), "same_ast_node_modified");
                assert!(atoms[0].semantic.is_some());
            }
            other => panic!("expected conflict, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // AST merge: identical changes resolve cleanly
    // -----------------------------------------------------------------------

    #[test]
    fn identical_changes_to_same_function_merge_cleanly() {
        let base = b"fn process() {\n    old();\n}\n";
        let variant = b"fn process() {\n    new();\n}\n";

        let variants = vec![
            (ws("ws-a"), variant.to_vec()),
            (ws("ws-b"), variant.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::Rust);
        assert!(
            matches!(result, AstMergeResult::Clean(_)),
            "identical changes should merge cleanly, got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // AST merge: K=3 (three workspaces, different functions)
    // -----------------------------------------------------------------------

    #[test]
    fn three_workspaces_different_functions_merge_cleanly() {
        let base = b"fn a() { 1 }\n\nfn b() { 2 }\n\nfn c() { 3 }\n";
        let va = b"fn a() { 10 }\n\nfn b() { 2 }\n\nfn c() { 3 }\n";
        let vb = b"fn a() { 1 }\n\nfn b() { 20 }\n\nfn c() { 3 }\n";
        let vc = b"fn a() { 1 }\n\nfn b() { 2 }\n\nfn c() { 30 }\n";

        let variants = vec![
            (ws("ws-a"), va.to_vec()),
            (ws("ws-b"), vb.to_vec()),
            (ws("ws-c"), vc.to_vec()),
        ];

        let result = try_ast_merge(base, &variants, AstLanguage::Rust);
        match result {
            AstMergeResult::Clean(merged) => {
                let s = std::str::from_utf8(&merged).unwrap();
                assert!(s.contains("10"), "should have ws-a's change");
                assert!(s.contains("20"), "should have ws-b's change");
                assert!(s.contains("30"), "should have ws-c's change");
            }
            other => panic!("expected clean merge, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // AST merge: mixed clean and conflict
    // -----------------------------------------------------------------------

    #[test]
    fn mixed_clean_and_conflict_returns_conflict() {
        // ws-a modifies foo (clean), ws-b modifies bar (clean),
        // but ws-a and ws-b both modify baz (conflict).
        let base = b"fn foo() { 1 }\n\nfn bar() { 2 }\n\nfn baz() { 3 }\n";
        let va = b"fn foo() { 10 }\n\nfn bar() { 2 }\n\nfn baz() { 30 }\n";
        let vb = b"fn foo() { 1 }\n\nfn bar() { 20 }\n\nfn baz() { 99 }\n";

        let variants = vec![(ws("ws-a"), va.to_vec()), (ws("ws-b"), vb.to_vec())];

        let result = try_ast_merge(base, &variants, AstLanguage::Rust);
        match result {
            AstMergeResult::Conflict { atoms } => {
                assert_eq!(atoms.len(), 1, "only baz should conflict");
                assert!(
                    matches!(&atoms[0].base_region, Region::AstNode { name, .. }
                        if name.as_deref() == Some("baz")),
                    "conflict should be on baz, got: {:?}",
                    atoms[0].base_region
                );
            }
            other => panic!("expected conflict, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // AST merge: unsupported for non-parseable content
    // -----------------------------------------------------------------------

    #[test]
    fn unparseable_base_returns_unsupported() {
        // Binary-ish content that tree-sitter can parse (it won't error),
        // but has no top-level items.
        let base = b"\x00\x01\x02\x03";
        let variant = b"\x00\x01\x02\x04";

        let variants = vec![(ws("ws-a"), variant.to_vec())];
        let result = try_ast_merge(base, &variants, AstLanguage::Rust);
        assert!(
            matches!(result, AstMergeResult::Unsupported),
            "no items → unsupported"
        );
    }

    // -----------------------------------------------------------------------
    // Config
    // -----------------------------------------------------------------------

    #[test]
    fn config_enabled_for_rust() {
        let config = AstMergeConfig::all_languages();
        assert_eq!(
            config.is_enabled_for(Path::new("src/main.rs")),
            Some(AstLanguage::Rust)
        );
    }

    #[test]
    fn config_disabled_returns_none() {
        let config = AstMergeConfig::default();
        assert_eq!(config.is_enabled_for(Path::new("src/main.rs")), None);
    }

    #[test]
    fn config_partial_languages() {
        let config = AstMergeConfig {
            enabled_languages: vec![AstLanguage::Rust],
            semantic_false_positive_budget_pct: 5,
            semantic_min_confidence: 70,
        };
        assert_eq!(
            config.is_enabled_for(Path::new("src/main.rs")),
            Some(AstLanguage::Rust)
        );
        assert_eq!(config.is_enabled_for(Path::new("app.py")), None);
    }

    // -----------------------------------------------------------------------
    // Benchmark helper
    // -----------------------------------------------------------------------

    #[test]
    fn benchmark_ast_merge_overhead() {
        use std::time::Instant;

        // Generate a Rust file with many functions.
        let mut base = String::new();
        let mut variant_a = String::new();
        let mut variant_b = String::new();

        for i in 0..50 {
            base.push_str(&format!("fn func_{i}() {{\n    // body {i}\n}}\n\n"));
            if i == 10 {
                variant_a.push_str(&format!("fn func_{i}() {{\n    // modified by a\n}}\n\n"));
            } else {
                variant_a.push_str(&format!("fn func_{i}() {{\n    // body {i}\n}}\n\n"));
            }
            if i == 40 {
                variant_b.push_str(&format!("fn func_{i}() {{\n    // modified by b\n}}\n\n"));
            } else {
                variant_b.push_str(&format!("fn func_{i}() {{\n    // body {i}\n}}\n\n"));
            }
        }

        let variants = vec![
            (ws("ws-a"), variant_a.into_bytes()),
            (ws("ws-b"), variant_b.into_bytes()),
        ];

        let start = Instant::now();
        let iterations = 100;
        for _ in 0..iterations {
            let _ = try_ast_merge(base.as_bytes(), &variants, AstLanguage::Rust);
        }
        let elapsed = start.elapsed();

        let per_merge_us = elapsed.as_micros() / iterations;
        // This is informational — just ensure it completes in reasonable time.
        // AST merge of a 50-function file should be well under 10ms.
        assert!(
            per_merge_us < 10_000,
            "AST merge took {per_merge_us}µs per iteration, expected < 10ms"
        );
        eprintln!(
            "Benchmark: AST merge of 50-function file: {per_merge_us}µs/merge ({iterations} iterations)"
        );
    }
}
