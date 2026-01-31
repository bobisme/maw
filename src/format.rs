use anyhow::{bail, Result};
use serde::Serialize;
use std::str::FromStr;

/// Output format for structured data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// TOON (Token-Oriented Object Notation) - compact, LLM-friendly
    Toon,
    /// JSON - machine-parseable
    Json,
    /// Plain text - jj's default output
    Text,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Toon
    }
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "toon" => Ok(Self::Toon),
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            _ => bail!("Invalid format '{}'. Use: toon, json, or text", s),
        }
    }
}

impl OutputFormat {
    /// Serialize data to the requested format
    pub fn serialize<T: Serialize>(self, data: &T) -> Result<String> {
        match self {
            Self::Json => {
                serde_json::to_string_pretty(data).map_err(|e| anyhow::anyhow!("JSON serialization failed: {}", e))
            }
            Self::Toon => {
                // toon-format's encode function expects a serde_json::Value
                let json_value = serde_json::to_value(data)
                    .map_err(|e| anyhow::anyhow!("Failed to convert to JSON value: {}", e))?;

                let options = toon_format::EncodeOptions::default();
                toon_format::encode(&json_value, &options)
                    .map_err(|e| anyhow::anyhow!("TOON encoding failed: {}", e))
            }
            Self::Text => {
                // Text format shouldn't use this path - caller should return raw text
                bail!("Text format should not use serialize()")
            }
        }
    }

}
