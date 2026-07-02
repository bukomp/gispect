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

    if let Ok(state) = app.update_state.lock() {
        match &*state {
            crate::app::UpdateState::Available(hash) => spans.push(Span::styled(
                format!("  UPDATE AVAILABLE: {} (press U)", &hash[..hash.len().min(7)]),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            crate::app::UpdateState::Failed(_) => spans.push(Span::styled(
                "  update check failed (U to retry)",
                Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
            )),
            crate::app::UpdateState::Checking | crate::app::UpdateState::UpToDate => {}
        }
    }

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some((status, _)) = &app.status {
        Line::from(Span::styled(
            format!(" {status}"),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            " j/k files  J/K scroll  m mode  c compact  s syntax  f files  b base  r reload  U update  ? help  q quit",
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_body(f: &mut Frame, app: &App, area: Rect) {
    let diff_area = if app.show_files {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(0)])
            .split(area);
        draw_file_list(f, app, chunks[0]);
        chunks[1]
    } else {
        area
    };

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(diff_area);

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
    // With the file list hidden, show the selected path in the pane title
    // so the user keeps their bearings.
    let title = match (is_left, app.show_files, app.files.get(app.selected)) {
        (true, false, Some(e)) => format!("Old — {}", e.path),
        (false, false, Some(e)) => format!("New — {}", e.path),
        (true, ..) => "Old".to_string(),
        (false, ..) => "New".to_string(),
    };
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

        let (line_no_str, segments): (String, Vec<(Style, String)>) = match cell {
            Some(c) => {
                let line_no_str = format!("{:>4} ", c.line_no);

                // Base segments: syntax-highlighted spans when enabled and
                // available, otherwise a single unstyled segment.
                let base_segments: Vec<(Style, String)> = if app.syntax {
                    let hl = if is_left { &app.left_hl } else { &app.right_hl };
                    hl.get(c.line_no - 1)
                        .cloned()
                        .unwrap_or_else(|| vec![(Style::default(), c.content.clone())])
                } else {
                    vec![(Style::default(), c.content.clone())]
                };

                // Expand tabs within each segment.
                let base_segments: Vec<(Style, String)> = base_segments
                    .into_iter()
                    .map(|(s, text)| (s, text.replace('\t', "    ")))
                    .collect();

                let segments = if app.syntax {
                    // Keep syntax foregrounds; mark diff roles with a
                    // background tint instead of overriding the foreground.
                    let bg = match (row.kind, is_left) {
                        (RowKind::Removed, true) => Some(Color::Rgb(80, 30, 30)),
                        (RowKind::Modified, true) => Some(Color::Rgb(80, 30, 30)),
                        (RowKind::Added, false) => Some(Color::Rgb(25, 70, 35)),
                        (RowKind::Modified, false) => Some(Color::Rgb(25, 70, 35)),
                        _ => None,
                    };
                    if let Some(bg) = bg {
                        base_segments
                            .into_iter()
                            .map(|(s, text)| (s.patch(Style::default().bg(bg)), text))
                            .collect()
                    } else {
                        base_segments
                    }
                } else {
                    // Syntax highlighting off: preserve the original
                    // foreground-color-only role styling.
                    let style = match row.kind {
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
                    };
                    vec![(style, base_segments.into_iter().map(|(_, t)| t).collect())]
                };

                (line_no_str, segments)
            }
            None => (
                "     ".to_string(),
                vec![(Style::default().add_modifier(Modifier::DIM), String::new())],
            ),
        };

        let content_width = width.saturating_sub(line_no_str.len());
        let sliced = slice_segments(&segments, app.h_scroll, content_width);

        let mut spans = vec![Span::styled(
            line_no_str,
            Style::default().add_modifier(Modifier::DIM),
        )];
        spans.extend(sliced.into_iter().map(|(style, text)| Span::styled(text, style)));

        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// Skip `skip` characters and keep at most `take` characters across a
/// sequence of styled segments, preserving per-segment styles and staying
/// char-boundary-safe.
fn slice_segments(segments: &[(Style, String)], skip: usize, take: usize) -> Vec<(Style, String)> {
    let mut skip_remaining = skip;
    let mut take_remaining = take;
    let mut result = Vec::new();

    for (style, text) in segments {
        if take_remaining == 0 {
            break;
        }
        let char_count = text.chars().count();
        if skip_remaining >= char_count {
            skip_remaining -= char_count;
            continue;
        }
        let piece: String = text
            .chars()
            .skip(skip_remaining)
            .take(take_remaining)
            .collect();
        skip_remaining = 0;
        if !piece.is_empty() {
            take_remaining -= piece.chars().count();
            result.push((*style, piece));
        }
    }

    result
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
    let height = 18u16.min(area.height);
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
        Line::from("mouse wheel  scroll diff (shift: horizontal)"),
        Line::from("m            cycle diff mode"),
        Line::from("c            toggle compact view (hide filler)"),
        Line::from("s            toggle syntax highlighting"),
        Line::from("f            toggle file list panel"),
        Line::from("b            cycle base branch"),
        Line::from("r            reload"),
        Line::from("U            apply update"),
        Line::from("? / Esc      toggle this help / quit"),
    ];

    let block = Block::default().borders(Borders::ALL).title("Help");
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, popup);
}
