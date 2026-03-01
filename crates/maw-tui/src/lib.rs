//! maw TUI crate — terminal user interface for workspace visualization.

pub mod app;
pub mod event;
pub mod theme;
pub mod ui;

pub use app::{App, RepoDataSource, WorkspaceEntry};

use std::io;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend};

/// Restore the terminal to its original state.
///
/// Disables raw mode, leaves the alternate screen, disables mouse capture,
/// and shows the cursor. Errors are intentionally ignored so this is safe
/// to call from a panic hook where we cannot propagate errors.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
}

/// Run the TUI application with the given data source.
pub fn run(data_source: Box<dyn RepoDataSource>) -> Result<()> {
    // Install a panic hook that restores the terminal before the default
    // handler runs.  Without this, a panic leaves the user's terminal in
    // raw mode with the alternate screen still active.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let mut app = App::new(data_source)?;
    let result = app.run(&mut terminal);

    // Restore terminal (normal exit path)
    restore_terminal();
    terminal.show_cursor()?;

    result
}
