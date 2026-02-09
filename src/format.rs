use anyhow::{bail, Result};
use serde::Serialize;
use std::io::IsTerminal;
use std::str::FromStr;

/// Output format for structured data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum OutputFormat {
    /// Pretty - colored, human-friendly output for terminals
    Pretty,
    /// JSON - machine-parseable
    Json,
    /// Plain text - compact, agent-friendly output
    #[default]
    Text,
}


impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            "toon" => bail!("Invalid format 'toon'. toon has been removed; use: text, json, or pretty"),
            _ => bail!("Invalid format '{s}'. Use: text, json, or pretty"),
        }
    }
}

impl OutputFormat {
    /// Resolve the output format based on explicit value, env var, or TTY detection
    pub fn resolve(explicit: Option<Self>) -> Self {
        // Priority: explicit flag > FORMAT env var > TTY detection
        if let Some(fmt) = explicit {
            return fmt;
        }

        if let Ok(env_format) = std::env::var("FORMAT")
            && let Ok(fmt) = env_format.parse::<Self>() {
                return fmt;
            }

        // TTY detection: pretty if TTY, text if piped
        if std::io::stdout().is_terminal() {
            Self::Pretty
        } else {
            Self::Text
        }
    }

    /// Check if colors should be disabled
    pub fn should_use_color(self) -> bool {
        match self {
            Self::Pretty => {
                // Respect NO_COLOR env var
                std::env::var("NO_COLOR").is_err() && std::io::stdout().is_terminal()
            }
            _ => false,
        }
    }

    /// Serialize data to the requested format
    pub fn serialize<T: Serialize>(self, data: &T) -> Result<String> {
        match self {
            Self::Json => {
                serde_json::to_string_pretty(data).map_err(|e| anyhow::anyhow!("JSON serialization failed: {e}"))
            }
            Self::Text | Self::Pretty => {
                // Text and Pretty formats use custom print functions, not serde serialization
                bail!("{self:?} format should not use serialize()")
            }
        }
    }

}
