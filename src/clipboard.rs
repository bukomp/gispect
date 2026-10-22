//! Copy text to the system clipboard: through a platform tool when one is
//! installed (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip.exe`), else with
//! an OSC 52 escape sequence that the terminal handles.

use std::io::Write;
use std::process::{Command, Stdio};

/// Where copied text went, for the status message.
pub enum Copied {
    /// A platform clipboard tool took it.
    Tool(&'static str),
    /// Sent to the terminal as OSC 52; whether it arrived depends on the
    /// terminal (and on tmux's `set-clipboard`).
    Terminal,
}

/// Copy `text` to the clipboard.
pub fn copy(text: &str) -> std::io::Result<Copied> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let tools: &[(&'static str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("clip.exe", &[]),
    ];
    for &(tool, args) in tools {
        if tool == "wl-copy" && !wayland {
            continue;
        }
        if pipe_to(tool, args, text) {
            return Ok(Copied::Tool(tool));
        }
    }
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()?;
    Ok(Copied::Terminal)
}

/// Run `tool` with `text` on stdin; true when it exited successfully.
fn pipe_to(tool: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | ((b as u32) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }
}
