//! Line-level side-by-side diff engine built on top of the `similar` crate.

use crate::types::{DiffRow, LineCell, RowKind, SideBySideDiff};
use similar::{DiffTag, TextDiff};

/// Strip a single trailing `\n` (and a preceding `\r`) from a line.
fn strip_newline(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(line)
}

fn cell(lines: &[&str], idx: usize) -> LineCell {
    LineCell {
        line_no: idx + 1,
        content: strip_newline(lines[idx]).to_string(),
        changed: Vec::new(),
    }
}

/// Merge adjacent/contiguous ranges (where the next range's start equals
/// the previous range's end) in place.
fn merge_adjacent(ranges: &mut Vec<(usize, usize)>) {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges.iter() {
        if let Some(last) = merged.last_mut() {
            if start == last.1 {
                last.1 = end;
                continue;
            }
        }
        merged.push((start, end));
    }
    *ranges = merged;
}

/// Word-level intra-line diff of a Modified pair: returns the changed
/// char ranges of (old_line, new_line). Empty vecs mean "treat the
/// whole line as changed".
fn inline_ranges(old: &str, new: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let old_chars = old.chars().count();
    let new_chars = new.chars().count();

    if old_chars == 0 || new_chars == 0 || old_chars > 2000 || new_chars > 2000 {
        return (Vec::new(), Vec::new());
    }

    let word_diff = TextDiff::from_words(old, new);

    let mut old_ranges: Vec<(usize, usize)> = Vec::new();
    let mut new_ranges: Vec<(usize, usize)> = Vec::new();
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;

    for change in word_diff.iter_all_changes() {
        let len = change.value().chars().count();
        match change.tag() {
            similar::ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
            similar::ChangeTag::Delete => {
                old_ranges.push((old_pos, old_pos + len));
                old_pos += len;
            }
            similar::ChangeTag::Insert => {
                new_ranges.push((new_pos, new_pos + len));
                new_pos += len;
            }
        }
    }

    merge_adjacent(&mut old_ranges);
    merge_adjacent(&mut new_ranges);

    let old_changed: usize = old_ranges.iter().map(|(s, e)| e - s).sum();
    let new_changed: usize = new_ranges.iter().map(|(s, e)| e - s).sum();

    let old_ratio = old_changed as f64 / old_chars as f64;
    let new_ratio = new_changed as f64 / new_chars as f64;

    if old_ratio > 0.7 || new_ratio > 0.7 {
        return (Vec::new(), Vec::new());
    }

    (old_ranges, new_ranges)
}

/// Build an aligned side-by-side diff of two file contents.
pub fn side_by_side(old: &str, new: &str) -> SideBySideDiff {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    let diff = TextDiff::from_lines(old, new);

    let mut rows = Vec::new();

    for op in diff.ops() {
        let old_range = op.old_range();
        let new_range = op.new_range();

        match op.tag() {
            DiffTag::Equal => {
                for (oi, ni) in old_range.zip(new_range) {
                    rows.push(DiffRow {
                        left: Some(cell(&old_lines, oi)),
                        right: Some(cell(&new_lines, ni)),
                        kind: RowKind::Context,
                    });
                }
            }
            DiffTag::Delete => {
                for oi in old_range {
                    rows.push(DiffRow {
                        left: Some(cell(&old_lines, oi)),
                        right: None,
                        kind: RowKind::Removed,
                    });
                }
            }
            DiffTag::Insert => {
                for ni in new_range {
                    rows.push(DiffRow {
                        left: None,
                        right: Some(cell(&new_lines, ni)),
                        kind: RowKind::Added,
                    });
                }
            }
            DiffTag::Replace => {
                let old_start = old_range.start;
                let new_start = new_range.start;
                let old_len = old_range.len();
                let new_len = new_range.len();
                let common = old_len.min(new_len);

                for i in 0..common {
                    let mut left = cell(&old_lines, old_start + i);
                    let mut right = cell(&new_lines, new_start + i);
                    let (old_changed, new_changed) = inline_ranges(&left.content, &right.content);
                    left.changed = old_changed;
                    right.changed = new_changed;

                    rows.push(DiffRow {
                        left: Some(left),
                        right: Some(right),
                        kind: RowKind::Modified,
                    });
                }

                for oi in (old_start + common)..(old_start + old_len) {
                    rows.push(DiffRow {
                        left: Some(cell(&old_lines, oi)),
                        right: None,
                        kind: RowKind::Removed,
                    });
                }

                for ni in (new_start + common)..(new_start + new_len) {
                    rows.push(DiffRow {
                        left: None,
                        right: Some(cell(&new_lines, ni)),
                        kind: RowKind::Added,
                    });
                }
            }
        }
    }

    SideBySideDiff { rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RowKind;

    #[test]
    fn pure_insert() {
        let old = "a\nb\n";
        let new = "a\nb\nc\nd\n";
        let diff = side_by_side(old, new);

        assert_eq!(diff.rows.len(), 4);
        assert_eq!(diff.rows[0].kind, RowKind::Context);
        assert_eq!(diff.rows[1].kind, RowKind::Context);
        assert_eq!(diff.rows[2].kind, RowKind::Added);
        assert_eq!(diff.rows[3].kind, RowKind::Added);

        let added: Vec<&str> = diff.rows[2..]
            .iter()
            .map(|r| r.right.as_ref().unwrap().content.as_str())
            .collect();
        assert_eq!(added, vec!["c", "d"]);
        for row in &diff.rows[2..] {
            assert!(row.left.is_none());
        }
    }

    #[test]
    fn pure_delete() {
        let old = "a\nb\nc\n";
        let new = "a\n";
        let diff = side_by_side(old, new);

        assert_eq!(diff.rows.len(), 3);
        assert_eq!(diff.rows[0].kind, RowKind::Context);
        assert_eq!(diff.rows[1].kind, RowKind::Removed);
        assert_eq!(diff.rows[2].kind, RowKind::Removed);

        assert_eq!(diff.rows[1].left.as_ref().unwrap().content, "b");
        assert_eq!(diff.rows[2].left.as_ref().unwrap().content, "c");
        for row in &diff.rows[1..] {
            assert!(row.right.is_none());
        }
    }

    #[test]
    fn replace_block_with_unequal_lengths() {
        // old has 2 lines replaced by 4 new lines: first 2 pair up as
        // Modified, remaining 2 become Added.
        let old = "x\none\ntwo\ny\n";
        let new = "x\nONE\nTWO\nTHREE\nFOUR\ny\n";
        let diff = side_by_side(old, new);

        // context "x", modified "one"/"ONE", modified "two"/"TWO",
        // added "THREE", added "FOUR", context "y"
        assert_eq!(diff.rows.len(), 6);
        assert_eq!(diff.rows[0].kind, RowKind::Context);

        assert_eq!(diff.rows[1].kind, RowKind::Modified);
        assert_eq!(diff.rows[1].left.as_ref().unwrap().content, "one");
        assert_eq!(diff.rows[1].right.as_ref().unwrap().content, "ONE");

        assert_eq!(diff.rows[2].kind, RowKind::Modified);
        assert_eq!(diff.rows[2].left.as_ref().unwrap().content, "two");
        assert_eq!(diff.rows[2].right.as_ref().unwrap().content, "TWO");

        assert_eq!(diff.rows[3].kind, RowKind::Added);
        assert!(diff.rows[3].left.is_none());
        assert_eq!(diff.rows[3].right.as_ref().unwrap().content, "THREE");

        assert_eq!(diff.rows[4].kind, RowKind::Added);
        assert!(diff.rows[4].left.is_none());
        assert_eq!(diff.rows[4].right.as_ref().unwrap().content, "FOUR");

        assert_eq!(diff.rows[5].kind, RowKind::Context);
    }

    #[test]
    fn context_preserves_line_numbers() {
        let old = "one\ntwo\nthree\n";
        let new = "one\nTWO\nthree\n";
        let diff = side_by_side(old, new);

        // "one" is context at line 1/1, "two"->"TWO" is modified at 2/2,
        // "three" is context but at line 3/3.
        assert_eq!(diff.rows.len(), 3);

        let first = &diff.rows[0];
        assert_eq!(first.kind, RowKind::Context);
        assert_eq!(first.left.as_ref().unwrap().line_no, 1);
        assert_eq!(first.right.as_ref().unwrap().line_no, 1);
        assert_eq!(first.left.as_ref().unwrap().content, "one");

        let last = &diff.rows[2];
        assert_eq!(last.kind, RowKind::Context);
        assert_eq!(last.left.as_ref().unwrap().line_no, 3);
        assert_eq!(last.right.as_ref().unwrap().line_no, 3);
        assert_eq!(last.left.as_ref().unwrap().content, "three");
    }

    #[test]
    fn modified_pair_has_intra_line_changes() {
        let old = "let x = foo(1);\n";
        let new = "let x = bar(1);\n";
        let diff = side_by_side(old, new);

        assert_eq!(diff.rows.len(), 1);
        let row = &diff.rows[0];
        assert_eq!(row.kind, RowKind::Modified);

        let left = row.left.as_ref().unwrap();
        let right = row.right.as_ref().unwrap();

        assert!(!left.changed.is_empty());
        assert!(!right.changed.is_empty());

        let right_chars: Vec<char> = right.content.chars().collect();
        let full_len = right_chars.len();
        let mut covered = vec![false; full_len];
        for &(start, end) in &right.changed {
            assert!(end <= full_len);
            for c in covered.iter_mut().take(end).skip(start) {
                *c = true;
            }
        }
        assert!(!covered.iter().all(|&c| c));

        let bar_start = right.content.find("bar").unwrap();
        let bar_range: Vec<usize> = (bar_start..bar_start + 3).collect();
        assert!(bar_range.iter().all(|&i| covered[i]));
    }

    #[test]
    fn completely_rewritten_pair_has_no_intra_line_changes() {
        let old = "aaaa bbbb cccc\n";
        let new = "x y z q r\n";
        let diff = side_by_side(old, new);

        assert_eq!(diff.rows.len(), 1);
        let row = &diff.rows[0];
        assert_eq!(row.kind, RowKind::Modified);

        assert!(row.left.as_ref().unwrap().changed.is_empty());
        assert!(row.right.as_ref().unwrap().changed.is_empty());
    }

    #[test]
    fn unicode_ranges_are_char_indexed_and_safe() {
        let old = "let s = \"héllo wörld\";\n";
        let new = "let s = \"héllo wörms\";\n";
        let diff = side_by_side(old, new);

        assert_eq!(diff.rows.len(), 1);
        let row = &diff.rows[0];
        assert_eq!(row.kind, RowKind::Modified);

        let left = row.left.as_ref().unwrap();
        let right = row.right.as_ref().unwrap();

        let left_len = left.content.chars().count();
        let right_len = right.content.chars().count();

        for &(start, end) in &left.changed {
            assert!(start <= end);
            assert!(left_len >= end);
        }
        for &(start, end) in &right.changed {
            assert!(start <= end);
            assert!(right_len >= end);
        }

        let _: Vec<char> = left
            .content
            .chars()
            .enumerate()
            .filter(|(i, _)| left.changed.iter().any(|&(s, e)| *i >= s && *i < e))
            .map(|(_, c)| c)
            .collect();
        let _: Vec<char> = right
            .content
            .chars()
            .enumerate()
            .filter(|(i, _)| right.changed.iter().any(|&(s, e)| *i >= s && *i < e))
            .map(|(_, c)| c)
            .collect();
    }

    #[test]
    fn non_modified_rows_have_empty_changed() {
        let old = "a\nb\nc\n";
        let new = "a\nX\nc\nd\n";
        let diff = side_by_side(old, new);

        for row in &diff.rows {
            if row.kind != RowKind::Modified {
                if let Some(left) = &row.left {
                    assert!(left.changed.is_empty());
                }
                if let Some(right) = &row.right {
                    assert!(right.changed.is_empty());
                }
            }
        }
    }
}
