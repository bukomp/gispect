//! Language-aware syntax highlighting for diff pane content, backed by
//! `syntect`. Loaded once at startup and reused for every highlight call.

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Highlighted segments for a whole file: one entry per line (index 0 =
/// line 1), each entry the line's styled segments.
pub type HlLines = Vec<Vec<(Style, String)>>;

/// Cache key for a highlight result: the path (drives syntax selection)
/// plus the exact content.
pub fn cache_key(path: &str, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    text.hash(&mut hasher);
    hasher.finish()
}

/// Maximum input size we'll attempt to highlight, to keep the UI responsive.
const MAX_BYTES: usize = 1024 * 1024;
/// Maximum number of lines we'll attempt to highlight.
const MAX_LINES: usize = 20_000;

/// Wraps syntect's syntax set + theme, loaded once at startup.
pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        // bat's extended syntax set (via two-face): covers TypeScript, TSX,
        // TOML, Vue, Svelte, Dockerfile and much more that syntect's bundled
        // Sublime defaults lack.
        let syntax_set = two_face::syntax::extra_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-eighties.dark")
            .cloned()
            .unwrap_or_else(|| {
                theme_set
                    .themes
                    .values()
                    .next()
                    .cloned()
                    .expect("syntect default themes should not be empty")
            });
        Highlighter { syntax_set, theme }
    }

    /// Highlight `text` as the language inferred from `path`. Returns one
    /// entry per line (index 0 = line 1); each entry is the line's styled
    /// segments. Segment strings contain no trailing '\n'.
    ///
    /// Returns an empty Vec if `text` is too large to highlight quickly, or
    /// for any line whose highlighting fails, falls back to a single
    /// unstyled segment for just that line.
    pub fn highlight(&self, path: &str, text: &str) -> HlLines {
        if text.len() > MAX_BYTES {
            return Vec::new();
        }
        let line_count = text.lines().count();
        if line_count > MAX_LINES {
            return Vec::new();
        }

        let ext = extension(path);
        let syntax = self
            .syntax_set
            .find_syntax_by_extension(ext)
            .or_else(|| {
                extension_alias(ext)
                    .and_then(|alias| self.syntax_set.find_syntax_by_extension(alias))
            })
            .or_else(|| {
                text.lines()
                    .next()
                    .and_then(|first| self.syntax_set.find_syntax_by_first_line(first))
            })
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut result = Vec::new();

        for line in LinesWithEndings::from(text) {
            let stripped = line.trim_end_matches(['\n', '\r']);
            match highlighter.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    let segments = ranges
                        .into_iter()
                        .map(|(style, s)| {
                            let s = s.trim_end_matches(['\n', '\r']).to_string();
                            (to_ratatui_style(style), s)
                        })
                        .collect();
                    result.push(segments);
                }
                Err(_) => {
                    result.push(vec![(Style::default(), stripped.to_string())]);
                }
            }
        }

        result
    }
}

/// Fallback mapping for extensions the syntax set doesn't know directly.
fn extension_alias(ext: &str) -> Option<&'static str> {
    match ext {
        "jsx" | "mjs" | "cjs" => Some("js"),
        "mts" | "cts" => Some("ts"),
        _ => None,
    }
}

fn extension(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

fn to_ratatui_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distinct_colors(h: &Highlighter, path: &str, text: &str) -> usize {
        let segs = h.highlight(path, text);
        let mut colors = std::collections::HashSet::new();
        for line in &segs {
            for (s, _) in line {
                colors.insert(format!("{:?}", s.fg));
            }
        }
        colors.len()
    }

    /// Real tokenization produces multiple foreground colors; a language
    /// falling back to plain text produces exactly one (the theme default).
    #[test]
    fn common_languages_are_tokenized() {
        let h = Highlighter::new();
        let cases = [
            ("f.rs", "fn f() -> u8 { return 1; } // c\n"),
            ("f.ts", "const f = (x: number): string => `v${x}`; // c\n"),
            ("f.tsx", "const A = () => <div className=\"x\">{y}</div>;\n"),
            ("f.jsx", "const A = () => <div className=\"x\">{y}</div>;\n"),
            ("f.py", "def g(x): return None  # c\n"),
            ("f.go", "func f() int { return 1 } // c\n"),
            ("f.toml", "[table]\nkey = \"value\"\n"),
            ("f.vue", "<template><div :class=\"x\">{{ y }}</div></template>\n"),
        ];
        for (path, text) in cases {
            assert!(
                distinct_colors(&h, path, text) > 1,
                "{path} was not tokenized (single color)"
            );
        }
    }
}
