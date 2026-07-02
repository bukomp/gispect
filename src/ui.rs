//! Rendering of the gispect TUI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::types::{FileStatus, RowKind};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_status_bar(f, app, chunks[0]);
    draw_body(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.show_help {
        draw_help(f, area);
    }
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(format!(
        " gispect | {} | mode: {} | base: {} | {} files changed",
        app.branch,
        app.mode.label(),
        app.base,
        app.files.len()
    ))];

    let update_hash = app.update_available.lock().ok().and_then(|g| g.clone());
    if update_hash.is_some() {
        spans.push(Span::styled(
            "  UPDATE AVAILABLE (press U)",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(status) = &app.status {
        Line::from(Span::styled(
            format!(" {status}"),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            " j/k files  J/K scroll  m mode  c compact  b base  r reload  U update  ? help  q quit",
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_body(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(34),
            Constraint::Min(0),
        ])
        .split(area);

    draw_file_list(f, app, chunks[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    draw_diff_pane(f, app, panes[0], true);
    draw_diff_pane(f, app, panes[1], false);
}

fn draw_file_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Files");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.files.is_empty() {
        let msg = Paragraph::new("(no changed files)")
            .style(Style::default().add_modifier(Modifier::DIM));
        f.render_widget(msg, inner);
        return;
    }

    let height = inner.height as usize;
    let offset = window_offset(app.selected, app.files.len(), height);

    let mut lines = Vec::with_capacity(height);
    for (i, entry) in app.files.iter().enumerate().skip(offset).take(height) {
        let marker = entry.status.marker();
        let marker_color = match &entry.status {
            FileStatus::Added => Color::Green,
            FileStatus::Modified => Color::Yellow,
            FileStatus::Deleted => Color::Red,
            FileStatus::Renamed { .. } => Color::Cyan,
            FileStatus::Other(_) => Color::White,
        };

        let mut spans = vec![
            Span::styled(format!("{marker} "), Style::default().fg(marker_color)),
            Span::raw(entry.path.clone()),
            Span::raw("  "),
            Span::styled(format!("+{}", entry.additions), Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(format!("-{}", entry.deletions), Style::default().fg(Color::Red)),
        ];

        let mut style = Style::default();
        if i == app.selected {
            style = style.add_modifier(Modifier::REVERSED);
            for span in spans.iter_mut() {
                span.style = span.style.patch(style);
            }
        }

        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// Compute a scroll offset that keeps `selected` visible within a list of
/// `total` items rendered in a viewport of `height` rows.
fn window_offset(selected: usize, total: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        0
    } else if selected >= total.saturating_sub(1) {
        total - height
    } else {
        selected.saturating_sub(height / 2).min(total - height)
    }
}

fn draw_diff_pane(f: &mut Frame, app: &App, area: Rect, is_left: bool) {
    let title = if is_left { "Old" } else { "New" };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.diff.rows.is_empty() {
        let msg = Paragraph::new("(no textual diff)")
            .style(Style::default().add_modifier(Modifier::DIM))
            .alignment(ratatui::layout::Alignment::Center);
        let centered = center_vertically(inner, 1);
        f.render_widget(msg, centered);
        return;
    }

    let height = inner.height as usize;
    let width = inner.width as usize;

    // In compact mode drop rows that are pure filler for this pane, so the
    // code reads contiguously (panes then scroll independently of alignment).
    let rows: Vec<&crate::types::DiffRow> = if app.compact {
        app.diff
            .rows
            .iter()
            .filter(|r| if is_left { r.left.is_some() } else { r.right.is_some() })
            .collect()
    } else {
        app.diff.rows.iter().collect()
    };

    let start = app.scroll.min(rows.len().saturating_sub(1));
    let end = (start + height).min(rows.len());

    let mut lines = Vec::with_capacity(height);
    for row in &rows[start..end] {
        let cell = if is_left { &row.left } else { &row.right };

        let (line_no_str, content) = match cell {
            Some(c) => (format!("{:>4} ", c.line_no), c.content.clone()),
            None => ("     ".to_string(), String::new()),
        };

        let style = if cell.is_none() {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            match row.kind {
                RowKind::Context => Style::default(),
                RowKind::Removed => {
                    if is_left {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().add_modifier(Modifier::DIM)
                    }
                }
                RowKind::Added => {
                    if is_left {
                        Style::default().add_modifier(Modifier::DIM)
                    } else {
                        Style::default().fg(Color::Green)
                    }
                }
                RowKind::Modified => Style::default().fg(Color::Yellow),
            }
        };

        let expanded = content.replace('\t', "    ");
        let sliced: String = expanded.chars().skip(app.h_scroll).collect();
        let content_width = width.saturating_sub(line_no_str.len());
        let truncated: String = sliced.chars().take(content_width).collect();

        let line = Line::from(vec![
            Span::styled(line_no_str, Style::default().add_modifier(Modifier::DIM)),
            Span::styled(truncated, style),
        ]);
        lines.push(line);
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn center_vertically(area: Rect, content_height: u16) -> Rect {
    if area.height <= content_height {
        return area;
    }
    let top = (area.height - content_height) / 2;
    Rect {
        x: area.x,
        y: area.y + top,
        width: area.width,
        height: content_height,
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let width = 50u16.min(area.width);
    let height = 15u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect { x, y, width, height };

    f.render_widget(Clear, popup);

    let text = vec![
        Line::from("gispect — key bindings"),
        Line::from(""),
        Line::from("j / Down     next file"),
        Line::from("k / Up       previous file"),
        Line::from("J / K        scroll diff down / up"),
        Line::from("Ctrl-d/u     half-page scroll"),
        Line::from("PgDn / PgUp  half-page scroll"),
        Line::from("g / G        top / bottom of diff"),
        Line::from("h/l ← →      scroll horizontally"),
        Line::from("m            cycle diff mode"),
        Line::from("c            toggle compact view (hide filler)"),
        Line::from("b            cycle base branch"),
        Line::from("r            reload"),
        Line::from("U            apply update"),
        Line::from("? / Esc      toggle this help / quit"),
    ];

    let block = Block::default().borders(Borders::ALL).title("Help");
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, popup);
}
