//! Rendering for the search UI: the footer input prompt and the
//! project-wide search results popup.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::search::SearchKind;

/// Background tint for search matches in the diff panes.
pub const MATCH_BG: Color = Color::Rgb(88, 68, 0);
/// Background tint for the current (n/N cursor) match row.
pub const CURRENT_MATCH_BG: Color = Color::Rgb(150, 110, 0);

/// Draw the search input prompt into the footer `area` when a prompt is
/// active. Returns true if it drew (the caller then skips the normal
/// footer content).
pub fn draw_search_footer(f: &mut Frame, app: &App, area: Rect) -> bool {
    if let Some(input) = &app.search_input {
        let prefix = match input.kind {
            SearchKind::File => " /",
            SearchKind::Project => " search all: ",
            SearchKind::PathFilter => " filter: ",
        };
        let hint = match input.kind {
            SearchKind::File => "  (Enter search · Esc cancel)",
            SearchKind::Project => "  (Enter search all files · Esc cancel)",
            SearchKind::PathFilter => "  (Enter keep filter · Esc clear)",
        };
        let line = Line::from(vec![
            Span::raw(prefix),
            Span::raw(input.query.clone()),
            Span::raw("█"),
            Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
        ])
        .style(Style::default().fg(Color::Yellow));
        f.render_widget(Paragraph::new(line), area);
        return true;
    }

    if let Some(fs) = &app.file_search {
        let total = fs.matches.len();
        let line = if total == 0 {
            Line::from(vec![
                Span::styled(
                    format!(" /{}", fs.query),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    " — no matches  Esc clear",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    format!(" /{}", fs.query),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    format!(
                        " — match {}/{}  n/N next/prev · Esc clear",
                        fs.current + 1,
                        total
                    ),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ])
        };
        f.render_widget(Paragraph::new(line), area);
        return true;
    }

    false
}

/// Draw the centered project-search results popup over `area` when
/// `app.project_search` is set.
pub fn draw_project_results(f: &mut Frame, app: &mut App, area: Rect) {
    if app.project_search.is_none() {
        return;
    }

    let width = ((area.width as u32 * 90 / 100) as u16)
        .max(20)
        .min(area.width);
    let height = ((area.height as u32 * 70 / 100) as u16)
        .max(8)
        .min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect { x, y, width, height };

    f.render_widget(Clear, popup);

    let ps = app.project_search.as_ref().unwrap();
    let title = format!(
        "Search \"{}\" — {} matches  (j/k move · Enter open · Esc close)",
        ps.query,
        ps.matches.len()
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    if ps.matches.is_empty() {
        let msg = Paragraph::new("(no matches)")
            .style(Style::default().add_modifier(Modifier::DIM));
        f.render_widget(msg, inner);
        return;
    }

    let inner_height = inner.height as usize;
    let inner_width = inner.width as usize;
    let offset = window_offset(ps.selected, ps.matches.len(), inner_height);

    // Report back the effective offset so key handling can page against it.
    if let Some(ps_mut) = app.project_search.as_mut() {
        ps_mut.scroll = offset;
    }

    let ps = app.project_search.as_ref().unwrap();
    let mut lines = Vec::with_capacity(inner_height);
    for (i, m) in ps
        .matches
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
    {
        let mut spans = vec![Span::styled(
            format!("{}:{}: ", m.path, m.line_no),
            Style::default().add_modifier(Modifier::DIM),
        )];
        if m.in_old {
            spans.push(Span::styled(
                "(old) ",
                Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
            ));
        }
        spans.push(Span::raw(m.line.trim().to_string()));

        // Truncate the whole row to the inner width, char-safely.
        let mut remaining = inner_width;
        let mut truncated: Vec<Span> = Vec::with_capacity(spans.len());
        for span in spans {
            if remaining == 0 {
                break;
            }
            let text: String = span.content.chars().take(remaining).collect();
            remaining -= text.chars().count();
            truncated.push(Span::styled(text, span.style));
        }

        if i == ps.selected {
            let style = Style::default().add_modifier(Modifier::REVERSED);
            for span in truncated.iter_mut() {
                span.style = span.style.patch(style);
            }
        }

        lines.push(Line::from(truncated));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// Compute a scroll offset that keeps `selected` visible within a list of
/// `total` items rendered in a viewport of `height` rows. Mirrors
/// `ui::window_offset`; kept local so this module stays self-contained.
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
