//! Rendering of the run detail screen: for each run a header line and
//! its job graph (a box per job in columns, lines to the jobs each one
//! waited for, like a run's GitHub page), and the steps of the selected
//! job below.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::actions::{ActionsView, RunDetail};
use crate::actions_ui::status_glyph;
use crate::gh::{Job, Step};
use crate::pr::Load;
use crate::pr_ui::{truncate, KeyGroup, DIM};
use crate::run_graph::{elapsed, format_duration, job_duration, Graph};

/// Keys listed by the `?` popup.
pub const KEYS: &[KeyGroup] = &[
    (
        "Move",
        &[
            ("j k ↑ ↓", "job above / below"),
            ("h l ← →", "previous / next column"),
        ],
    ),
    (
        "Run",
        &[
            ("Enter o", "open job in browser"),
            ("R", "re-run"),
            ("F", "re-run failed jobs"),
            ("X", "cancel (press twice)"),
        ],
    ),
    ("View", &[("r", "refresh"), ("Esc", "back to the tree")]),
];

/// A job box on the canvas: `(x, y, width)` and the job it shows.
type CanvasBox = ((usize, usize, usize), (usize, u64));

/// Columns between two columns of boxes, where the lines run.
const GUTTER: usize = 6;
/// Text width inside a job box.
const MIN_BOX: usize = 16;
const MAX_BOX: usize = 44;
/// Left margin of the graph.
const MARGIN: usize = 2;

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

const EDGE: Style = Style::new().fg(Color::DarkGray);

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    style: Style,
}

fn char_width(ch: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*ch.encode_utf8(&mut buf)).width()
}

/// Marks the second column of a wide character.
const WIDE_TAIL: char = '\0';

/// A grid of styled characters the graph is drawn on, then cut to the
/// visible window.
#[derive(Default)]
struct Canvas {
    rows: Vec<Vec<Cell>>,
}

impl Canvas {
    fn put(&mut self, x: usize, y: usize, ch: char, style: Style) {
        if self.rows.len() <= y {
            self.rows.resize(y + 1, Vec::new());
        }
        let row = &mut self.rows[y];
        if row.len() <= x {
            row.resize(
                x + 1,
                Cell {
                    ch: ' ',
                    style: Style::default(),
                },
            );
        }
        row[x] = Cell { ch, style };
    }

    fn get(&self, x: usize, y: usize) -> Option<Cell> {
        self.rows.get(y)?.get(x).copied()
    }

    /// Write `text` from `x`, at most `max` columns; returns the end column.
    fn text(&mut self, x: usize, y: usize, text: &str, style: Style, max: usize) -> usize {
        let mut col = x;
        for ch in text.chars() {
            let w = char_width(ch);
            if w == 0 {
                continue;
            }
            if col + w > x + max {
                break;
            }
            self.put(col, y, ch, style);
            if w == 2 {
                self.put(col + 1, y, WIDE_TAIL, style);
            }
            col += w;
        }
        col
    }

    fn spans(&mut self, x: usize, y: usize, spans: &[Span], max: usize) -> usize {
        let mut col = x;
        for span in spans {
            col = self.text(
                col,
                y,
                &span.content,
                span.style,
                (x + max).saturating_sub(col),
            );
        }
        col
    }

    /// Lines of the window `x..x+width`, `y..y+height`.
    fn window(&self, x: usize, y: usize, width: usize, height: usize) -> Vec<Line<'static>> {
        (y..y + height)
            .map(|row| {
                let cells = self.rows.get(row).map(Vec::as_slice).unwrap_or(&[]);
                let mut spans: Vec<Span> = Vec::new();
                let mut text = String::new();
                let mut style = Style::default();
                for (i, cell) in cells.iter().enumerate().skip(x).take(width) {
                    let ch = match cell.ch {
                        // A wide character cut by the left edge.
                        WIDE_TAIL if i == x => ' ',
                        WIDE_TAIL => continue,
                        ch => ch,
                    };
                    if cell.style != style && !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = cell.style;
                    text.push(ch);
                }
                if !text.is_empty() {
                    spans.push(Span::styled(text, style));
                }
                Line::from(spans)
            })
            .collect()
    }
}

/// Box-drawing character joining the given sides.
fn joint(bits: u8) -> char {
    match bits {
        b if b == LEFT | RIGHT || b == LEFT || b == RIGHT => '─',
        b if b == UP | DOWN || b == UP || b == DOWN => '│',
        b if b == DOWN | RIGHT => '╭',
        b if b == DOWN | LEFT => '╮',
        b if b == UP | RIGHT => '╰',
        b if b == UP | LEFT => '╯',
        b if b == UP | DOWN | RIGHT => '├',
        b if b == UP | DOWN | LEFT => '┤',
        b if b == LEFT | RIGHT | DOWN => '┬',
        b if b == LEFT | RIGHT | UP => '┴',
        _ => '┼',
    }
}

/// Connection bits of the line cells in the gutters.
#[derive(Default)]
struct Edges(HashMap<(usize, usize), u8>);

impl Edges {
    fn link(&mut self, a: (usize, usize), b: (usize, usize)) {
        let (da, db) = if a.0 == b.0 {
            if a.1 < b.1 {
                (DOWN, UP)
            } else {
                (UP, DOWN)
            }
        } else if a.0 < b.0 {
            (RIGHT, LEFT)
        } else {
            (LEFT, RIGHT)
        };
        *self.0.entry(a).or_default() |= da;
        *self.0.entry(b).or_default() |= db;
    }

    /// A line from the box ending at column `x0 - 1`, row `ys`, to the
    /// box starting at column `x0 + GUTTER`, row `yd`.
    fn route(&mut self, x0: usize, ys: usize, yd: usize) {
        let mid = x0 + GUTTER / 2;
        let end = x0 + GUTTER - 1;
        *self.0.entry((x0, ys)).or_default() |= LEFT;
        *self.0.entry((end, yd)).or_default() |= RIGHT;
        let mut path = Vec::new();
        path.extend((x0..=mid).map(|x| (x, ys)));
        if ys <= yd {
            path.extend((ys + 1..=yd).map(|y| (mid, y)));
        } else {
            path.extend((yd..ys).rev().map(|y| (mid, y)));
        }
        path.extend((mid + 1..=end).map(|x| (x, yd)));
        for pair in path.windows(2) {
            self.link(pair[0], pair[1]);
        }
    }
}

fn color_of(status: &str, conclusion: Option<&str>) -> Color {
    status_glyph(status, conclusion).1
}

/// Right-hand text of a job box: run time, or what it's waiting for.
fn job_note(job: &Job) -> String {
    match (job.status.as_str(), job.conclusion.as_deref()) {
        ("completed", Some("skipped")) => "skipped".to_string(),
        _ => match job_duration(job) {
            Some(secs) => format_duration(secs),
            None => job.status.replace('_', " "),
        },
    }
}

fn box_width(jobs: &[Job], column: &[usize]) -> usize {
    column
        .iter()
        .map(|&j| Span::raw(jobs[j].name.as_str()).width() + 3 + job_note(&jobs[j]).len())
        .max()
        .unwrap_or(0)
        .clamp(MIN_BOX, MAX_BOX)
}

/// Draw one job box with `inner` text columns at `(x, y)`.
fn draw_box(canvas: &mut Canvas, x: usize, y: usize, inner: usize, job: &Job, selected: bool) {
    let color = color_of(&job.status, job.conclusion.as_deref());
    let skipped = job.conclusion.as_deref() == Some("skipped");
    let mut border = Style::default().fg(if skipped { Color::DarkGray } else { color });
    let [tl, tr, bl, br, h, v] = if selected {
        border = border.add_modifier(Modifier::BOLD);
        ['┏', '┓', '┗', '┛', '━', '┃']
    } else {
        ['╭', '╮', '╰', '╯', '─', '│']
    };
    let w = inner + 2;
    canvas.put(x, y, tl, border);
    canvas.put(x + w + 1, y, tr, border);
    canvas.put(x, y + 2, bl, border);
    canvas.put(x + w + 1, y + 2, br, border);
    for i in 1..=w {
        canvas.put(x + i, y, h, border);
        canvas.put(x + i, y + 2, h, border);
    }
    canvas.put(x, y + 1, v, border);
    canvas.put(x + w + 1, y + 1, v, border);

    let (glyph, gcolor) = status_glyph(&job.status, job.conclusion.as_deref());
    let mut text = Style::default();
    if skipped {
        text = DIM;
    }
    if selected {
        text = text.add_modifier(Modifier::REVERSED);
    }
    for i in 1..=w {
        canvas.put(x + i, y + 1, ' ', text);
    }
    canvas.put(
        x + 2,
        y + 1,
        glyph.chars().next().unwrap_or(' '),
        text.fg(gcolor),
    );
    let note = job_note(job);
    let name_max = inner.saturating_sub(note.len() + 3);
    canvas.text(x + 4, y + 1, &truncate(&job.name, name_max), text, name_max);
    let note_x = x + 2 + inner - note.len();
    canvas.text(
        note_x,
        y + 1,
        &note,
        text.add_modifier(Modifier::DIM),
        note.len(),
    );
}

/// Header of one run: status, workflow, number, title, then details.
fn header_spans(view: &ActionsView, detail: &RunDetail, s: usize) -> Vec<Span<'static>> {
    let run_id = detail.runs[s];
    let data = view.repos.get(detail.repo).and_then(|r| r.ready());
    let Some((data, run)) = data.and_then(|d| Some((d, d.runs.iter().find(|r| r.id == run_id)?)))
    else {
        return vec![Span::styled(format!("run {run_id}"), DIM)];
    };
    let (glyph, color) = status_glyph(&run.status, run.conclusion.as_deref());
    let mut name = data.workflow_name(run.workflow_id).to_string();
    if name.is_empty() {
        name = "workflow".to_string();
    }
    let state = if run.is_active() {
        run.status.replace('_', " ")
    } else {
        run.conclusion.clone().unwrap_or_default()
    };
    let mut detail_text = format!("  {state} · {}", run.event);
    if let Some(branch) = &run.head_branch {
        detail_text.push_str(&format!(" · {branch}"));
    }
    if !run.head_sha.is_empty() {
        detail_text.push_str(&format!(" · {}", run.short_sha()));
    }
    detail_text.push_str(&format!(" · @{}", run.actor));
    if let Load::Ready(jobs) = &detail.jobs[s] {
        let start = jobs.iter().filter_map(|j| j.started_at.as_deref()).min();
        let end = if run.is_active() {
            None
        } else {
            jobs.iter().filter_map(|j| j.completed_at.as_deref()).max()
        };
        if let Some(secs) = elapsed(start, end) {
            detail_text.push_str(&format!(" · {}", format_duration(secs)));
        }
    }
    vec![
        Span::styled(glyph, Style::default().fg(color)),
        Span::styled(
            format!(" {name}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" #{}  ", run.run_number), DIM),
        Span::raw(run.display_title.clone()),
        Span::styled(detail_text, DIM),
    ]
}

/// Draw every run of the detail screen on a canvas; also the canvas
/// rectangle of each job box.
fn draw_canvas(view: &ActionsView, detail: &RunDetail, width: usize) -> (Canvas, Vec<CanvasBox>) {
    let mut canvas = Canvas::default();
    let mut boxes = Vec::new();
    let mut y = 0;
    for s in 0..detail.runs.len() {
        canvas.spans(
            1,
            y,
            &header_spans(view, detail, s),
            width.saturating_sub(2),
        );
        y += 2;
        let jobs = match &detail.jobs[s] {
            Load::Loading => {
                canvas.text(MARGIN, y, "loading jobs…", DIM, width);
                y += 2;
                continue;
            }
            Load::Failed(e) => {
                let msg = format!("could not load jobs: {e}");
                canvas.text(MARGIN, y, &msg, Style::default().fg(Color::Red), width);
                y += 2;
                continue;
            }
            Load::Ready(jobs) if jobs.is_empty() => {
                canvas.text(MARGIN, y, "no jobs yet", DIM, width);
                y += 2;
                continue;
            }
            Load::Ready(jobs) => jobs,
        };
        let graph: Graph = crate::run_graph::layout(jobs);
        let inner: Vec<usize> = graph.columns.iter().map(|c| box_width(jobs, c)).collect();
        let mut xs = Vec::new();
        let mut x = MARGIN;
        for w in &inner {
            xs.push(x);
            x += w + 4 + GUTTER;
        }
        let row_of = |j: usize| graph.position(j).map_or(0, |(_, r)| r);
        let mut edges = Edges::default();
        for (c, column) in graph.columns.iter().enumerate() {
            for (r, &j) in column.iter().enumerate() {
                let by = y + r * 3;
                let selected = detail.selected == Some((s, jobs[j].id));
                draw_box(&mut canvas, xs[c], by, inner[c], &jobs[j], selected);
                boxes.push(((xs[c], by, inner[c] + 4), (s, jobs[j].id)));
                if c == 0 {
                    continue;
                }
                let x0 = xs[c - 1] + inner[c - 1] + 4;
                for &k in &graph.needs[j] {
                    edges.route(x0, y + row_of(k) * 3 + 1, by + 1);
                }
            }
        }
        for (&(ex, ey), &bits) in &edges.0 {
            canvas.put(ex, ey, joint(bits), EDGE);
            // Join the line to the box borders it touches.
            if bits & LEFT != 0 && ex > 0 {
                if let Some(cell) = canvas.get(ex - 1, ey) {
                    let ch = match cell.ch {
                        '│' => '├',
                        '┃' => '┣',
                        ch => ch,
                    };
                    canvas.put(ex - 1, ey, ch, cell.style);
                }
            }
            if bits & RIGHT != 0 {
                if let Some(cell) = canvas.get(ex + 1, ey) {
                    let ch = match cell.ch {
                        '│' => '┤',
                        '┃' => '┫',
                        ch => ch,
                    };
                    canvas.put(ex + 1, ey, ch, cell.style);
                }
            }
        }
        let tallest = graph.columns.iter().map(Vec::len).max().unwrap_or(0);
        y += tallest * 3 + 1;
    }
    (canvas, boxes)
}

pub fn draw_detail(f: &mut Frame, view: &mut ActionsView, area: Rect) {
    let Some(detail) = view.detail.as_ref() else {
        return;
    };
    let title = if detail.runs.len() == 1 {
        " Run ".to_string()
    } else {
        format!(" {} runs ", detail.runs.len())
    };
    let job = detail.selected_job().cloned();
    let steps_height = job.as_ref().map_or(0, |j| {
        let want = j.steps.len().max(1) as u16 + 2;
        want.clamp(4, (area.height * 2 / 5).max(4))
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(steps_height)])
        .split(area);

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(chunks[0]);
    f.render_widget(block, chunks[0]);
    let (width, height) = (inner.width as usize, inner.height as usize);
    let (canvas, boxes) = draw_canvas(view, detail, width);

    // Keep the selected box in view; with it, the run header above the
    // first row of jobs.
    let (mut top, mut left) = detail.scroll;
    if let Some(&((bx, by, bw), _)) = boxes.iter().find(|(_, t)| Some(*t) == detail.selected) {
        let want_top = by.saturating_sub(if by >= 2 && canvas_is_header(&boxes, by) {
            2
        } else {
            1
        });
        if want_top < top {
            top = want_top;
        }
        if by + 3 > top + height {
            top = (by + 3).saturating_sub(height);
        }
        if bx < left + MARGIN {
            left = bx.saturating_sub(MARGIN);
        }
        if bx + bw > left + width {
            left = (bx + bw + MARGIN).saturating_sub(width);
        }
    }
    let max_top = canvas.rows.len().saturating_sub(height);
    top = top.min(max_top);
    f.render_widget(
        Paragraph::new(canvas.window(left, top, width, height)),
        inner,
    );

    let visible: Vec<(Rect, (usize, u64))> = boxes
        .iter()
        .filter(|((bx, by, bw), _)| {
            *by >= top && by + 3 <= top + height && *bx >= left && bx + bw <= left + width
        })
        .map(|&((bx, by, bw), target)| {
            let rect = Rect::new(
                inner.x + (bx - left) as u16,
                inner.y + (by - top) as u16,
                bw as u16,
                3,
            );
            (rect, target)
        })
        .collect();
    if let Some(detail) = view.detail.as_mut() {
        detail.scroll = (top, left);
        detail.boxes = visible;
    }

    if let Some(job) = job {
        draw_steps(f, &job, chunks[1]);
    }
}

/// Whether the box at canvas row `by` is in the first row of its graph
/// (no box sits right above it), so its run header should show too.
fn canvas_is_header(boxes: &[CanvasBox], by: usize) -> bool {
    !boxes.iter().any(|((_, y, _), _)| *y + 3 == by)
}

fn step_note(step: &Step) -> String {
    match (step.status.as_str(), step.conclusion.as_deref()) {
        ("completed", Some("skipped")) => "skipped".to_string(),
        ("completed" | "in_progress", _) => {
            elapsed(step.started_at.as_deref(), step.completed_at.as_deref())
                .map(format_duration)
                .unwrap_or_default()
        }
        (status, _) => status.replace('_', " "),
    }
}

fn draw_steps(f: &mut Frame, job: &Job, area: Rect) {
    let mut title = format!(" {} · {}", job.name, job_note(job));
    if let Some(runner) = job.runner_name.as_deref().filter(|r| !r.is_empty()) {
        title.push_str(&format!(" · {runner}"));
    } else if !job.labels.is_empty() {
        title.push_str(&format!(" · {}", job.labels.join(", ")));
    }
    title.push(' ');
    let color = color_of(&job.status, job.conclusion.as_deref());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(title, Style::default().fg(color)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = inner.width as usize;
    let height = inner.height as usize;
    if job.steps.is_empty() {
        let msg = match job.status.as_str() {
            "completed" => " no steps",
            _ => " waiting for a runner",
        };
        f.render_widget(Paragraph::new(msg).style(DIM), inner);
        return;
    }
    // Keep the step that is running (else the first failed one) in view.
    let focus = job
        .steps
        .iter()
        .position(|s| s.status != "completed")
        .or_else(|| {
            job.steps
                .iter()
                .position(|s| matches!(s.conclusion.as_deref(), Some("failure" | "cancelled")))
        })
        .unwrap_or(0);
    let offset = focus
        .saturating_sub(height / 2)
        .min(job.steps.len().saturating_sub(height));
    let lines: Vec<Line> = job
        .steps
        .iter()
        .skip(offset)
        .take(height)
        .map(|step| {
            let (glyph, color) = status_glyph(&step.status, step.conclusion.as_deref());
            let skipped = step.conclusion.as_deref() == Some("skipped");
            let name_style = if skipped { DIM } else { Style::default() };
            let note = format!("{} ", step_note(step));
            let left = format!(" {glyph} {:>2}  ", step.number);
            let used = Span::raw(left.as_str()).width() + note.len();
            let name = truncate(&step.name, width.saturating_sub(used));
            let pad = width.saturating_sub(used + Span::raw(name.as_str()).width());
            Line::from(vec![
                Span::styled(format!(" {glyph}"), Style::default().fg(color)),
                Span::styled(format!(" {:>2}  ", step.number), DIM),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(note, DIM),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

pub fn draw_footer(f: &mut Frame, status: Option<String>, area: Rect) {
    let line = match status {
        Some(status) => Line::from(Span::styled(
            format!(" {status}"),
            Style::default().fg(Color::Yellow),
        )),
        None => Line::from(Span::styled(
            " j k h l select job  Enter open job  R re-run  F failed  X cancel  r refresh  ? keys  Esc back",
            DIM,
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, name: &str, created: &str, done: Option<&str>) -> Job {
        let ts = |hms: &str| format!("2026-11-18T{hms}Z");
        Job {
            id,
            name: name.into(),
            status: if done.is_some() {
                "completed"
            } else {
                "in_progress"
            }
            .into(),
            conclusion: done.map(|_| "success".to_string()),
            created_at: ts(created),
            started_at: Some(ts(created)),
            completed_at: done.map(ts),
            run_attempt: 1,
            ..Default::default()
        }
    }

    #[test]
    fn lines_join_boxes() {
        let mut e = Edges::default();
        // Two sources (rows 1 and 4) into one target (row 1).
        e.route(10, 1, 1);
        e.route(10, 4, 1);
        let at = |x, y| joint(e.0[&(x, y)]);
        assert_eq!(at(10, 1), '─');
        assert_eq!(at(13, 1), '┬');
        assert_eq!(at(13, 4), '╯');
        assert_eq!(at(15, 1), '─');
    }

    #[test]
    fn canvas_draws_a_graph() {
        let jobs = vec![
            job(1, "lint", "08:00:00", Some("08:01:00")),
            job(2, "test", "08:00:00", Some("08:02:00")),
            job(3, "build", "08:02:01", None),
        ];
        let graph = crate::run_graph::layout(&jobs);
        let mut canvas = Canvas::default();
        let inner: Vec<usize> = graph.columns.iter().map(|c| box_width(&jobs, c)).collect();
        draw_box(&mut canvas, 0, 0, inner[0], &jobs[0], false);
        draw_box(&mut canvas, 0, 3, inner[0], &jobs[1], true);
        let lines = canvas.window(0, 0, 40, 6);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text[1].contains("✓ lint") && text[1].contains("1m 00s"),
            "{text:?}"
        );
        assert!(text[3].starts_with('┏'), "selected box is bold: {text:?}");
    }
}
