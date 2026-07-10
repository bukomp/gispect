//! Pure search logic: matching within the current diff, path filtering
//! for the file panel, and content filtering of the file panel by changed
//! lines. Keep this module free of terminal/event concerns so it stays
//! unit-testable.

use std::collections::HashMap;

use ratatui::style::Style;

use crate::types::SideBySideDiff;

/// Which search prompt the user is typing into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    /// Code search within the currently selected file's diff.
    File,
    /// Filter the file panel to files whose changed lines (added/removed
    /// lines only) contain the query.
    Content,
    /// Filter the file panel by file/folder name.
    PathFilter,
}

/// An in-progress search prompt (the user is still typing).
#[derive(Debug, Clone)]
pub struct SearchInput {
    pub kind: SearchKind,
    pub query: String,
}

/// A match within the aligned diff rows of the current file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMatch {
    /// Index into `SideBySideDiff::rows`.
    pub row: usize,
    /// The old (left) cell content matched.
    pub in_left: bool,
    /// The new (right) cell content matched.
    pub in_right: bool,
}

/// A committed in-file search: highlighted matches plus n/N cursor.
#[derive(Debug, Clone)]
pub struct FileSearch {
    pub query: String,
    pub matches: Vec<RowMatch>,
    /// Index into `matches` of the current match.
    pub current: usize,
}

/// Smart-case: a query containing any ASCII uppercase letter matches
/// case-sensitively; otherwise matching is ASCII-case-insensitive.
pub fn smart_case_sensitive(query: &str) -> bool {
    query.chars().any(|c| c.is_ascii_uppercase())
}

/// Char-index ranges `(start, end)` (end exclusive) of every
/// non-overlapping occurrence of `query` in `haystack`, smart-case.
/// Empty query yields no ranges.
pub fn match_ranges(haystack: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let case_sensitive = smart_case_sensitive(query);
    let haystack_chars: Vec<char> = haystack.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    let qlen = query_chars.len();
    if qlen == 0 || haystack_chars.len() < qlen {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut i = 0usize;
    while i + qlen <= haystack_chars.len() {
        let window = &haystack_chars[i..i + qlen];
        let matches = if case_sensitive {
            window == query_chars.as_slice()
        } else {
            window
                .iter()
                .zip(query_chars.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        };
        if matches {
            ranges.push((i, i + qlen));
            i += qlen;
        } else {
            i += 1;
        }
    }
    ranges
}

/// All rows of `diff` whose left or right cell content contains `query`
/// (smart-case), in row order. Empty query yields no matches.
pub fn search_diff(diff: &SideBySideDiff, query: &str) -> Vec<RowMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (idx, row) in diff.rows.iter().enumerate() {
        let in_left = row
            .left
            .as_ref()
            .map(|c| !match_ranges(&c.content, query).is_empty())
            .unwrap_or(false);
        let in_right = row
            .right
            .as_ref()
            .map(|c| !match_ranges(&c.content, query).is_empty())
            .unwrap_or(false);
        if in_left || in_right {
            out.push(RowMatch {
                row: idx,
                in_left,
                in_right,
            });
        }
    }
    out
}

/// Whether `path` passes the file-panel filter: ASCII-case-insensitive
/// substring match on the full path (so folder names match too).
/// An empty filter matches everything.
pub fn path_matches(path: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let path_lower = path.to_ascii_lowercase();
    let filter_lower = filter.to_ascii_lowercase();
    path_lower.contains(&filter_lower)
}

/// Changed-line contents per file, parsed from unified `git diff` text.
/// Values are the contents of added and removed lines (marker char
/// stripped); keys are post-change paths (pre-change for deletions).
///
/// Known limitation: this does not handle git's quoted-path escaping for
/// paths containing unusual characters (e.g. spaces, quotes, non-ASCII
/// under `core.quotepath`); such paths are used as-is, quotes and all.
pub fn parse_changed_lines(diff: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();

    let mut pre_path: Option<String> = None;
    let mut current_key: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut in_section = false;

    // Flush the in-progress section's lines into `out`, keyed by its path.
    fn flush(
        out: &mut HashMap<String, Vec<String>>,
        key: &Option<String>,
        lines: &mut Vec<String>,
    ) {
        if let Some(k) = key {
            if !lines.is_empty() {
                out.entry(k.clone()).or_default().append(lines);
            }
        }
        lines.clear();
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let _ = rest;
            flush(&mut out, &current_key, &mut current_lines);
            pre_path = None;
            current_key = None;
            in_section = false;
            continue;
        }

        if let Some(path) = line.strip_prefix("--- ") {
            flush(&mut out, &current_key, &mut current_lines);
            current_key = None;
            in_section = false;
            pre_path = if path == "/dev/null" {
                None
            } else {
                Some(strip_ab_prefix(path))
            };
            continue;
        }

        if let Some(path) = line.strip_prefix("+++ ") {
            current_key = if path == "/dev/null" {
                pre_path.clone()
            } else {
                Some(strip_ab_prefix(path))
            };
            in_section = true;
            continue;
        }

        if !in_section {
            continue;
        }

        if line.starts_with('\\') {
            // "\ No newline at end of file" - not a changed line.
            continue;
        }

        if let Some(content) = line.strip_prefix('+') {
            current_lines.push(content.to_string());
        } else if let Some(content) = line.strip_prefix('-') {
            current_lines.push(content.to_string());
        }
        // Any other line (context, hunk headers, etc.) is ignored.
    }

    flush(&mut out, &current_key, &mut current_lines);
    out
}

/// Strip a leading `a/` or `b/` prefix from a unified-diff header path.
fn strip_ab_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// Whether any of `lines` contains `query` (smart-case). An empty query
/// matches everything, mirroring `path_matches`.
pub fn any_line_matches(lines: &[String], query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    lines.iter().any(|line| !match_ranges(line, query).is_empty())
}

/// Overlay `style` (via `Style::patch`) on every occurrence of `query`
/// within a run of styled segments, matching across segment boundaries
/// and splitting segments as needed. Char-boundary safe; smart-case.
/// Returns the segments unchanged when `query` is empty or unmatched.
pub fn highlight_segments(
    segments: &[(Style, String)],
    query: &str,
    style: Style,
) -> Vec<(Style, String)> {
    if query.is_empty() {
        return segments.to_vec();
    }

    let full: String = segments.iter().map(|(_, s)| s.as_str()).collect();
    let ranges = match_ranges(&full, query);
    if ranges.is_empty() {
        return segments.to_vec();
    }

    // Boundary offsets (char-index) marking where a matched range starts or
    // ends, so we can split segments exactly at those points.
    let mut cut_points: Vec<usize> = Vec::new();
    for (start, end) in &ranges {
        cut_points.push(*start);
        cut_points.push(*end);
    }

    let mut out = Vec::new();
    let mut char_offset = 0usize; // running char offset into `full`

    for (seg_style, seg_text) in segments {
        let seg_chars: Vec<char> = seg_text.chars().collect();
        let seg_start = char_offset;
        let seg_end = char_offset + seg_chars.len();

        // Determine local cut points that fall within this segment.
        let mut locals: Vec<usize> = cut_points
            .iter()
            .copied()
            .filter(|c| *c > seg_start && *c < seg_end)
            .map(|c| c - seg_start)
            .collect();
        locals.sort_unstable();
        locals.dedup();

        let mut pieces_start = 0usize;
        let mut piece_bounds: Vec<(usize, usize)> = Vec::new();
        for cut in locals {
            piece_bounds.push((pieces_start, cut));
            pieces_start = cut;
        }
        piece_bounds.push((pieces_start, seg_chars.len()));

        for (a, b) in piece_bounds {
            if a == b {
                continue;
            }
            let piece_text: String = seg_chars[a..b].iter().collect();
            let global_a = seg_start + a;
            let global_b = seg_start + b;
            let is_match = ranges
                .iter()
                .any(|(rs, re)| global_a >= *rs && global_b <= *re);
            let out_style = if is_match {
                seg_style.patch(style)
            } else {
                *seg_style
            };
            out.push((out_style, piece_text));
        }

        char_offset = seg_end;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DiffRow, LineCell, RowKind};
    use ratatui::style::Color;

    fn cell(line_no: usize, content: &str) -> LineCell {
        LineCell {
            line_no,
            content: content.to_string(),
            changed: Vec::new(),
        }
    }

    // -- smart_case_sensitive --

    #[test]
    fn smart_case_lowercase_query_is_insensitive() {
        assert!(!smart_case_sensitive("hello"));
        assert!(!smart_case_sensitive("héllo"));
    }

    #[test]
    fn smart_case_uppercase_query_is_sensitive() {
        assert!(smart_case_sensitive("Hello"));
        assert!(smart_case_sensitive("HELLO"));
    }

    // -- match_ranges --

    #[test]
    fn match_ranges_lowercase_query_matches_mixed_case() {
        let ranges = match_ranges("Hello World hello", "hello");
        assert_eq!(ranges, vec![(0, 5), (12, 17)]);
    }

    #[test]
    fn match_ranges_uppercase_query_is_exact() {
        let ranges = match_ranges("Hello hello HELLO", "Hello");
        assert_eq!(ranges, vec![(0, 5)]);
    }

    #[test]
    fn match_ranges_empty_query_yields_none() {
        assert_eq!(match_ranges("hello", ""), Vec::new());
    }

    #[test]
    fn match_ranges_adjacent_non_overlapping_occurrences() {
        // "aa" in "aaaa" should yield two non-overlapping matches, not three
        // overlapping ones.
        let ranges = match_ranges("aaaa", "aa");
        assert_eq!(ranges, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn match_ranges_non_ascii_haystack_uses_char_indices() {
        let haystack = "héllo wörld héllo";
        let ranges = match_ranges(haystack, "héllo");
        assert_eq!(ranges, vec![(0, 5), (12, 17)]);
        // Verify char-index slicing lines up with the actual text.
        let chars: Vec<char> = haystack.chars().collect();
        for (s, e) in &ranges {
            let slice: String = chars[*s..*e].iter().collect();
            assert_eq!(slice, "héllo");
        }
    }

    // -- search_diff --

    fn make_diff() -> SideBySideDiff {
        SideBySideDiff {
            rows: vec![
                DiffRow {
                    left: Some(cell(1, "left only match")),
                    right: None,
                    kind: RowKind::Removed,
                },
                DiffRow {
                    left: None,
                    right: Some(cell(1, "right only match")),
                    kind: RowKind::Added,
                },
                DiffRow {
                    left: Some(cell(2, "match on both sides")),
                    right: Some(cell(2, "match on both sides")),
                    kind: RowKind::Modified,
                },
                DiffRow {
                    left: Some(cell(3, "no hit here")),
                    right: Some(cell(3, "no hit here")),
                    kind: RowKind::Context,
                },
            ],
        }
    }

    #[test]
    fn search_diff_finds_left_only_right_only_and_both() {
        let diff = make_diff();
        let matches = search_diff(&diff, "match");
        assert_eq!(matches.len(), 3);
        assert_eq!(
            matches[0],
            RowMatch {
                row: 0,
                in_left: true,
                in_right: false
            }
        );
        assert_eq!(
            matches[1],
            RowMatch {
                row: 1,
                in_left: false,
                in_right: true
            }
        );
        assert_eq!(
            matches[2],
            RowMatch {
                row: 2,
                in_left: true,
                in_right: true
            }
        );
    }

    #[test]
    fn search_diff_empty_query_yields_none() {
        let diff = make_diff();
        assert!(search_diff(&diff, "").is_empty());
    }

    // -- path_matches --

    #[test]
    fn path_matches_folder_segment() {
        assert!(path_matches("src/app.rs", "src/"));
        assert!(path_matches("src/app.rs", "app"));
        assert!(!path_matches("src/app.rs", "lib"));
    }

    #[test]
    fn path_matches_case_insensitive() {
        assert!(path_matches("src/App.rs", "app"));
        assert!(path_matches("src/app.rs", "APP"));
    }

    #[test]
    fn path_matches_empty_filter_matches_everything() {
        assert!(path_matches("anything.rs", ""));
    }

    // -- parse_changed_lines --

    const MULTI_FILE_DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1234567..89abcde 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,7 +10,8 @@ fn foo() {
 context line
-    old code
+    new code
+    x += 1;
diff --git a/new.rs b/new.rs
new file mode 100644
index 0000000..abc1234
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn added() {}
+// new file
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index abc1234..0000000
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn removed() {}
-// bye
diff --git a/img.png b/img.png
index abc1234..def5678 100644
Binary files a/img.png and b/img.png differ
diff --git a/nonl.rs b/nonl.rs
index abc1234..def5678 100644
--- a/nonl.rs
+++ b/nonl.rs
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";

    #[test]
    fn parse_changed_lines_modified_file_strips_only_marker_char() {
        let parsed = parse_changed_lines(MULTI_FILE_DIFF);
        let lib = parsed.get("src/lib.rs").expect("src/lib.rs should have changed lines");
        assert_eq!(
            lib,
            &vec![
                "    old code".to_string(),
                "    new code".to_string(),
                "    x += 1;".to_string(),
            ]
        );
    }

    #[test]
    fn parse_changed_lines_added_file_keyed_by_new_path() {
        let parsed = parse_changed_lines(MULTI_FILE_DIFF);
        let new_file = parsed.get("new.rs").expect("new.rs should have changed lines");
        assert_eq!(
            new_file,
            &vec!["fn added() {}".to_string(), "// new file".to_string()]
        );
    }

    #[test]
    fn parse_changed_lines_deleted_file_keyed_by_pre_change_path() {
        let parsed = parse_changed_lines(MULTI_FILE_DIFF);
        let gone = parsed.get("gone.rs").expect("gone.rs should have changed lines");
        assert_eq!(
            gone,
            &vec!["fn removed() {}".to_string(), "// bye".to_string()]
        );
    }

    #[test]
    fn parse_changed_lines_binary_file_produces_no_entry() {
        let parsed = parse_changed_lines(MULTI_FILE_DIFF);
        assert!(parsed.get("img.png").is_none());
    }

    #[test]
    fn parse_changed_lines_headers_and_hunk_markers_not_captured() {
        let parsed = parse_changed_lines(MULTI_FILE_DIFF);
        let lib = parsed.get("src/lib.rs").unwrap();
        // Neither the "--- a/..." / "+++ b/..." header lines nor the
        // "@@ ... @@" hunk header should show up as changed-line content.
        assert!(!lib.iter().any(|l| l.contains("@@")));
        assert!(!lib.iter().any(|l| l.starts_with("- a/") || l.starts_with("+ b/")));
        assert!(!lib.contains(&"context line".to_string()));
    }

    #[test]
    fn parse_changed_lines_skips_no_newline_marker() {
        let parsed = parse_changed_lines(MULTI_FILE_DIFF);
        let nonl = parsed.get("nonl.rs").expect("nonl.rs should have changed lines");
        assert_eq!(nonl, &vec!["old".to_string(), "new".to_string()]);
    }

    #[test]
    fn parse_changed_lines_empty_diff_yields_empty_map() {
        assert!(parse_changed_lines("").is_empty());
    }

    // -- any_line_matches --

    #[test]
    fn any_line_matches_smart_case() {
        let lines = vec!["Hello World".to_string(), "other".to_string()];
        assert!(any_line_matches(&lines, "hello"));
        assert!(any_line_matches(&lines, "Hello"));
        assert!(!any_line_matches(&lines, "Goodbye"));
        // Smart-case: uppercase query is case-sensitive and should not
        // match a lowercase-only occurrence.
        assert!(!any_line_matches(&lines, "OTHER"));
    }

    #[test]
    fn any_line_matches_empty_query_matches_everything() {
        let lines = vec!["anything".to_string()];
        assert!(any_line_matches(&lines, ""));
        let empty: Vec<String> = Vec::new();
        assert!(any_line_matches(&empty, ""));
    }

    #[test]
    fn any_line_matches_no_lines_no_match() {
        let empty: Vec<String> = Vec::new();
        assert!(!any_line_matches(&empty, "needle"));
    }

    // -- highlight_segments --

    #[test]
    fn highlight_segments_splits_across_segment_boundary() {
        // "hello" split across two segments as "hel" + "lo world"; query
        // "hello" spans the boundary.
        let segments = vec![
            (Style::default().fg(Color::Red), "hel".to_string()),
            (Style::default().fg(Color::Blue), "lo world".to_string()),
        ];
        let highlight = Style::default().bg(Color::Yellow);
        let result = highlight_segments(&segments, "hello", highlight);

        // Total text must be unchanged.
        let joined: String = result.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(joined, "hello world");

        // Every piece that is part of "hello" should carry the patched
        // background; " world" should not.
        let mut consumed = 0usize;
        for (style, text) in &result {
            let is_within_hello = consumed < 5;
            if is_within_hello {
                assert_eq!(style.bg, Some(Color::Yellow), "piece {text:?} should be highlighted");
            } else {
                assert_eq!(style.bg, None, "piece {text:?} should not be highlighted");
            }
            consumed += text.chars().count();
        }
    }

    #[test]
    fn highlight_segments_empty_query_returns_unchanged() {
        let segments = vec![(Style::default(), "hello".to_string())];
        let result = highlight_segments(&segments, "", Style::default().bg(Color::Yellow));
        assert_eq!(result, segments);
    }

    #[test]
    fn highlight_segments_no_match_returns_unchanged() {
        let segments = vec![(Style::default(), "hello".to_string())];
        let result = highlight_segments(&segments, "xyz", Style::default().bg(Color::Yellow));
        assert_eq!(result, segments);
    }
}
