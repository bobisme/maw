use ratatui::style::Color;
use ratatui::widgets::block::BorderType;

// Panel colors
pub const FOCUSED: Color = Color::Green;
pub const SELECTED_BG: Color = Color::DarkGray;

// Border style
pub const BORDER_TYPE: BorderType = BorderType::Rounded;

// Status colors
pub const CURRENT: Color = Color::Green;
pub const STALE: Color = Color::Yellow;
pub const CONFLICT: Color = Color::Red;

// Priority colors
pub const PRIORITY_HIGH: Color = Color::Red;
pub const PRIORITY_MED: Color = Color::Yellow;
pub const PRIORITY_LOW: Color = Color::Gray;
