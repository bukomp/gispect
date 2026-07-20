//! Rendering of the gispect TUI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, FilePanelView};
use crate::types::{FileStatus, RowKind};

pub fn draw(f: &mut Frame, app: &mut App) {
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
    // Footer priority: active prompt, then transient status, then the
    // persistent search line, then the default hints.
    let status_pending = app.status.is_some() && app.search_input.is_none();
    if status_pending || !crate::search_ui::draw_search_footer(f, app, chunks[2]) {
        draw_footer(f, app, chunks[2]);
    }

    if app.show_help {
        draw_help(f, area);
    }

    if app.config_modal.is_some() {
        draw_config_modal(f, app, area);
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
            " j/k files  J/K scroll  n/N change  / search  ? help  q quit",
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    app.old_pane_area = Rect::default();
    app.new_pane_area = Rect::default();

    let diff_area = if app.show_files {
        let rows = if app.tree_view {
            tree_rows(app)
        } else {
            list_rows(app)
        };
        let width = file_panel_width(&rows, app.wide_files, area.width);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(width), Constraint::Min(0)])
            .split(area);
        draw_file_list(f, app, chunks[0], rows);
        chunks[1]
    } else {
        app.file_view = FilePanelView::default();
        area
    };

    match (app.show_old, app.show_new) {
        (true, true) => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(diff_area);
            draw_diff_pane(f, app, panes[0], true);
            draw_diff_pane(f, app, panes[1], false);
        }
        (true, false) => draw_diff_pane(f, app, diff_area, true),
        (false, true) => draw_diff_pane(f, app, diff_area, false),
        // toggle_pane never allows both panes hidden.
        (false, false) => {}
    }
}

/// One visual row of the file panel: a selectable file (with its index
/// into `app.files`) or a non-selectable directory header in tree view.
struct FileRow<'a> {
    file_idx: Option<usize>,
    spans: Vec<Span<'a>>,
}

/// Width of the file panel: fixed by default, sized to the widest row
/// (plus borders) when expanded — capped so the diff keeps some room.
fn file_panel_width(rows: &[FileRow], wide: bool, total_width: u16) -> u16 {
    const DEFAULT_WIDTH: u16 = 34;
    if !wide {
        return DEFAULT_WIDTH.min(total_width);
    }
    let content = rows
        .iter()
        .map(|r| r.spans.iter().map(|s| s.width()).sum::<usize>())
        .max()
        .unwrap_or(0);
    let cap = (total_width as usize * 2) / 3;
    ((content + 2).clamp(DEFAULT_WIDTH as usize, cap.max(DEFAULT_WIDTH as usize))) as u16
}

fn draw_file_list(f: &mut Frame, app: &mut App, area: Rect, rows: Vec<FileRow<'static>>) {
    // With any filter active, show visible/total so hidden files are
    // accounted for at a glance.
    let filtered = app.active_path_filter().is_some() || app.active_content_filter().is_some();
    let count = if filtered {
        format!(
            "{}/{}",
            rows.iter().filter(|r| r.file_idx.is_some()).count(),
            app.files.len()
        )
    } else {
        app.files.len().to_string()
    };
    let title = format!(
        "Files ({}) [f] {} [t] {} [F]",
        count,
        if app.tree_view { "tree" } else { "list" },
        if app.wide_files { "wide" } else { "auto" }
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if rows.is_empty() {
        app.file_view = FilePanelView {
            area: inner,
            ..FilePanelView::default()
        };
        let msg = Paragraph::new(if app.files.is_empty() {
            "(no changed files)"
        } else {
            "(no files match filter)"
        })
        .style(Style::default().add_modifier(Modifier::DIM));
        f.render_widget(msg, inner);
        return;
    }

    let height = inner.height as usize;
    let selected_row = rows
        .iter()
        .position(|r| r.file_idx == Some(app.selected))
        .unwrap_or(0);
    // A wheel/PgUp scroll detaches the panel from the selection; otherwise
    // the window follows the selected file.
    let offset = match app.file_scroll {
        Some(o) => o.min(rows.len().saturating_sub(height)),
        None => window_offset(selected_row, rows.len(), height),
    };
    app.file_view = FilePanelView {
        area: inner,
        offset,
        total: rows.len(),
        rows: rows
            .iter()
            .skip(offset)
            .take(height)
            .map(|r| r.file_idx)
            .collect(),
    };

    let mut lines = Vec::with_capacity(height);
    for row in rows.into_iter().skip(offset).take(height) {
        let mut spans = row.spans;
        if row.file_idx == Some(app.selected) {
            let style = Style::default().add_modifier(Modifier::REVERSED);
            for span in spans.iter_mut() {
                span.style = span.style.patch(style);
            }
        }
        let pairs: Vec<(Style, String)> = spans
            .into_iter()
            .map(|span| (span.style, span.content.into_owned()))
            .collect();
        let sliced = slice_segments(&pairs, app.h_scroll_files, inner.width as usize);
        let spans: Vec<Span> = sliced
            .into_iter()
            .map(|(style, text)| Span::styled(text, style))
            .collect();
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// Marker character span (A/M/D/R…) colored by file status.
fn marker_span(status: &FileStatus) -> Span<'static> {
    let color = match status {
        FileStatus::Added => Color::Green,
        FileStatus::Modified => Color::Yellow,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed { .. } => Color::Cyan,
        FileStatus::Other(_) => Color::White,
    };
    Span::styled(format!("{} ", status.marker()), Style::default().fg(color))
}

/// `+N -M` change-count spans for one file entry.
fn count_spans(entry: &crate::types::FileEntry) -> Vec<Span<'static>> {
    vec![
        Span::raw("  "),
        Span::styled(format!("+{}", entry.additions), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("-{}", entry.deletions), Style::default().fg(Color::Red)),
    ]
}

/// Flat view: one row per file with its full path. Files hidden by the
/// active path filter are skipped.
fn list_rows(app: &App) -> Vec<FileRow<'static>> {
    app.files
        .iter()
        .enumerate()
        .filter(|(i, _)| app.passes_filter(*i))
        .map(|(i, entry)| {
            let mut spans = vec![marker_span(&entry.status), Span::raw(entry.path.clone())];
            spans.extend(count_spans(entry));
            FileRow {
                file_idx: Some(i),
                spans,
            }
        })
        .collect()
}

/// A node of the file-panel tree: a directory with children, or a leaf
/// file carrying its index into `app.files`.
enum TreeNode {
    Dir { name: String, children: Vec<TreeNode> },
    File { idx: usize, name: String },
}

/// Tree view: directories and files rendered with `tree`-style ASCII
/// connectors (├──, └──, │). Files keep their `app.files` order (git
/// emits paths sorted, so siblings group naturally).
fn tree_rows(app: &App) -> Vec<FileRow<'static>> {
    let mut roots: Vec<TreeNode> = Vec::new();

    for (i, entry) in app
        .files
        .iter()
        .enumerate()
        .filter(|(i, _)| app.passes_filter(*i))
    {
        let mut parts: Vec<&str> = entry.path.split('/').collect();
        let name = parts.pop().unwrap_or(entry.path.as_str()).to_string();

        // Descend the directory chain, reusing the last child when it is
        // the same directory (paths arrive sorted, so siblings are
        // consecutive) and creating new dir nodes otherwise.
        let mut children = &mut roots;
        for part in parts {
            let reuse = matches!(
                children.last(),
                Some(TreeNode::Dir { name, .. }) if name.as_str() == part
            );
            if !reuse {
                children.push(TreeNode::Dir {
                    name: part.to_string(),
                    children: Vec::new(),
                });
            }
            children = match children.last_mut() {
                Some(TreeNode::Dir { children, .. }) => children,
                _ => unreachable!("last child was just ensured to be a Dir"),
            };
        }
        children.push(TreeNode::File { idx: i, name });
    }

    let mut rows = Vec::new();
    emit_tree(&roots, "", app, &mut rows);
    rows
}

/// Recursively render tree nodes into panel rows. `prefix` accumulates
/// the `│   `/`    ` guides owed to ancestor levels.
fn emit_tree(nodes: &[TreeNode], prefix: &str, app: &App, rows: &mut Vec<FileRow<'static>>) {
    for (i, node) in nodes.iter().enumerate() {
        let last = i + 1 == nodes.len();
        let connector = if last { "└── " } else { "├── " };
        let branch = Span::styled(
            format!("{prefix}{connector}"),
            Style::default().add_modifier(Modifier::DIM),
        );
        match node {
            TreeNode::Dir { name, children } => {
                rows.push(FileRow {
                    file_idx: None,
                    spans: vec![
                        branch,
                        Span::styled(format!("{name}/"), Style::default().fg(Color::Blue)),
                    ],
                });
                let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
                emit_tree(children, &child_prefix, app, rows);
            }
            TreeNode::File { idx, name } => {
                let entry = &app.files[*idx];
                let mut spans = vec![branch, marker_span(&entry.status), Span::raw(name.clone())];
                spans.extend(count_spans(entry));
                rows.push(FileRow {
                    file_idx: Some(*idx),
                    spans,
                });
            }
        }
    }
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

fn draw_diff_pane(f: &mut Frame, app: &mut App, area: Rect, is_left: bool) {
    // With the file list hidden, show the selected path in the pane title
    // so the user keeps their bearings.
    let title = match (is_left, app.show_files, app.files.get(app.selected)) {
        (true, false, Some(e)) => format!("Old [1] — {}", e.path),
        (false, false, Some(e)) => format!("New [2] — {}", e.path),
        (true, ..) => "Old [1]".to_string(),
        (false, ..) => "New [2]".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    app.diff_height = inner.height as usize;
    if is_left {
        app.old_pane_area = inner;
    } else {
        app.new_pane_area = inner;
    }

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
    // Rows keep their aligned index so search matches can be looked up.
    let rows: Vec<(usize, &crate::types::DiffRow)> = if app.compact {
        app.diff
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| if is_left { r.left.is_some() } else { r.right.is_some() })
            .collect()
    } else {
        app.diff.rows.iter().enumerate().collect()
    };

    let start = app.scroll.min(rows.len().saturating_sub(1));
    let end = (start + height).min(rows.len());

    let mut lines = Vec::with_capacity(height);
    for (row_idx, row) in &rows[start..end] {
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
                        (RowKind::Removed, true) => Some(Color::Rgb(52, 18, 18)),
                        (RowKind::Modified, true) => Some(Color::Rgb(52, 18, 18)),
                        (RowKind::Added, false) => Some(Color::Rgb(16, 45, 22)),
                        (RowKind::Modified, false) => Some(Color::Rgb(16, 45, 22)),
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

        // Emphasize intra-line changed spans on Modified rows with a
        // stronger background tint than the whole-line role tint above.
        // Ranges are char offsets into the original (unexpanded) content,
        // so they must be shifted for tabs expanded in step 2.
        let segments = match (row.kind, cell) {
            (RowKind::Modified, Some(c)) if !c.changed.is_empty() => {
                let emphasis_bg = if is_left {
                    Color::Rgb(110, 36, 36)
                } else {
                    Color::Rgb(30, 95, 45)
                };
                let adjusted: Vec<(usize, usize)> = c
                    .changed
                    .iter()
                    .map(|r| expand_tab_range(&c.content, *r))
                    .collect();
                overlay_ranges(&segments, &adjusted, Style::default().bg(emphasis_bg))
            }
            _ => segments,
        };

        // Tint search matches on top of whatever styling the row already
        // has; the current (n/N cursor) match gets a brighter tint.
        let segments = match &app.file_search {
            Some(fs) => {
                let bg = if fs.matches.get(fs.current).map(|m| m.row) == Some(*row_idx) {
                    crate::search_ui::CURRENT_MATCH_BG
                } else {
                    crate::search_ui::MATCH_BG
                };
                crate::search::highlight_segments(&segments, &fs.query, Style::default().bg(bg))
            }
            None => segments,
        };

        let content_width = width.saturating_sub(line_no_str.len());
        let h_scroll = if is_left { app.h_scroll_old } else { app.h_scroll_new };
        let sliced = slice_segments(&segments, h_scroll, content_width);

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

/// Shift a char range from original-content coordinates into
/// tab-expanded coordinates, given tabs are rendered as 4 spaces. Each
/// tab before a position adds 3 extra chars ahead of it.
fn expand_tab_range(content: &str, (start, end): (usize, usize)) -> (usize, usize) {
    let expand = |pos: usize| -> usize {
        let tabs_before = content.chars().take(pos).filter(|&c| c == '\t').count();
        pos + 3 * tabs_before
    };
    (expand(start), expand(end))
}

/// Patch `patch` over the given char ranges of a styled-segment line,
/// splitting segments at range boundaries. Ranges are char-indexed into
/// the concatenated segment text, sorted and non-overlapping. Kept in
/// the same spirit as `slice_segments`: char-boundary-safe, never
/// byte-slices.
fn overlay_ranges(
    segments: &[(Style, String)],
    ranges: &[(usize, usize)],
    patch: Style,
) -> Vec<(Style, String)> {
    if ranges.is_empty() {
        return segments.to_vec();
    }

    let mut result = Vec::new();
    let mut pos = 0usize; // char offset into the concatenated text so far
    let mut range_idx = 0usize;

    for (style, text) in segments {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            // Advance past ranges that ended before the current position.
            while range_idx < ranges.len() && ranges[range_idx].1 <= pos {
                range_idx += 1;
            }
            let in_range = range_idx < ranges.len()
                && ranges[range_idx].0 <= pos
                && pos < ranges[range_idx].1;
            let run_style = if in_range { style.patch(patch) } else { *style };
            // Find how far this run extends within the current segment,
            // stopping at the next range boundary or segment end.
            let run_end = if in_range {
                (ranges[range_idx].1 - pos).min(chars.len() - i)
            } else if range_idx < ranges.len() && ranges[range_idx].0 > pos {
                (ranges[range_idx].0 - pos).min(chars.len() - i)
            } else {
                chars.len() - i
            };
            let run_end = run_end.max(1); // always make progress
            let piece: String = chars[i..i + run_end].iter().collect();
            result.push((run_style, piece));
            i += run_end;
            pos += run_end;
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
    let width = 52u16.min(area.width);
    let height = 31u16.min(area.height);
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
        Line::from("PgDn / PgUp  full-page scroll (pane under mouse)"),
        Line::from("n / N        next / previous change (or match)"),
        Line::from("g / G        top / bottom of diff"),
        Line::from("/            search in current file"),
        Line::from("S            filter files by changed text"),
        Line::from("p            filter file panel by name"),
        Line::from("Esc          clear search / filters, then quit"),
        Line::from("h/l ← →      h-scroll pane under mouse"),
        Line::from("mouse wheel  scroll pane under cursor"),
        Line::from("mouse click  select file in the file panel"),
        Line::from("m            cycle diff mode"),
        Line::from("c            toggle compact view (hide filler)"),
        Line::from("s            toggle syntax highlighting"),
        Line::from("C            open config (startup defaults)"),
        Line::from("f            toggle file list panel"),
        Line::from("t            toggle file tree / list view"),
        Line::from("F            expand file panel to fit names"),
        Line::from("1 / 2        toggle old / new pane"),
        Line::from("b            cycle base branch"),
        Line::from("r            reload"),
        Line::from("U            apply update & restart"),
        Line::from("? / Esc      toggle this help / quit"),
    ];

    let block = Block::default().borders(Borders::ALL).title("Help");
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, popup);
}

fn draw_config_modal(f: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = app.config_modal.as_ref() else {
        return;
    };

    // 8 setting rows + blank + hint line, plus the border.
    let width = 54u16.min(area.width);
    let height = 12u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect { x, y, width, height };

    f.render_widget(Clear, popup);

    let mut lines: Vec<Line> = Vec::with_capacity(crate::config::ConfigField::ALL.len() + 2);
    for (i, field) in crate::config::ConfigField::ALL.into_iter().enumerate() {
        let text = format!(" {:<20} {} ", field.label(), modal.draft.value_label(field));
        let style = if i == modal.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " space toggle · u use current · s save · esc cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" config — startup defaults ");
    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Vec<(Style, String)> {
        vec![(Style::default(), text.to_string())]
    }

    fn text_of(segments: &[(Style, String)]) -> String {
        segments.iter().map(|(_, t)| t.as_str()).collect()
    }

    #[test]
    fn overlay_range_inside_one_segment_splits_into_three() {
        let segments = plain("hello");
        let patch = Style::default().bg(Color::Red);
        let out = overlay_ranges(&segments, &[(1, 3)], patch);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0], (Style::default(), "h".to_string()));
        assert_eq!(out[1], (Style::default().patch(patch), "el".to_string()));
        assert_eq!(out[2], (Style::default(), "lo".to_string()));
        assert_eq!(text_of(&out), "hello");
    }

    #[test]
    fn overlay_range_spans_two_segments() {
        let s1 = Style::default().fg(Color::Blue);
        let s2 = Style::default().fg(Color::Green);
        let segments = vec![(s1, "foo".to_string()), (s2, "bar".to_string())];
        let patch = Style::default().bg(Color::Red);
        // "foo bar" chars: f0 o1 o2 b3 a4 r5 -> range (2,5) covers o,b,a.
        let out = overlay_ranges(&segments, &[(2, 5)], patch);

        assert_eq!(text_of(&out), "foobar");
        assert_eq!(out[0], (s1, "fo".to_string()));
        assert_eq!(out[1], (s1.patch(patch), "o".to_string()));
        assert_eq!(out[2], (s2.patch(patch), "ba".to_string()));
        assert_eq!(out[3], (s2, "r".to_string()));
    }

    #[test]
    fn overlay_multiple_ranges() {
        let segments = plain("abcdefgh");
        let patch = Style::default().bg(Color::Red);
        let out = overlay_ranges(&segments, &[(1, 2), (4, 6)], patch);

        assert_eq!(text_of(&out), "abcdefgh");
        let patched: String = out
            .iter()
            .filter(|(s, _)| *s == Style::default().patch(patch))
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(patched, "bef");
    }

    #[test]
    fn overlay_multi_byte_chars_split_at_char_positions_without_panicking() {
        // "héllo": h(0) é(1) l(2) l(3) o(4) — é is multi-byte in UTF-8.
        let segments = plain("héllo");
        let patch = Style::default().bg(Color::Red);
        let out = overlay_ranges(&segments, &[(1, 3)], patch);

        assert_eq!(text_of(&out), "héllo");
        assert_eq!(out[0].1, "h");
        assert_eq!(out[1].1, "él");
        assert_eq!(out[2].1, "lo");
    }

    #[test]
    fn overlay_empty_ranges_returns_input_unchanged() {
        let segments = vec![
            (Style::default().fg(Color::Blue), "foo".to_string()),
            (Style::default().fg(Color::Green), "bar".to_string()),
        ];
        let out = overlay_ranges(&segments, &[], Style::default().bg(Color::Red));
        assert_eq!(out, segments);
    }

    #[test]
    fn expand_tab_range_shifts_by_tabs_before_position() {
        // Original chars: a(0) \t(1) b(2) \t(3) c(4).
        // Expanded: 'a' + 4 spaces + 'b' + 4 spaces + 'c', so 'b' moves
        // from original index 2 to expanded index 5.
        assert_eq!(expand_tab_range("a\tb\tc", (2, 3)), (5, 6));
    }

    #[test]
    fn expand_tab_range_no_tabs_is_identity() {
        assert_eq!(expand_tab_range("hello", (1, 3)), (1, 3));
    }

    #[test]
    fn expand_tab_range_multiple_tabs_before_start() {
        // "\t\tx": two tabs then x at original index 2 -> expanded index 8.
        assert_eq!(expand_tab_range("\t\tx", (2, 3)), (8, 9));
    }
}
