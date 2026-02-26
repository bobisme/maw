use ratatui::style::Color;
use ratatui::widgets::block::BorderType;

// Panel colors
pub const FOCUSED: Color = Color::Green;
pub const SELECTED_BG: Color = Color::DarkGray;

// Border style
pub const BORDER_TYPE: BorderType = BorderType::Rounded;

// Status colors
#[allow(dead_code)]
pub const CURRENT: Color = Color::Green;
pub const STALE: Color = Color::Yellow;
#[allow(dead_code)]
pub const CONFLICT: Color = Color::Red;

// File status colors
pub const FILE_MODIFIED: Color = Color::Yellow;
pub const FILE_ADDED: Color = Color::Green;
pub const FILE_DELETED: Color = Color::Red;
pub const FILE_RENAMED: Color = Color::Cyan;

// Overlap
pub const OVERLAP: Color = Color::Magenta;

// Warnings (sync issues, stray root files)
pub const WARNING: Color = Color::LightRed;

// Dirty indicator
#[allow(dead_code)]
pub const DIRTY: Color = Color::Yellow;

// Priority colors (kept for potential future use)
#[allow(dead_code)]
pub const PRIORITY_HIGH: Color = Color::Red;
#[allow(dead_code)]
pub const PRIORITY_MED: Color = Color::Yellow;
#[allow(dead_code)]
pub const PRIORITY_LOW: Color = Color::Gray;
