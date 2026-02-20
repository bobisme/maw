use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::app::{App, Panel, Popup};
use super::theme;

/// Create a styled block with rounded corners
fn styled_block(title: &str, is_focused: bool) -> Block<'_> {
    Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_type(theme::BORDER_TYPE)
        .border_style(if is_focused {
            Style::default().fg(theme::FOCUSED)
        } else {
            Style::default()
        })
}

pub fn draw(frame: &mut Frame, app: &App) {
    // Top-level layout: main area + status bar
    let outer_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    let main_area = outer_chunks[0];
    let status_area = outer_chunks[1];

    // Main layout: left panels | right panels
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(main_area);

    // Left side: stacked panels
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40), // Workspaces
            Constraint::Percentage(40), // Commits
            Constraint::Percentage(20), // Issues
        ])
        .split(main_chunks[0]);

    // Right side: details + command log
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(main_chunks[1]);

    // Draw panels
    draw_workspaces(frame, app, left_chunks[0]);
    draw_commits(frame, app, left_chunks[1]);
    draw_issues(frame, app, left_chunks[2]);
    draw_details(frame, app, right_chunks[0]);
    draw_command_log(frame, app, right_chunks[1]);

    // Status bar
    draw_status_bar(frame, app, status_area);

    // Popups (rendered last, on top)
    if app.show_help {
        draw_help_popup(frame);
    }

    if let Some(popup) = &app.popup {
        draw_popup(frame, popup);
    }
}

fn draw_workspaces(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focused_panel == Panel::Workspaces;
    let title = "[1]-Workspaces";

    let items: Vec<ListItem> = app
        .workspaces
        .iter()
        .enumerate()
        .map(|(i, ws)| {
            let current_marker = if ws.is_current { "@" } else { " " };
            let stale_marker = if ws.is_stale { " (stale)" } else { "" };
            let selected = i == app.selected_workspace;

            let content = format!(
                "{} {:<15} {:.8} {}{}",
                current_marker,
                ws.name,
                ws.change_id,
                truncate(&ws.description, 20),
                stale_marker
            );

            let style = if selected && is_focused {
                Style::default()
                    .bg(theme::SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else if ws.is_current {
                Style::default().fg(theme::CURRENT)
            } else {
                Style::default()
            };

            ListItem::new(content).style(style)
        })
        .collect();

    let block = styled_block(title, is_focused);
    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_commits(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focused_panel == Panel::Commits;

    let items: Vec<ListItem> = app
        .commits
        .iter()
        .enumerate()
        .map(|(i, commit)| {
            let selected = i == app.selected_commit;

            // Commit symbol: ◆ immutable, ● conflict, ○ normal
            let symbol = if commit.is_conflict {
                "●"
            } else if commit.is_immutable {
                "◆"
            } else {
                "○"
            };

            let working = if commit.is_working_copy { "@" } else { " " };

            let content = format!(
                "{} {}{:.8} {}",
                symbol,
                working,
                commit.change_id,
                truncate(&commit.description, 30),
            );

            let style = if selected && is_focused {
                Style::default()
                    .bg(theme::SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else if commit.is_working_copy {
                Style::default().fg(theme::CURRENT)
            } else if commit.is_immutable {
                Style::default().fg(Color::Blue)
            } else if commit.is_conflict {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };

            ListItem::new(content).style(style)
        })
        .collect();

    let title = format!("[2]-Commits ({})", app.commits.len());
    let block = styled_block(&title, is_focused);
    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_issues(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focused_panel == Panel::Issues;

    // Show different content based on beads availability
    if !app.beads_available {
        let block = styled_block("[3]-Issues", is_focused);

        let text = Paragraph::new("(beads not configured)")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);

        frame.render_widget(text, area);
        return;
    }

    let items: Vec<ListItem> = app
        .issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let selected = i == app.selected_issue;

            // Status symbol: filled for open/in_progress, empty for closed
            let status_sym = if issue.status == "closed" {
                "○"
            } else {
                "●"
            };

            // Priority color
            let priority_color = match issue.priority {
                1 | 2 => theme::PRIORITY_HIGH,
                3 => theme::PRIORITY_MED,
                _ => theme::PRIORITY_LOW,
            };

            let content = format!(
                "{} {} [P{}] {}",
                status_sym,
                issue.id,
                issue.priority,
                truncate(&issue.title, 25),
            );

            let style = if selected && is_focused {
                Style::default()
                    .bg(theme::SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else if issue.status == "in_progress" {
                Style::default().fg(Color::Cyan)
            } else if issue.status == "closed" {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(priority_color)
            };

            ListItem::new(content).style(style)
        })
        .collect();

    let title = if app.issues.is_empty() {
        "[3]-Issues".to_string()
    } else {
        format!("[3]-Issues ({})", app.issues.len())
    };

    let block = styled_block(&title, is_focused);
    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

#[allow(clippy::too_many_lines, clippy::option_if_let_else)]
fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focused_panel == Panel::Details;
    let block = styled_block("[0]-Details", is_focused);

    // Context-sensitive: show details for focused panel's selection
    let content = match app.focused_panel {
        Panel::Commits => {
            if let Some(commit) = app.commits.get(app.selected_commit) {
                let status = if commit.is_conflict {
                    ("conflict", theme::CONFLICT)
                } else if commit.is_immutable {
                    ("immutable", Color::Blue)
                } else {
                    ("working", theme::CURRENT)
                };
                vec![
                    Line::from(vec![
                        Span::styled("Change: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(&commit.change_id),
                    ]),
                    Line::from(vec![
                        Span::styled("Commit: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(&commit.commit_id),
                    ]),
                    Line::from(vec![
                        Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(status.0, Style::default().fg(status.1)),
                    ]),
                    Line::from(""),
                    Line::from(vec![Span::styled(
                        "Description:",
                        Style::default().fg(Color::DarkGray),
                    )]),
                    Line::from(vec![Span::raw(format!("  {}", commit.description))]),
                ]
            } else {
                vec![Line::from("No commit selected")]
            }
        }
        Panel::Issues => {
            if let Some(issue) = app.issues.get(app.selected_issue) {
                let priority_color = match issue.priority {
                    1 | 2 => theme::PRIORITY_HIGH,
                    3 => theme::PRIORITY_MED,
                    _ => theme::PRIORITY_LOW,
                };
                let status_color = match issue.status.as_str() {
                    "closed" => Color::DarkGray,
                    "in_progress" => Color::Cyan,
                    _ => Color::White,
                };
                vec![
                    Line::from(vec![
                        Span::styled("Issue: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(&issue.id),
                    ]),
                    Line::from(vec![
                        Span::styled("Type: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(&issue.kind),
                    ]),
                    Line::from(vec![
                        Span::styled("Priority: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("P{}", issue.priority),
                            Style::default().fg(priority_color),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(&issue.status, Style::default().fg(status_color)),
                    ]),
                    Line::from(""),
                    Line::from(vec![Span::styled(
                        "Title:",
                        Style::default().fg(Color::DarkGray),
                    )]),
                    Line::from(vec![Span::raw(format!("  {}", issue.title))]),
                ]
            } else {
                vec![Line::from("No issue selected")]
            }
        }
        Panel::Workspaces | Panel::Details => {
            // Default: show workspace details with file changes
            let mut lines = Vec::new();

            if let Some(ws) = app.workspaces.get(app.selected_workspace) {
                lines.push(Line::from(vec![
                    Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&ws.name),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Change: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&ws.change_id),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if ws.is_stale { "stale" } else { "active" },
                        Style::default().fg(if ws.is_stale {
                            theme::STALE
                        } else {
                            theme::CURRENT
                        }),
                    ),
                ]));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![Span::styled(
                    "Description:",
                    Style::default().fg(Color::DarkGray),
                )]));
                lines.push(Line::from(vec![Span::raw(format!(
                    "  {}",
                    if ws.description.is_empty() {
                        "(no description)"
                    } else {
                        &ws.description
                    }
                ))]));

                // Show file changes if this is the current workspace
                if ws.is_current && !app.file_changes.is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![Span::styled(
                        format!("Files changed: {}", app.file_changes.len()),
                        Style::default().fg(Color::DarkGray),
                    )]));

                    for change in app.file_changes.iter().take(15) {
                        let (color, label) = match change.status {
                            'M' => (Color::Yellow, "M"),
                            'A' => (Color::Green, "A"),
                            'D' => (Color::Red, "D"),
                            'R' => (Color::Cyan, "R"),
                            _ => (Color::Gray, "?"),
                        };
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(label, Style::default().fg(color)),
                            Span::raw(" "),
                            Span::raw(&change.path),
                        ]));
                    }

                    if app.file_changes.len() > 15 {
                        lines.push(Line::from(vec![Span::styled(
                            format!("  ... and {} more", app.file_changes.len() - 15),
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                }
            } else {
                lines.push(Line::from("No workspace selected"));
            }

            lines
        }
    };

    let text = Paragraph::new(content).block(block);
    frame.render_widget(text, area);
}

fn draw_command_log(frame: &mut Frame, app: &App, area: Rect) {
    let block = styled_block("Command Log", false);

    let items: Vec<ListItem> = app
        .command_log
        .iter()
        .rev()
        .take(10)
        .map(|cmd| ListItem::new(cmd.as_str()).style(Style::default().fg(Color::DarkGray)))
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let blue = Color::Blue;

    // Build key hints based on focused panel
    let mut hints: Vec<(&str, &str)> = match app.focused_panel {
        Panel::Workspaces => vec![
            ("Create", "c"),
            ("Destroy", "d"),
            ("Sync", "s"),
            ("Merge", "m"),
        ],
        Panel::Commits | Panel::Issues | Panel::Details => vec![],
    };

    // Add common hints
    hints.push(("Keybindings", "?"));

    // Format: "Action: key | Action: key | ..."
    let mut spans = Vec::new();
    for (i, (action, key)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(blue)));
        }
        spans.push(Span::styled(
            format!("{action}: "),
            Style::default().fg(blue),
        ));
        spans.push(Span::styled(*key, Style::default().fg(blue)));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

fn draw_help_popup(frame: &mut Frame) {
    let area = frame.area();

    // Center a 50x20 popup
    let popup_width = 50.min(area.width.saturating_sub(4));
    let popup_height = 20.min(area.height.saturating_sub(4));
    let popup_x = (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = (area.height.saturating_sub(popup_height)) / 2;

    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    let help_text = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  j/k, Up/Down   Move selection"),
        Line::from("  g/G            Go to top/bottom"),
        Line::from("  1-3            Focus panel"),
        Line::from("  0              Focus Details"),
        Line::from("  Tab            Cycle panels"),
        Line::from(""),
        Line::from(Span::styled(
            "Workspaces",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  c              Create workspace"),
        Line::from("  d              Destroy workspace"),
        Line::from("  s              Sync (fix stale)"),
        Line::from("  m              Merge selected"),
        Line::from("  M              Merge all"),
        Line::from(""),
        Line::from(Span::styled(
            "General",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  r              Refresh"),
        Line::from("  ?              This help"),
        Line::from("  q              Quit"),
    ];

    let block = Block::default()
        .title("Keybindings")
        .borders(Borders::ALL)
        .border_type(theme::BORDER_TYPE)
        .border_style(Style::default().fg(theme::FOCUSED));

    let paragraph = Paragraph::new(help_text).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn draw_popup(frame: &mut Frame, popup: &Popup) {
    match popup {
        Popup::CreateWorkspace { input, error } => {
            draw_create_workspace_popup(frame, input, error.as_deref());
        }
        Popup::ConfirmDestroy { name } => {
            draw_confirm_destroy_popup(frame, name);
        }
        Popup::Message {
            title,
            message,
            is_error,
        } => {
            draw_message_popup(frame, title, message, *is_error);
        }
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let popup_x = area.x + (area.width.saturating_sub(width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(
        popup_x,
        popup_y,
        width.min(area.width),
        height.min(area.height),
    )
}

fn draw_create_workspace_popup(frame: &mut Frame, input: &str, error: Option<&str>) {
    let area = frame.area();
    let popup_area = centered_rect(45, 9, area);

    frame.render_widget(Clear, popup_area);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Name: "),
            Span::styled(
                format!("{input}_"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];

    if let Some(err) = error {
        lines.push(Line::from(vec![Span::styled(
            format!("  Error: {err}"),
            Style::default().fg(Color::Red),
        )]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            "  Ctrl+W: delete word  Ctrl+U: clear",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Enter: create  Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )]));

    let block = Block::default()
        .title("Create Workspace")
        .borders(Borders::ALL)
        .border_type(theme::BORDER_TYPE)
        .border_style(Style::default().fg(theme::FOCUSED));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn draw_confirm_destroy_popup(frame: &mut Frame, name: &str) {
    let area = frame.area();
    let popup_area = centered_rect(45, 10, area);

    frame.render_widget(Clear, popup_area);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Destroy workspace '"),
            Span::styled(name, Style::default().fg(Color::Yellow)),
            Span::raw("'?"),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  This will:",
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(vec![Span::raw("  - Forget workspace from jj")]),
        Line::from(vec![Span::raw(format!("  - Delete ws/{name}/"))]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  y: confirm  n/Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )]),
    ];

    let block = Block::default()
        .title("Confirm Destroy")
        .borders(Borders::ALL)
        .border_type(theme::BORDER_TYPE)
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}

#[allow(clippy::cast_possible_truncation)]
fn draw_message_popup(frame: &mut Frame, title: &str, message: &str, is_error: bool) {
    let area = frame.area();

    // Calculate width based on message length, with min/max
    let msg_width = (message.len() + 6).min(u16::MAX as usize) as u16;
    let popup_width = msg_width.clamp(30, 60);

    // Wrap message if too long
    let chunk_size = popup_width.saturating_sub(4) as usize;
    let wrapped: Vec<&str> = message
        .as_bytes()
        .chunks(chunk_size.max(1))
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
        .collect();
    let popup_height = (wrapped.len().min(10) as u16 + 5).min(15);

    let popup_area = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup_area);

    let mut lines = vec![Line::from("")];
    for line in wrapped {
        lines.push(Line::from(format!("  {line}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Press any key to close",
        Style::default().fg(Color::DarkGray),
    )]));

    let border_color = if is_error { Color::Red } else { Color::Green };

    let block = Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_type(theme::BORDER_TYPE)
        .border_style(Style::default().fg(border_color));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}
