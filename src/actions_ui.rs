//! Rendering for the GitHub Actions view: the repository → workflow →
//! run tree, the run (dispatch) modal and the group start modal.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::actions::{ActionsView, BatchModal, Dispatch, RepoData, RowKey, RunModal, WorkflowNode};
use crate::app::App;
use crate::gh::WorkflowRun;
use crate::pr::{self, Load};
use crate::pr_ui::{centered, mark_gutter, split_last_line, truncate, DIM};
use crate::workflow::InputKind;

/// Widest workflow name column.
const MAX_NAME: usize = 40;

/// The full-screen actions view: status line, tree, footer, modal.
pub fn draw_actions(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let status = app.status.as_ref().map(|(s, _)| s.clone());
    let Some(view) = app.actions.as_mut() else {
        return;
    };
    draw_status(f, view, chunks[0]);
    draw_tree(f, view, chunks[1]);
    draw_footer(f, view, status, chunks[2]);
    if let Some(modal) = &view.modal {
        draw_modal(f, modal, area);
    }
    if let Some(modal) = &view.batch_modal {
        draw_batch_modal(f, modal, area);
    }
}

fn draw_status(f: &mut Frame, view: &ActionsView, area: Rect) {
    let mut text = " gispect | actions".to_string();
    if view.multi {
        text.push_str(&format!(" | {} repos", view.repos.len()));
    } else {
        let repo = &view.repos[0];
        match (repo.ready(), &repo.remote) {
            (Some(d), _) => text.push_str(&format!(" | {} as {}", d.full_name, d.account.label())),
            (None, Some((_, name))) => text.push_str(&format!(" | {name}")),
            (None, None) => text.push_str(&format!(" | {}", repo.name)),
        }
        text.push_str(&format!(" | {}", repo.branch));
    }
    match &view.accounts {
        Load::Loading => text.push_str(" | loading accounts…"),
        Load::Failed(_) => text.push_str(" | gh unavailable"),
        Load::Ready(_) => {}
    }
    let running: usize = view
        .repos
        .iter()
        .filter_map(|r| r.ready())
        .map(RepoData::active_runs)
        .sum();
    if running > 0 {
        text.push_str(&format!(" | running: {running}"));
    }
    if view.running_only {
        text.push_str(" | running only");
    }
    if let Some(filter) = &view.text_filter {
        text.push_str(&format!(" | filter: {filter}"));
    }
    if !view.marked.is_empty() {
        text.push_str(&format!(" | marked: {}", view.marked.len()));
    }
    f.render_widget(Paragraph::new(text), area);
}

fn draw_tree(f: &mut Frame, view: &mut ActionsView, area: Rect) {
    let title = if view.running_only {
        " Actions · running "
    } else {
        " Actions "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Load::Failed(e) = &view.accounts {
        let lines = vec![
            Line::from(Span::styled(
                format!("could not list gh accounts: {e}"),
                Style::default().fg(Color::Red),
            )),
            Line::from(Span::styled("run `gh auth login`, then press r", DIM)),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        view.view_area = Rect::default();
        return;
    }

    let rows = view.rows();
    let height = inner.height as usize;
    let selected = view.selected_index(&rows);
    let offset = crate::ui::window_offset(selected, rows.len(), height);
    if rows.is_empty() {
        let loading = view
            .repos
            .iter()
            .any(|r| r.remote.is_some() && matches!(r.data, Load::Loading));
        let msg = if view.active_filter().is_some() {
            " (nothing matches the filter)"
        } else if view.running_only && loading {
            " (loading…)"
        } else if view.running_only {
            " (no runs in progress; a: show all)"
        } else {
            " (no repositories)"
        };
        f.render_widget(Paragraph::new(msg).style(DIM), inner);
    } else {
        let width = inner.width as usize;
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(i, key)| {
                let mut line = row_line(view, *key, width);
                if view.marked.contains(key) {
                    mark_gutter(&mut line.spans);
                }
                if i == selected {
                    for span in line.spans.iter_mut() {
                        span.style = span.style.add_modifier(Modifier::REVERSED);
                    }
                }
                line
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }
    view.view_area = inner;
    view.view_offset = offset;
}

/// Status glyph and color of a run.
pub fn run_glyph(run: &WorkflowRun) -> (&'static str, Color) {
    match run.status.as_str() {
        "completed" => {}
        "in_progress" => return ("●", Color::Yellow),
        _ => return ("◌", Color::Yellow),
    }
    match run.conclusion.as_deref() {
        Some("success") => ("✓", Color::Green),
        Some("failure" | "timed_out" | "startup_failure") => ("✗", Color::Red),
        Some("cancelled" | "skipped") => ("⊘", Color::DarkGray),
        Some("action_required") => ("!", Color::Yellow),
        _ => ("○", Color::DarkGray),
    }
}

fn glyph_span(run: Option<&WorkflowRun>) -> Span<'static> {
    match run {
        Some(run) => {
            let (glyph, color) = run_glyph(run);
            Span::styled(glyph, Style::default().fg(color))
        }
        None => Span::styled("·", DIM),
    }
}

fn row_line(view: &ActionsView, key: RowKey, width: usize) -> Line<'static> {
    let repo = &view.repos[key.repo()];
    // Repository rows only exist with several repositories; everything
    // under them is indented one level.
    let indent = if view.multi { "   " } else { " " };
    let data = repo.ready();
    let left: Vec<Span> = match key {
        RowKey::Repo(_) => {
            let arrow = if repo.expanded || view.running_only {
                "▾"
            } else {
                "▸"
            };
            let mut spans = vec![
                Span::raw(format!(" {arrow} ")),
                Span::styled(
                    repo.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some((_, full_name)) = &repo.remote {
                spans.push(Span::styled(
                    format!("  {full_name}"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            spans.push(Span::styled(format!("  {}", repo.branch), DIM));
            match (data, repo.note()) {
                (Some(d), None) => {
                    spans.push(Span::raw("  "));
                    // One glyph per workflow: its latest run.
                    spans.extend(
                        d.workflows
                            .iter()
                            .take(24)
                            .map(|w| glyph_span(d.latest_run(w.workflow.id))),
                    );
                    spans.push(Span::styled(
                        format!(
                            "  {} workflow{}",
                            d.workflows.len(),
                            if d.workflows.len() == 1 { "" } else { "s" }
                        ),
                        DIM,
                    ));
                    let running = d.active_runs();
                    if running > 0 {
                        spans.push(Span::styled(
                            format!("  ● {running} running"),
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                }
                // An open repository shows the note on its own row.
                (_, Some(note)) if !view.repo_open(key.repo()) => spans.push(note_span(
                    matches!(repo.data, Load::Failed(_)),
                    format!("  {note}"),
                )),
                _ => {}
            }
            spans
        }
        RowKey::Note(_) => {
            let note = repo.note().unwrap_or_default();
            vec![note_span(
                matches!(repo.data, Load::Failed(_)),
                format!("{indent}{note}"),
            )]
        }
        RowKey::Workflow(_, id) => {
            match data.and_then(|d| Some((d, d.workflows.iter().find(|w| w.workflow.id == id)?))) {
                Some((d, w)) => workflow_spans(
                    view.workflow_open(key.repo(), id),
                    indent,
                    d,
                    w,
                    name_width(d),
                ),
                None => Vec::new(),
            }
        }
        RowKey::Run(_, _, run_id) => {
            match data.and_then(|d| d.runs.iter().find(|r| r.id == run_id)) {
                Some(run) => return run_line(run, indent, width),
                None => Vec::new(),
            }
        }
        RowKey::NoRuns(..) => {
            let msg = match data.and_then(|d| d.runs_error.as_ref()) {
                Some(e) => format!("{indent}    could not load runs: {e}"),
                None => format!("{indent}    no recent runs"),
            };
            vec![note_span(false, msg)]
        }
    };
    pad_to(left, width)
}

fn note_span(failed: bool, text: String) -> Span<'static> {
    if failed {
        Span::styled(text, Style::default().fg(Color::Red))
    } else {
        Span::styled(text, DIM)
    }
}

/// Width of the workflow name column of one repository.
fn name_width(data: &RepoData) -> usize {
    data.workflows
        .iter()
        .map(|w| Span::raw(w.workflow.name.as_str()).width())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME)
}

fn workflow_spans(
    open: bool,
    indent: &str,
    data: &RepoData,
    w: &WorkflowNode,
    name_width: usize,
) -> Vec<Span<'static>> {
    let latest = data.latest_run(w.workflow.id);
    let arrow = if open { "▾" } else { "▸" };
    // GitHub-managed workflows (Copilot, Dependabot, Pages) recede.
    let name_style = if w.workflow.is_file() {
        Style::default()
    } else {
        DIM
    };
    let mut spans = vec![
        Span::raw(format!("{indent}{arrow} ")),
        glyph_span(latest),
        Span::styled(
            format!(" {:<name_width$}  ", truncate(&w.workflow.name, name_width)),
            name_style,
        ),
    ];
    if !w.workflow.is_active() {
        spans.push(Span::styled("disabled ", DIM));
    } else if w.runnable() {
        spans.push(Span::styled("▶ run    ", Style::default().fg(Color::Green)));
    } else if let Dispatch::Unknown(_) = w.dispatch {
        spans.push(Span::styled(
            "? inputs ",
            Style::default().fg(Color::Yellow),
        ));
    } else {
        spans.push(Span::raw("         "));
    }
    if let Some(run) = latest {
        let branch = run.head_branch.as_deref().unwrap_or("");
        spans.push(Span::styled(
            format!(
                "#{} {branch} · {}",
                run.run_number,
                pr::age(&run.created_at)
            ),
            DIM,
        ));
    }
    spans
}

/// A run row: glyph, number and title, then right-aligned event, branch,
/// actor and age.
fn run_line(run: &WorkflowRun, indent: &str, width: usize) -> Line<'static> {
    let (glyph, color) = run_glyph(run);
    let left = vec![
        Span::raw(format!("{indent}    ")),
        Span::styled(glyph, Style::default().fg(color)),
        Span::styled(format!(" #{:<5} ", run.run_number), DIM),
    ];
    let mut detail = run.event.clone();
    if let Some(branch) = &run.head_branch {
        detail.push_str(&format!(" · {branch}"));
    }
    detail.push_str(&format!(
        " · @{} · {:>4} ",
        run.actor,
        pr::age(&run.created_at)
    ));
    let right = Span::styled(format!("  {detail}"), DIM);
    let used: usize = left.iter().map(Span::width).sum::<usize>() + right.width();
    let title = truncate(&run.display_title, width.saturating_sub(used));
    let pad = width.saturating_sub(used + Span::raw(title.as_str()).width());
    let mut spans = left;
    spans.push(Span::raw(title));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(right);
    Line::from(spans)
}

/// Cut or pad spans to exactly `width` columns, so a selected row's
/// highlight spans the whole line.
fn pad_to(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = span.width();
        if used + w > width {
            out.push(Span::styled(
                truncate(&span.content, width - used),
                span.style,
            ));
            used = width;
            break;
        }
        used += w;
        out.push(span);
    }
    out.push(Span::raw(" ".repeat(width - used)));
    Line::from(out)
}

fn draw_footer(f: &mut Frame, view: &ActionsView, status: Option<String>, area: Rect) {
    let line = if let Some(input) = &view.filter_input {
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
    } else if !view.marked.is_empty() {
        Line::from(vec![
            Span::styled(
                format!(" {} marked: ", view.marked.len()),
                Style::default().fg(Color::Magenta),
            ),
            Span::styled(
                "s start  R re-run  F re-run failed  X cancel  Space mark/unmark  u unmark all  a running  Esc back",
                DIM,
            ),
        ])
    } else {
        Line::from(Span::styled(
            " Enter run/open  Space mark  ←→ fold  e fold all  a running  s start  R re-run  F re-run failed  X cancel  o browser  / filter  r refresh  Esc back",
            DIM,
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_modal(f: &mut Frame, modal: &RunModal, area: Rect) {
    let label_width = modal
        .inputs
        .iter()
        .map(|i| Span::raw(i.name.as_str()).width() + usize::from(i.required))
        .max()
        .unwrap_or(0)
        .clamp(3, 24);
    // Title, blank, fields, blank, description, error, warning, hint.
    let fields = 1 + modal.inputs.len();
    let height = (fields as u16).saturating_add(9);
    let popup = centered(area, 76, height);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" run workflow ");
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [body, hint] = split_last_line(inner);
    let width = body.width as usize;

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {}", modal.workflow_name),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", modal.full_name),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(format!("  as {}", modal.account), DIM),
        ]),
        Line::from(""),
    ];

    // Keep the focused field on screen when there are many inputs.
    let room = (body.height as usize).saturating_sub(6).max(1);
    let first = modal.field.saturating_sub(room - 1);
    for field in (0..fields).skip(first).take(room) {
        let focused = field == modal.field;
        let (label, value) = match field {
            0 => ("ref".to_string(), text_value(&modal.git_ref, focused)),
            i => {
                let input = &modal.inputs[i - 1];
                let value = &modal.values[i - 1];
                let star = if input.required { "*" } else { "" };
                let shown = match &input.kind {
                    InputKind::Boolean => if value == "true" { "[x]" } else { "[ ]" }.to_string(),
                    InputKind::Choice(options) if !options.is_empty() => {
                        format!("‹ {value} ›  {}", options.join(" | "))
                    }
                    _ => text_value(value, focused),
                };
                (format!("{}{star}", input.name), shown)
            }
        };
        let marker = if focused { "▸" } else { " " };
        let style = if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), style),
            Span::styled(
                format!("{:<label_width$}  ", truncate(&label, label_width)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(&value, width.saturating_sub(label_width + 5)),
                style,
            ),
        ]));
    }
    lines.push(Line::from(""));

    let help = match modal.focused_input() {
        None => "branch or tag to run on".to_string(),
        Some(input) => {
            let kind = match &input.kind {
                InputKind::String => "text",
                InputKind::Boolean => "boolean",
                InputKind::Number => "number",
                InputKind::Choice(_) => "choice",
                InputKind::Environment => "environment",
            };
            if input.description.is_empty() {
                kind.to_string()
            } else {
                format!("{} ({kind})", input.description)
            }
        }
    };
    lines.push(Line::from(Span::styled(
        format!(" {}", truncate(&help, width.saturating_sub(1))),
        DIM,
    )));
    if modal.submitting {
        lines.push(Line::from(Span::styled(
            " starting…",
            Style::default().fg(Color::Yellow),
        )));
    } else if let Some(e) = &modal.error {
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate(e, width.saturating_sub(1))),
            Style::default().fg(Color::Red),
        )));
    }
    if !modal.can_push {
        lines.push(Line::from(Span::styled(
            format!(" {} may not have write access here", modal.account),
            Style::default().fg(Color::Yellow),
        )));
    }
    f.render_widget(Paragraph::new(lines), body);
    f.render_widget(
        Paragraph::new(" Tab/↑↓ field · Space/←→ toggle · type to edit · Enter run · Esc cancel")
            .style(DIM),
        hint,
    );
}

/// Confirmation listing the marked workflows to start and those skipped.
fn draw_batch_modal(f: &mut Frame, modal: &BatchModal, area: Rect) {
    let rows = modal.runs.len() + modal.skipped.len() + usize::from(!modal.skipped.is_empty());
    let height = (rows as u16).saturating_add(5);
    let popup = centered(area, 90, height);
    f.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title(format!(
        " start {} workflow{} ",
        modal.runs.len(),
        if modal.runs.len() == 1 { "" } else { "s" }
    ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [body, hint] = split_last_line(inner);
    let width = body.width as usize;
    let name_width = modal
        .runs
        .iter()
        .map(|r| Span::raw(r.name.as_str()).width())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME);

    let mut lines = vec![
        Line::from(Span::styled(
            " with default inputs, on each repository's checked-out branch",
            DIM,
        )),
        Line::from(""),
    ];
    for run in &modal.runs {
        let line = vec![
            Span::styled(" ▶ ", Style::default().fg(Color::Green)),
            Span::styled(
                format!("{:<name_width$}  ", truncate(&run.name, name_width)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(run.full_name.clone(), Style::default().fg(Color::Cyan)),
            Span::styled(format!("  on {}", run.git_ref), DIM),
        ];
        lines.push(pad_to(line, width));
    }
    if !modal.skipped.is_empty() {
        lines.push(Line::from(Span::styled(" skipped:", DIM)));
        for why in &modal.skipped {
            lines.push(Line::from(Span::styled(
                format!("   {}", truncate(why, width.saturating_sub(3))),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), body);
    f.render_widget(
        Paragraph::new(" Enter start all · Esc cancel").style(DIM),
        hint,
    );
}

fn text_value(value: &str, focused: bool) -> String {
    if focused {
        format!("{value}█")
    } else {
        value.to_string()
    }
}
