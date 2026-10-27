//! Startup splash: a scan beam decodes the half-block wordmark while
//! red and green ghosts (diff colors) converge on it, then the rows tear
//! once and the logo settles. Plays for 0.7 seconds.

use std::io;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;

/// Total splash length.
pub const DURATION: Duration = Duration::from_millis(700);

/// Delay between frames.
const FRAME: Duration = Duration::from_millis(16);

/// Wordmark bitmap. A pixel is one cell wide and half a cell tall, so
/// each pair of rows renders as one line of half-block glyphs.
const LOGO: [&str; 9] = [
    "......#...........................#..",
    "..................................#..",
    "####..#..####..####..####..####..####",
    "#..#..#..#.....#..#..#..#..#......#..",
    "#..#..#..####..#..#..####..#......#..",
    "#..#..#.....#..#..#..#.....#......#..",
    "####..#..####..####..####..####...###",
    "...#...........#.....................",
    "####...........#.....................",
];
const LOGO_W: u16 = 37;
const LOGO_H: u16 = (LOGO.len() as u16).div_ceil(2);

/// The dot of the `i`, left green in the settled logo like a cursor.
const DOT: (i32, i32) = (6, 0);

/// The beam crosses the logo in this many milliseconds.
const SWEEP_MS: f32 = 420.0;
/// Columns of decode noise trailing the beam.
const BAND: f32 = 5.0;
/// Columns drawn around the logo, so torn rows and ghosts aren't clipped.
const MARGIN: i32 = 3;
const NOISE: &[char] = &['0', '1', '<', '>', '/', '\\', '+', '-', '=', '#'];

/// Play the splash on its own thread so the app can load meanwhile.
/// Expects raw mode and the alternate screen to be on. Any key skips to
/// the last frame, which stays up until the caller clears the screen.
pub fn spawn() -> JoinHandle<()> {
    thread::spawn(|| {
        let _ = play();
        // Dropping the splash terminal shows the cursor again.
        let _ = execute!(io::stdout(), cursor::Hide);
    })
}

fn play() -> io::Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed().min(DURATION);
        let ms = elapsed.as_secs_f32() * 1000.0;
        terminal.draw(|f| draw(f, ms))?;
        if elapsed >= DURATION {
            return Ok(());
        }
        if event::poll(FRAME)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let end = DURATION.as_secs_f32() * 1000.0;
                    terminal.draw(|f| draw(f, end))?;
                    return Ok(());
                }
            }
        }
    }
}

fn draw(f: &mut ratatui::Frame, ms: f32) {
    let area = f.area();
    render(f.buffer_mut(), area, ms);
}

/// Draw the splash frame at `ms` milliseconds, centered in `area`.
/// Draws nothing when the logo doesn't fit.
pub fn render(buf: &mut Buffer, area: Rect, ms: f32) {
    if area.width < LOGO_W || area.height < LOGO_H {
        return;
    }
    let ox = (area.x + (area.width - LOGO_W) / 2) as i32;
    let oy = area.y + (area.height - LOGO_H) / 2;

    // Cubic ease-out: the beam snaps in, then slows to read the tail.
    let sweep = 1.0 - (1.0 - (ms / SWEEP_MS).min(1.0)).powi(3);
    let beam = sweep * (LOGO_W as f32 + BAND);
    let frame = Frame {
        ms,
        reveal: beam - BAND,
        split: match ms {
            ms if ms < 280.0 => 2,
            ms if ms < 532.0 => 1,
            _ => 0,
        },
    };

    for cy in 0..LOGO_H as i32 {
        for cx in -MARGIN..LOGO_W as i32 + MARGIN {
            let x = ox + cx;
            if x < area.left() as i32 || x >= area.right() as i32 {
                continue;
            }
            let pos = (x as u16, oy + cy as u16);
            let decoding = (cx as f32) >= frame.reveal && (cx as f32) < beam;
            let cell = if decoding && (lit(cx, 2 * cy) || lit(cx, 2 * cy + 1)) {
                let i = hash(cx as u32, cy as u32, (ms / 16.0) as u32) as usize;
                Some((NOISE[i % NOISE.len()], Style::default().fg(Color::Green)))
            } else {
                half_block(frame.paint(cx, 2 * cy), frame.paint(cx, 2 * cy + 1))
            };
            if let (Some((ch, style)), Some(c)) = (cell, buf.cell_mut(pos)) {
                c.set_char(ch).set_style(style);
            }
        }
    }

    // The beam itself, one row taller than the logo on each side.
    let bx = beam as i32;
    if sweep < 1.0 && (0..LOGO_W as i32).contains(&bx) {
        let style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let top = oy.saturating_sub(1).max(area.top());
        let bottom = (oy + LOGO_H + 1).min(area.bottom());
        for y in top..bottom {
            if let Some(c) = buf.cell_mut(((ox + bx) as u16, y)) {
                c.set_char('┃').set_style(style);
            }
        }
    }
}

/// Animation state shared by every pixel of one frame.
struct Frame {
    ms: f32,
    /// Logo columns left of this are decoded.
    reveal: f32,
    /// Offset of the red (left) and green (right) ghosts, in pixels.
    split: i32,
}

impl Frame {
    /// Color of logo pixel (`x`, `y`) after tearing and ghosting.
    fn paint(&self, x: i32, y: i32) -> Option<Color> {
        let x = x - self.tear(y);
        let shown = |x: i32| lit(x, y) && (x as f32) < self.reveal;
        if shown(x) {
            Some(if (x, y) == DOT && self.split == 0 {
                Color::Green
            } else {
                Color::White
            })
        } else if shown(x + self.split) {
            Some(Color::Red)
        } else if shown(x - self.split) {
            Some(Color::Green)
        } else {
            None
        }
    }

    /// Horizontal shift of pixel row `y`: two quick scanline tears.
    fn tear(&self, y: i32) -> i32 {
        match (self.ms, y) {
            (ms, 2 | 3) if (420.0..476.0).contains(&ms) => 3,
            (ms, 6 | 7) if (420.0..476.0).contains(&ms) => -2,
            (ms, 4 | 5) if (476.0..532.0).contains(&ms) => -3,
            (ms, 8) if (476.0..532.0).contains(&ms) => 2,
            _ => 0,
        }
    }
}

fn lit(x: i32, y: i32) -> bool {
    let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
        return false;
    };
    LOGO.get(y)
        .and_then(|row| row.as_bytes().get(x))
        .is_some_and(|&b| b == b'#')
}

/// Merge a top and bottom pixel into one half-block cell.
fn half_block(top: Option<Color>, bottom: Option<Color>) -> Option<(char, Style)> {
    let style = Style::default();
    match (top, bottom) {
        (None, None) => None,
        (Some(t), None) => Some(('▀', style.fg(t))),
        (None, Some(b)) => Some(('▄', style.fg(b))),
        (Some(t), Some(b)) if t == b => Some(('█', style.fg(t))),
        (Some(t), Some(b)) => Some(('▀', style.fg(t).bg(b))),
    }
}

/// Cheap deterministic noise, so frames don't need an RNG.
fn hash(a: u32, b: u32, c: u32) -> u32 {
    let mut h =
        a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77) ^ c.wrapping_mul(0xC2B2_AE3D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^ (h >> 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|row| !row.is_empty())
            .collect()
    }

    #[test]
    fn logo_rows_match_width() {
        assert!(LOGO.iter().all(|row| row.len() == LOGO_W as usize));
        assert!(lit(DOT.0, DOT.1));
    }

    #[test]
    fn last_frame_is_the_clean_wordmark() {
        let area = Rect::new(0, 0, 41, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, 700.0);
        assert_eq!(
            text(&buf),
            [
                "        ▀                           █",
                "  █▀▀█  █  █▀▀▀  █▀▀█  █▀▀█  █▀▀▀  ▀█▀▀",
                "  █  █  █  ▀▀▀█  █  █  █▀▀▀  █      █",
                "  ▀▀▀█  ▀  ▀▀▀▀  █▀▀▀  ▀▀▀▀  ▀▀▀▀   ▀▀▀",
                "  ▀▀▀▀           ▀",
            ]
        );
        assert_eq!(buf[(8, 1)].fg, Color::Green);
        assert_eq!(buf[(2, 2)].fg, Color::White);
    }

    #[test]
    fn every_frame_fits_any_size() {
        for (w, h) in [(0, 0), (10, 3), (37, 5), (38, 6), (120, 40)] {
            let area = Rect::new(0, 0, w, h);
            for ms in (0..=700).step_by(8) {
                let mut buf = Buffer::empty(area);
                render(&mut buf, area, ms as f32);
            }
        }
    }

    #[test]
    fn first_frame_shows_no_logo_yet() {
        let area = Rect::new(0, 0, 41, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, 0.0);
        assert!(text(&buf).iter().all(|row| !row.contains('█')));
    }
}
