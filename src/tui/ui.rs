use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::app::{App, TreeNode, flatten_tree, format_time_ago};
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

pub fn draw(frame: &mut Frame, app: &mut App) {
    // Top-level layout: header + overlap bar (optional) + pane grid + footer
    let has_overlaps = !app.overlaps.is_empty();

    let mut constraints = vec![Constraint::Length(1)]; // header
    if has_overlaps {
        // One line per unique overlap pair, capped at 3
        #[allow(clippy::cast_possible_truncation)]
        let overlap_lines = app.overlaps.len().min(3) as u16;
        constraints.push(Constraint::Length(overlap_lines));
    }
    constraints.push(Constraint::Min(0)); // pane grid
    constraints.push(Constraint::Length(1)); // footer

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let mut slot = 0;
    let header_area = outer[slot];
    slot += 1;

    let overlap_area = if has_overlaps {
        let area = outer[slot];
        slot += 1;
        Some(area)
    } else {
        None
    };

    let grid_area = outer[slot];
    slot += 1;
    let footer_area = outer[slot];

    // Draw header
    draw_header(frame, app, header_area);

    // Draw overlap bar
    if let Some(area) = overlap_area {
        draw_overlap_bar(frame, app, area);
    }

    // Draw pane grid
    draw_pane_grid(frame, app, grid_area);

    // Draw footer
    draw_footer(frame, footer_area);

    // Help popup (rendered on top)
    if app.show_help {
        draw_help_popup(frame);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let ws_count = app.workspaces.len();
    let epoch_part = if app.epoch_hash.is_empty() {
        String::new()
    } else {
        format!(" @ {}", app.epoch_hash)
    };

    let line = Line::from(vec![
        Span::styled(" maw ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{}{}", app.branch_name, epoch_part)),
        Span::raw(format!("  {ws_count} workspace{}", if ws_count == 1 { "" } else { "s" })),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_overlap_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    for entry in app.overlaps.iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled(
                " !! ",
                Style::default()
                    .fg(theme::OVERLAP)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("overlap: ", Style::default().fg(theme::OVERLAP)),
            Span::raw(&entry.ws_a),
            Span::styled(" <> ", Style::default().fg(theme::OVERLAP)),
            Span::raw(&entry.ws_b),
            Span::raw("  "),
            Span::styled(&entry.path, Style::default().fg(theme::OVERLAP)),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_pane_grid(frame: &mut Frame, app: &mut App, area: Rect) {
    let ws_count = app.workspaces.len();
    if ws_count == 0 {
        let block = styled_block("No workspaces", false);
        let text = Paragraph::new("  No agent workspaces found. Create one with: maw ws create <name>")
            .block(block);
        frame.render_widget(text, area);
        app.pane_areas.clear();
        return;
    }

    // Tiling strategy:
    //   1 workspace: full width
    //   2 workspaces: side by side
    //   3+ workspaces: 2-column grid
    app.pane_areas = compute_pane_rects(ws_count, area);

    for (i, ws) in app.workspaces.iter().enumerate() {
        if let Some(&rect) = app.pane_areas.get(i) {
            let is_focused = i == app.focused_pane;
            let selected = if is_focused {
                Some(app.selected_row)
            } else {
                None
            };
            draw_workspace_pane(frame, ws, rect, is_focused, selected, app.overlap_paths.get(&ws.name));
        }
    }
}

fn compute_pane_rects(count: usize, area: Rect) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![area];
    }
    if count == 2 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        return vec![cols[0], cols[1]];
    }

    // 2-column grid
    let cols = 2;
    let rows = count.div_ceil(cols);
    #[allow(clippy::cast_possible_truncation)]
    let row_constraints: Vec<Constraint> = (0..rows)
        .map(|_| Constraint::Ratio(1, rows as u32))
        .collect();

    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    let mut rects = Vec::with_capacity(count);
    for (row_idx, &row_rect) in row_rects.iter().enumerate() {
        let items_in_row = if row_idx == rows - 1 {
            count - row_idx * cols
        } else {
            cols
        };

        if items_in_row == 1 {
            // Last row with single item: span full width
            rects.push(row_rect);
        } else {
            let col_rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(row_rect);
            for c in 0..items_in_row {
                rects.push(col_rects[c]);
            }
        }
    }

    rects
}

#[allow(clippy::too_many_arguments)]
fn draw_workspace_pane(
    frame: &mut Frame,
    ws: &super::app::WorkspacePane,
    area: Rect,
    is_focused: bool,
    selected: Option<usize>,
    overlap_set: Option<&std::collections::BTreeSet<String>>,
) {
    // Build title line: name* +N commits Xm ago [stale]
    let mut title_parts = vec![ws.name.clone()];
    if ws.is_dirty {
        title_parts.push("*".to_string());
    }
    title_parts.push(format!(
        "  +{} commit{}",
        ws.commit_count,
        if ws.commit_count == 1 { "" } else { "s" }
    ));
    if let Some(secs) = ws.last_activity_secs {
        title_parts.push(format!("  {}", format_time_ago(secs)));
    }
    if ws.is_stale {
        title_parts.push("  stale".to_string());
    }
    let title = title_parts.join("");

    let block = if ws.is_stale {
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(theme::BORDER_TYPE)
            .border_style(if is_focused {
                Style::default().fg(theme::FOCUSED)
            } else {
                Style::default().fg(theme::STALE)
            })
    } else {
        styled_block(&title, is_focused)
    };

    // Flatten file tree for rendering
    let flat = flatten_tree(&ws.file_tree, 0);

    if flat.is_empty() {
        let text = Paragraph::new("  (no changes)").block(block);
        frame.render_widget(text, area);
        return;
    }

    let items: Vec<ListItem> = flat
        .iter()
        .enumerate()
        .map(|(i, (depth, node))| {
            let indent = "  ".repeat(*depth);
            let is_selected = selected == Some(i);

            match node {
                TreeNode::Dir {
                    name, collapsed, ..
                } => {
                    let arrow = if *collapsed { ">" } else { "v" };
                    let content = format!("{indent}{arrow} {name}/");
                    let style = if is_selected {
                        Style::default()
                            .bg(theme::SELECTED_BG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(content).style(style)
                }
                TreeNode::File {
                    name,
                    status,
                    full_path,
                } => {
                    let color = match status {
                        super::app::FileStatus::Modified => theme::FILE_MODIFIED,
                        super::app::FileStatus::Added => theme::FILE_ADDED,
                        super::app::FileStatus::Deleted => theme::FILE_DELETED,
                        super::app::FileStatus::Renamed => theme::FILE_RENAMED,
                    };

                    // Check if this file overlaps with another workspace
                    let is_overlap = overlap_set
                        .is_some_and(|set| set.contains(full_path.as_str()));

                    let display_color = if is_overlap { theme::OVERLAP } else { color };

                    let line = Line::from(vec![
                        Span::raw(format!("{indent}  ")),
                        Span::styled(
                            status.label(),
                            Style::default().fg(display_color),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            name.clone(),
                            Style::default().fg(display_color),
                        ),
                    ]);

                    let style = if is_selected {
                        Style::default()
                            .bg(theme::SELECTED_BG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };

                    ListItem::new(line).style(style)
                }
            }
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled("r", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" refresh  "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit  "),
        Span::styled("j/k", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" navigate  "),
        Span::styled("tab", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" cycle pane  "),
        Span::styled("?", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" help"),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_help_popup(frame: &mut Frame) {
    let area = frame.area();

    let popup_width = 50.min(area.width.saturating_sub(4));
    let popup_height = 16.min(area.height.saturating_sub(4));
    let popup_x = (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = (area.height.saturating_sub(popup_height)) / 2;

    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let help_text = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  j/k, Up/Down   Move selection"),
        Line::from("  g/G            Go to top/bottom"),
        Line::from("  Tab/Shift-Tab  Cycle panes"),
        Line::from("  Enter/Space    Toggle dir collapse"),
        Line::from(""),
        Line::from(Span::styled(
            "General",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  r              Refresh"),
        Line::from("  ?              This help"),
        Line::from("  q              Quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Display",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  * after name   Uncommitted changes"),
        Line::from("  !! overlap     Files changed by 2+ ws"),
    ];

    let block = Block::default()
        .title("Keybindings")
        .borders(Borders::ALL)
        .border_type(theme::BORDER_TYPE)
        .border_style(Style::default().fg(theme::FOCUSED));

    let paragraph = Paragraph::new(help_text).block(block);
    frame.render_widget(paragraph, popup_area);
}
