use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Built-in workspace template archetypes for common bead classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum WorkspaceTemplate {
    Feature,
    Bugfix,
    Refactor,
    Eval,
    Release,
}

impl std::fmt::Display for WorkspaceTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = match self {
            Self::Feature => "feature",
            Self::Bugfix => "bugfix",
            Self::Refactor => "refactor",
            Self::Eval => "eval",
            Self::Release => "release",
        };
        write!(f, "{v}")
    }
}

/// Effective defaults derived from a selected template.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateDefaults {
    pub merge_policy: String,
    pub default_checks: Vec<String>,
    pub recommended_validation: Vec<String>,
}

/// Full template metadata for machine consumers and docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateProfile {
    pub template: WorkspaceTemplate,
    pub description: String,
    pub defaults: TemplateDefaults,
}

impl WorkspaceTemplate {
    #[must_use]
    pub fn profile(self) -> TemplateProfile {
        match self {
            Self::Feature => TemplateProfile {
                template: self,
                description: "New user-facing behavior with tests and docs".to_string(),
                defaults: TemplateDefaults {
                    merge_policy: "squash-after-checks".to_string(),
                    default_checks: vec!["just check".to_string(), "cargo test".to_string()],
                    recommended_validation: vec![
                        "maw ws merge <name> --check".to_string(),
                        "maw ws overlap <name> default --format json".to_string(),
                    ],
                },
            },
            Self::Bugfix => TemplateProfile {
                template: self,
                description: "Focused defect fix with regression coverage".to_string(),
                defaults: TemplateDefaults {
                    merge_policy: "fast-track-if-clean".to_string(),
                    default_checks: vec![
                        "cargo test -- --nocapture".to_string(),
                        "just check".to_string(),
                    ],
                    recommended_validation: vec![
                        "maw ws merge <name> --check".to_string(),
                        "maw ws touched <name> --format json".to_string(),
                    ],
                },
            },
            Self::Refactor => TemplateProfile {
                template: self,
                description: "Behavior-preserving internal change".to_string(),
                defaults: TemplateDefaults {
                    merge_policy: "require-clean-diff".to_string(),
                    default_checks: vec!["just check".to_string(), "cargo test".to_string()],
                    recommended_validation: vec![
                        "cargo clippy --all-targets".to_string(),
                        "maw ws merge <name> --plan --format json".to_string(),
                    ],
                },
            },
            Self::Eval => TemplateProfile {
                template: self,
                description: "Evaluation/prototyping sandbox with explicit reporting".to_string(),
                defaults: TemplateDefaults {
                    merge_policy: "manual-review-before-merge".to_string(),
                    default_checks: vec!["cargo test".to_string()],
                    recommended_validation: vec![
                        "maw ws touched <name> --format json".to_string(),
                        "maw ws status --format json".to_string(),
                    ],
                },
            },
            Self::Release => TemplateProfile {
                template: self,
                description: "Release prep with changelog/version validation".to_string(),
                defaults: TemplateDefaults {
                    merge_policy: "strict-no-conflicts".to_string(),
                    default_checks: vec![
                        "just check".to_string(),
                        "cargo test --release".to_string(),
                    ],
                    recommended_validation: vec![
                        "maw ws merge <name> --check --format json".to_string(),
                        "maw status".to_string(),
                    ],
                },
            },
        }
    }
}
