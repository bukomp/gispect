//! Rendering for the pull request view: the PR list screen with its info
//! and dash list popups, the review status bar, and the PR modals (inline
//! comment editor, review submission, PR description).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::gh::{PrDetails, PrSummary, ReviewEvent, Side};
use crate::pr::{self, Load, PrList, PrTab, PreloadStatus, Preloader};

pub(crate) const DIM: Style = Style::new().add_modifier(Modifier::DIM);

/// The full-screen PR list: status line, tab bar, rows, footer.
pub fn draw_pr_list(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let status = app.status.as_ref().map(|(s, _)| s.clone());
    let Some(list) = app.pr_list.as_mut() else {
        return;
    };
    let statuses = preload_statuses(list, &app.preloader);
    draw_list_status(f, list, &statuses, chunks[0]);
    draw_tabs(f, list, chunks[1]);
    draw_rows(f, list, &statuses, chunks[2]);
    draw_list_footer(f, list, status, chunks[3]);

    if let Some(scroll) = list.info_scroll {
        if let Some((account, summary)) = list.selected_row() {
            let info = Info {
                summary: &summary,
                details: app.preloader.details(&account, &summary),
                marked: list.is_marked(&summary),
                list: true,
            };
            draw_info(f, area, &info, scroll);
        }
    }
    if let Some(dash) = &list.dash_list {
        draw_dash_list(f, area, dash);
    }
}

/// Preload progress of each list row, in row order.
fn preload_statuses(list: &PrList, preloader: &Preloader) -> Vec<Option<PreloadStatus>> {
    let accounts = list.account_list();
    list.rows()
        .into_iter()
        .map(|(a, pr)| preloader.status(accounts.get(a)?, pr))
        .collect()
}

fn draw_list_status(f: &mut Frame, list: &PrList, statuses: &[Option<PreloadStatus>], area: Rect) {
    let accounts = match &list.accounts {
        Load::Loading => "loading…".to_string(),
        Load::Failed(_) => "unavailable".to_string(),
        Load::Ready(accounts) => match list.account_filter.and_then(|i| accounts.get(i)) {
            Some(a) => a.label(),
            None => accounts
                .iter()
                .map(|a| a.label())
                .collect::<Vec<_>>()
                .join(", "),
        },
    };
    let repos = match list.active_scope() {
        Some([(_, repo)]) => repo.clone(),
        Some(scope) => format!("{} local", scope.len()),
        None => "all".to_string(),
    };
    let mut text = format!(
        " gispect | pull requests | accounts: {accounts} | repos: {repos} | state: {} | drafts: {}",
        list.state.label(),
        if list.hide_drafts { "hidden" } else { "shown" }
    );
    if let Some(filter) = &list.text_filter {
        text.push_str(&format!(" | filter: {filter}"));
    }
    if !list.marked.is_empty() {
        text.push_str(&format!(" | marked: {}", list.marked.len()));
    }
    let ready = statuses
        .iter()
        .filter(|s| **s == Some(PreloadStatus::Ready))
        .count();
    if !statuses.is_empty() {
        text.push_str(&format!(" | preloaded: {ready}/{}", statuses.len()));
    }
    f.render_widget(Paragraph::new(text), area);
}

fn draw_tabs(f: &mut Frame, list: &PrList, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in PrTab::ALL.into_iter().enumerate() {
        let count = if !list.loading(tab) {
            format!(" ({})", list.rows_for(tab).len())
        } else if tab == list.tab {
            " (…)".to_string()
        } else {
            String::new()
        };
        let style = if tab == list.tab {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(
            format!(" {} {}{count} ", i + 1, tab.label()),
            style,
        ));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_rows(f: &mut Frame, list: &mut PrList, statuses: &[Option<PreloadStatus>], area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} — {} ",
        list.tab.label(),
        list.state.label()
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Load::Failed(e) = &list.accounts {
        let lines = vec![
            Line::from(Span::styled(
                format!("could not list gh accounts: {e}"),
                Style::default().fg(Color::Red),
            )),
            Line::from(Span::styled("run `gh auth login`, then press r", DIM)),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        list.view_area = Rect::default();
        return;
    }

    // Failed searches go on top; the rest of the area lists PRs.
    let errors: Vec<Line> = list
        .errors()
        .into_iter()
        .map(|(account, e)| {
            Line::from(Span::styled(
                format!("{account}: {e}"),
                Style::default().fg(Color::Red),
            ))
        })
        .collect();
    let error_height = (errors.len() as u16).min(inner.height);
    f.render_widget(
        Paragraph::new(errors),
        Rect {
            height: error_height,
            ..inner
        },
    );
    let rows_area = Rect {
        y: inner.y + error_height,
        height: inner.height - error_height,
        ..inner
    };

    let multi_account = list.account_list().len() > 1;
    let rows = list.rows();
    let height = rows_area.height as usize;
    let offset = crate::ui::window_offset(list.selected, rows.len(), height);

    if rows.is_empty() {
        let msg = if list.loading(list.tab) {
            " loading…"
        } else {
            " (no pull requests)"
        };
        f.render_widget(Paragraph::new(msg).style(DIM), rows_area);
    } else {
        let accounts = list.account_list();
        let cols = Columns::fit(&rows);
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(i, (account, pr))| {
                let account = multi_account
                    .then(|| accounts.get(*account).map(|a| a.label()))
                    .flatten();
                pr_row(
                    pr,
                    account,
                    statuses.get(i).copied().flatten(),
                    &cols,
                    rows_area.width as usize,
                    i == list.selected,
                    list.is_marked(pr),
                )
            })
            .collect();
        f.render_widget(Paragraph::new(lines), rows_area);
    }
    list.view_area = rows_area;
    list.view_offset = offset;
}

/// One list row: state glyph, `repo#number`, title, then right-aligned
/// account, author, size, review decision and age.
/// Widths of the right-hand columns, sized to the widest value in the
/// whole list so they line up (and don't jitter while scrolling).
struct Columns {
    /// `owner/repo#number`, so titles start in one column.
    repo: usize,
    author: usize,
    additions: usize,
    deletions: usize,
}

impl Columns {
    fn fit(rows: &[(usize, &PrSummary)]) -> Self {
        let max =
            |f: &dyn Fn(&PrSummary) -> usize| rows.iter().map(|(_, pr)| f(pr)).max().unwrap_or(0);
        Columns {
            repo: max(&|pr| repo_label(pr).len()).min(48),
            author: max(&|pr| pr.author.chars().count()).min(24),
            additions: max(&|pr| pr.additions.to_string().len()),
            deletions: max(&|pr| pr.deletions.to_string().len()),
        }
    }
}

fn repo_label(pr: &PrSummary) -> String {
    format!("{}#{}", pr.repo, pr.number)
}

fn pr_row(
    pr: &PrSummary,
    account: Option<String>,
    preload: Option<PreloadStatus>,
    cols: &Columns,
    width: usize,
    selected: bool,
    marked: bool,
) -> Line<'static> {
    let (glyph, glyph_color) = match (pr.is_draft, pr.state.as_str()) {
        (true, _) => ("◌", Color::DarkGray),
        (_, "MERGED") => ("●", Color::Magenta),
        (_, "CLOSED") => ("●", Color::Red),
        _ => ("●", Color::Green),
    };
    let (decision, decision_color) = match pr.review_decision.as_deref() {
        Some("APPROVED") => ("✓ approved", Color::Green),
        Some("CHANGES_REQUESTED") => ("✗ changes", Color::Red),
        Some("REVIEW_REQUIRED") => ("○ review", Color::Yellow),
        _ => ("", Color::Reset),
    };

    // Hollow while files are still being fetched, filled once opening
    // the PR is instant.
    let (preload_glyph, preload_color) = match preload {
        Some(PreloadStatus::Ready) => ("◆", Color::DarkGray),
        Some(PreloadStatus::Loading) => ("◇", Color::DarkGray),
        Some(PreloadStatus::Failed) => ("!", Color::Red),
        None => (" ", Color::Reset),
    };

    let left = vec![
        Span::styled(format!(" {glyph}"), Style::default().fg(glyph_color)),
        Span::styled(
            format!("{preload_glyph} "),
            Style::default().fg(preload_color),
        ),
        Span::styled(
            format!(
                "{:<w$} ",
                truncate(&repo_label(pr), cols.repo),
                w = cols.repo
            ),
            Style::default().fg(Color::Cyan),
        ),
    ];
    let mut right = Vec::new();
    if let Some(account) = account {
        right.push(Span::styled(format!(" [{account}]"), DIM));
    }
    right.extend([
        Span::styled(
            format!(
                "  @{:<w$}",
                truncate(&pr.author, cols.author),
                w = cols.author
            ),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(
            format!(
                "  {:>w$}",
                format!("+{}", pr.additions),
                w = cols.additions + 1
            ),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!(
                " {:<w$}",
                format!("-{}", pr.deletions),
                w = cols.deletions + 1
            ),
            Style::default().fg(Color::Red),
        ),
        Span::styled(
            format!("  {decision:<10}"),
            Style::default().fg(decision_color),
        ),
        Span::styled(format!(" {:>4} ", pr::age(&pr.updated_at)), DIM),
    ]);

    let used: usize = left.iter().chain(&right).map(Span::width).sum();
    let title = truncate(&pr.title, width.saturating_sub(used));
    let pad = width.saturating_sub(used + Span::raw(title.as_str()).width());

    let mut spans = left;
    spans.push(Span::raw(title));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    if marked {
        mark_gutter(&mut spans);
    }
    if selected {
        for span in spans.iter_mut() {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    Line::from(spans)
}

fn draw_list_footer(f: &mut Frame, list: &PrList, status: Option<String>, area: Rect) {
    let line = if let Some(input) = &list.filter_input {
        Line::from(vec![
            Span::raw(" filter: "),
            Span::raw(input.clone()),
            Span::raw("█"),
            Span::styled("  (Enter keep filter · Esc clear)", DIM),
        ])
        .style(Style::default().fg(Color::Yellow))
    } else if let Some(status) = status {
        Line::from(Span::styled(
            format!(" {status}"),
            Style::default().fg(Color::Yellow),
        ))
    } else if !list.marked.is_empty() {
        Line::from(vec![
            Span::styled(
                format!(" {} marked: ", list.marked.len()),
                Style::default().fg(Color::Magenta),
            ),
            Span::styled(
                "y copy dash list  Space mark/unmark  u unmark all  i info  Enter review  Esc back",
                DIM,
            ),
        ])
    } else {
        Line::from(Span::styled(
            " Enter review  i info  Space mark  y dash list  Tab/1-4 tab  a account  w repos  s state  d drafts  / filter  o browser  r refresh  Esc back",
            DIM,
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Status bar while reviewing a PR.
pub fn draw_review_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let Some(session) = &app.pr else {
        return;
    };
    let s = &session.summary;
    let mut spans = vec![Span::raw(format!(
        " gispect | PR {}#{} {} | @{} | {} ← {} | {} files",
        s.repo,
        s.number,
        s.title,
        session.account.label(),
        s.base_ref_name,
        s.head_ref_name,
        app.files.len()
    ))];
    if !session.drafts.is_empty() {
        spans.push(Span::styled(
            format!("  ● {} draft(s) — R to submit", session.drafts.len()),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if matches!(session.details, Load::Loading) {
        spans.push(Span::styled("  loading…", DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Draw whichever PR modal is open.
pub fn draw_pr_overlays(f: &mut Frame, app: &App, area: Rect) {
    if let Some(editor) = &app.comment_editor {
        let side = match editor.side {
            Side::Left => "old",
            Side::Right => "new",
        };
        let title = format!(
            " comment — {}:{} ({side}) ",
            crate::gh::sanitize_line(&editor.path),
            editor.line
        );
        let popup = centered(area, 72, 14);
        f.render_widget(Clear, popup);
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let [body, hint] = split_last_line(inner);
        draw_text_buffer(f, &editor.buffer.text, body);
        f.render_widget(
            Paragraph::new(" Enter newline · Ctrl-S save draft (empty removes) · Esc cancel")
                .style(DIM),
            hint,
        );
    }

    if let Some(modal) = &app.review_modal {
        let drafts = app.pr.as_ref().map_or(0, |s| s.drafts.len());
        let popup = centered(area, 76, 18);
        f.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" submit review ");
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        let mut verdicts = vec![Span::raw(" ")];
        for event in ReviewEvent::ALL {
            let enabled = modal.can_approve || event == ReviewEvent::Comment;
            let mark = if event == modal.event { "(•)" } else { "( )" };
            let style = match (event == modal.event, enabled) {
                (true, _) => Style::default()
                    .fg(verdict_color(event))
                    .add_modifier(Modifier::BOLD),
                (false, true) => Style::default(),
                (false, false) => DIM,
            };
            verdicts.push(Span::styled(format!("{mark} {}   ", event.label()), style));
        }
        if !modal.can_approve {
            verdicts.push(Span::styled("(your own PR: comment only)", DIM));
        }
        let mut header = vec![
            Line::from(verdicts),
            Line::from(Span::styled(
                format!(" {drafts} inline draft comment(s) will be included"),
                DIM,
            )),
        ];
        if modal.submitting {
            header.push(Line::from(Span::styled(
                " submitting…",
                Style::default().fg(Color::Yellow),
            )));
        } else if let Some(e) = &modal.error {
            header.push(Line::from(Span::styled(
                format!(" {e}"),
                Style::default().fg(Color::Red),
            )));
        } else {
            header.push(Line::from(""));
        }
        let header_height = (header.len() as u16).min(inner.height);
        f.render_widget(
            Paragraph::new(header),
            Rect {
                height: header_height,
                ..inner
            },
        );
        let rest = Rect {
            y: inner.y + header_height,
            height: inner.height - header_height,
            ..inner
        };
        let [body, hint] = split_last_line(rest);
        let body_block = Block::default().borders(Borders::TOP).title(" summary ");
        let body_inner = body_block.inner(body);
        f.render_widget(body_block, body);
        draw_text_buffer(f, &modal.buffer.text, body_inner);
        f.render_widget(
            Paragraph::new(" Tab verdict · Enter newline · Ctrl-S submit · Esc cancel").style(DIM),
            hint,
        );
    }

    if let (Some(scroll), Some(session)) = (app.pr_info_scroll, &app.pr) {
        let info = Info {
            summary: &session.summary,
            details: Some(&session.details),
            marked: false,
            list: false,
        };
        draw_info(f, area, &info, scroll);
    }
}

/// What the PR info popup shows.
struct Info<'a> {
    summary: &'a PrSummary,
    /// `None` until a fetch of the details has been queued.
    details: Option<&'a Load<PrDetails>>,
    marked: bool,
    /// Opened from the list: also lists the changed files and the keys.
    list: bool,
}

/// The PR info popup: title, author, branches, state, size, review state,
/// URL and description (plus the changed files, from the list).
fn draw_info(f: &mut Frame, area: Rect, info: &Info, scroll: u16) {
    let s = info.summary;
    let popup = centered(
        area,
        area.width.saturating_sub(8).min(100),
        area.height.saturating_sub(4),
    );
    f.render_widget(Clear, popup);
    let mut title = format!(" {}#{} ", s.repo, s.number);
    if info.marked {
        title.push_str("▌marked ");
    }
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [body_area, hint] = if info.list {
        split_last_line(inner)
    } else {
        [inner, Rect::default()]
    };
    let width = body_area.width as usize;

    let (decision, decision_color) = match s.review_decision.as_deref() {
        Some("APPROVED") => ("✓ approved", Color::Green),
        Some("CHANGES_REQUESTED") => ("✗ changes requested", Color::Red),
        Some("REVIEW_REQUIRED") => ("○ review required", Color::Yellow),
        _ => ("", Color::Reset),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            s.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(format!("@{}", s.author), Style::default().fg(Color::Blue)),
            Span::raw(format!(
                "  {} ← {}  {}{}  ",
                s.base_ref_name,
                s.head_ref_name,
                s.state.to_lowercase(),
                if s.is_draft { " (draft)" } else { "" }
            )),
            Span::styled(
                format!("+{}", s.additions),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(" -{}", s.deletions),
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                format!(
                    "  {} file{}",
                    s.changed_files,
                    if s.changed_files == 1 { "" } else { "s" }
                ),
                DIM,
            ),
        ]),
        Line::from(vec![
            Span::styled(decision, Style::default().fg(decision_color)),
            Span::styled(
                format!(
                    "{}updated {} ago",
                    if decision.is_empty() { "" } else { "  " },
                    pr::age(&s.updated_at)
                ),
                DIM,
            ),
        ]),
        Line::from(Span::styled(crate::gh::sanitize_line(&s.url), DIM)),
        Line::from(""),
    ];
    match info.details {
        Some(Load::Ready(d)) if d.body.trim().is_empty() => {
            lines.push(Line::from(Span::styled("(no description)", DIM)));
        }
        Some(Load::Ready(d)) => {
            lines.extend(wrap_chars(&d.body, width).into_iter().map(Line::from));
        }
        Some(Load::Failed(e)) => lines.push(Line::from(Span::styled(
            format!("(could not load the description: {e})"),
            Style::default().fg(Color::Red),
        ))),
        Some(Load::Loading) | None => {
            lines.push(Line::from(Span::styled("(loading description…)", DIM)));
        }
    }
    if let (true, Some(Load::Ready(d))) = (info.list, info.details) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("files ({})", d.files.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for file in &d.files {
            let e = &file.entry;
            let size = format!("  +{} -{}", e.additions, e.deletions);
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", e.status.marker()), DIM),
                Span::raw(truncate(
                    &crate::gh::sanitize_line(&e.path),
                    width.saturating_sub(3 + Span::raw(size.as_str()).width()),
                )),
                Span::styled(size, DIM),
            ]));
        }
    }
    let max_scroll = lines.len().saturating_sub(body_area.height as usize) as u16;
    f.render_widget(
        Paragraph::new(lines).scroll((scroll.min(max_scroll), 0)),
        body_area,
    );
    if info.list {
        f.render_widget(
            Paragraph::new(
                " j/k scroll · J/K next/prev PR · Space mark · Enter review · o browser · Esc close",
            )
            .style(DIM),
            hint,
        );
    }
}

/// The dash list popup: the copied lines and how copying went.
fn draw_dash_list(f: &mut Frame, area: Rect, dash: &pr::DashList) {
    let lines: Vec<&str> = dash.text.lines().collect();
    // Borders, copy note, blank line and hint around the list.
    let height = (lines.len() as u16).saturating_add(5);
    let popup = centered(area, area.width.saturating_sub(8).min(120), height);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" dash list ({}) ", lines.len()));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [body, hint] = split_last_line(inner);
    let width = body.width as usize;
    let (note, color) = match &dash.copied {
        Ok(msg) => (msg.as_str(), Color::Green),
        Err(e) => (e.as_str(), Color::Red),
    };
    let mut text = vec![Line::from(Span::styled(
        truncate(note, width),
        Style::default().fg(color),
    ))];
    text.push(Line::from(""));
    text.extend(lines.iter().map(|l| Line::from(truncate(l, width))));
    f.render_widget(Paragraph::new(text), body);
    f.render_widget(Paragraph::new(" y copy again · Esc close").style(DIM), hint);
}

/// Show a row as marked: a bar replaces its leading space.
pub(crate) fn mark_gutter(spans: &mut Vec<Span<'static>>) {
    if let Some(first) = spans.first_mut() {
        if let Some(rest) = first.content.strip_prefix(' ') {
            first.content = rest.to_string().into();
        }
    }
    spans.insert(0, Span::styled("▌", Style::default().fg(Color::Magenta)));
}

fn verdict_color(event: ReviewEvent) -> Color {
    match event {
        ReviewEvent::Comment => Color::Yellow,
        ReviewEvent::Approve => Color::Green,
        ReviewEvent::RequestChanges => Color::Red,
    }
}

/// Draw an editable text buffer with a cursor block at its end, scrolled
/// so the end stays visible.
fn draw_text_buffer(f: &mut Frame, text: &str, area: Rect) {
    let mut lines = wrap_chars(&format!("{text}█"), area.width as usize);
    let height = area.height as usize;
    if lines.len() > height {
        lines.drain(..lines.len() - height);
    }
    f.render_widget(
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()),
        area,
    );
}

/// Hard-wrap text to `width` columns, splitting on newlines first.
fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let line = raw.replace('\t', "    ");
        let mut current = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let w = char_width(ch);
            if used + w > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += w;
        }
        out.push(current);
    }
    out
}

fn char_width(ch: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*ch.encode_utf8(&mut buf)).width()
}

/// Cut `text` to at most `max` display columns, ending in `…` when cut.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if Span::raw(text).width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w + 1 > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// A `width`×`height` rect centered in `area` (clamped to fit).
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Split off the bottom row of `area` (for a hint line).
pub(crate) fn split_last_line(area: Rect) -> [Rect; 2] {
    let body_height = area.height.saturating_sub(1);
    [
        Rect {
            height: body_height,
            ..area
        },
        Rect {
            y: area.y + body_height,
            height: area.height.min(1),
            ..area
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_splits_newlines_and_long_lines() {
        assert_eq!(wrap_chars("abcdef\ngh", 4), vec!["abcd", "ef", "gh"]);
        assert_eq!(wrap_chars("", 4), vec![""]);
    }

    #[test]
    fn truncate_adds_ellipsis_only_when_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a longer title", 6), "a lon…");
        assert_eq!(truncate("abc", 0), "");
    }
}
