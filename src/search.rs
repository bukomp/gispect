//! Pure search logic: matching within the current diff, path filtering
//! for the file panel, and project-wide search across changed files.
//! Keep this module free of terminal/event concerns so it stays unit-testable.

use ratatui::style::Style;

use crate::types::{FileEntry, SideBySideDiff};

/// Which search prompt the user is typing into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    /// Code search within the currently selected file's diff.
    File,
    /// Code search across all changed files.
    Project,
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

/// One match from a project-wide search across changed files. Matches
/// carry the file path rather than an index: indices into `App::files`
/// can go stale while the popup is open (auto-refresh).
#[derive(Debug, Clone)]
pub struct ProjectMatch {
    pub path: String,
    /// 1-based line number in the searched version of the file.
    pub line_no: usize,
    /// True when the old side was searched (deleted files).
    pub in_old: bool,
    /// Full content of the matching line, without trailing newline.
    pub line: String,
}

/// State of the project-wide search results popup.
#[derive(Debug, Clone)]
pub struct ProjectSearch {
    pub query: String,
    pub matches: Vec<ProjectMatch>,
    /// Index into `matches` of the selected result row.
    pub selected: usize,
    /// Scroll offset of the results list.
    pub scroll: usize,
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

/// Search `query` (smart-case) across changed files. `contents[i]` is the
/// `(old, new)` content pair for `files[i]`. The new side is searched;
/// when the new side is empty and the old is not (deleted files), the old
/// side is searched with `in_old = true`. Matches are ordered by file,
/// then line number. Empty query yields no matches.
pub fn search_files(
    files: &[FileEntry],
    contents: &[(String, String)],
    query: &str,
) -> Vec<ProjectMatch> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    for (file, (old, new)) in files.iter().zip(contents.iter()) {
        if file.binary {
            continue;
        }
        let (text, in_old) = if new.is_empty() && !old.is_empty() {
            (old, true)
        } else {
            (new, false)
        };
        for (i, line) in text.lines().enumerate() {
            if !match_ranges(line, query).is_empty() {
                out.push(ProjectMatch {
                    path: file.path.clone(),
                    line_no: i + 1,
                    in_old,
                    line: line.to_string(),
                });
            }
        }
    }
    out
}

/// Aligned row index whose new-side (or old-side when `in_old`) cell has
/// line number `line_no`, if any.
pub fn row_for_line(diff: &SideBySideDiff, line_no: usize, in_old: bool) -> Option<usize> {
    diff.rows.iter().position(|row| {
        let cell = if in_old { &row.left } else { &row.right };
        cell.as_ref().map(|c| c.line_no) == Some(line_no)
    })
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
    use crate::types::{DiffRow, FileStatus, LineCell, RowKind};
    use ratatui::style::Color;

    fn cell(line_no: usize, content: &str) -> LineCell {
        LineCell {
            line_no,
            content: content.to_string(),
        }
    }

    fn file_entry(path: &str, binary: bool) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            binary,
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

    // -- search_files --

    #[test]
    fn search_files_searches_new_side_and_skips_binary() {
        let files = vec![
            file_entry("a.rs", false),
            file_entry("b.bin", true),
            file_entry("c.rs", false),
        ];
        let contents = vec![
            ("old a\n".to_string(), "needle here\nsecond needle\n".to_string()),
            ("needle".to_string(), "needle".to_string()),
            ("".to_string(), "no match\n".to_string()),
        ];
        let matches = search_files(&files, &contents, "needle");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "a.rs");
        assert_eq!(matches[0].line_no, 1);
        assert!(!matches[0].in_old);
        assert_eq!(matches[0].line, "needle here");
        assert_eq!(matches[1].path, "a.rs");
        assert_eq!(matches[1].line_no, 2);
        assert_eq!(matches[1].line, "second needle");
    }

    #[test]
    fn search_files_deleted_file_falls_back_to_old_side() {
        let files = vec![file_entry("deleted.rs", false)];
        let contents = vec![("needle here\n".to_string(), "".to_string())];
        let matches = search_files(&files, &contents, "needle");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].in_old);
        assert_eq!(matches[0].line_no, 1);
        assert_eq!(matches[0].line, "needle here");
    }

    #[test]
    fn search_files_empty_query_yields_none() {
        let files = vec![file_entry("a.rs", false)];
        let contents = vec![("".to_string(), "needle\n".to_string())];
        assert!(search_files(&files, &contents, "").is_empty());
    }

    // -- row_for_line --

    #[test]
    fn row_for_line_finds_old_and_new_sides() {
        let diff = make_diff();
        assert_eq!(row_for_line(&diff, 1, false), Some(1)); // right side line 1
        assert_eq!(row_for_line(&diff, 1, true), Some(0)); // left side line 1
        assert_eq!(row_for_line(&diff, 2, false), Some(2));
        assert_eq!(row_for_line(&diff, 99, false), None);
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
