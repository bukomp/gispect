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
    }
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
                    rows.push(DiffRow {
                        left: Some(cell(&old_lines, old_start + i)),
                        right: Some(cell(&new_lines, new_start + i)),
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
}
