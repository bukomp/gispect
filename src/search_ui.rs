//! Rendering for the search UI: the footer input prompt and the
//! committed in-file-search status line.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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
            SearchKind::Content => " search changes: ",
            SearchKind::PathFilter => " filter: ",
        };
        let hint = match input.kind {
            SearchKind::File => "  (Enter search · Esc cancel)",
            SearchKind::Content => "  (Enter keep filter · Esc clear)",
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
