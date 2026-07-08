/// Classifies a single line in a unified diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Unchanged context line (prefixed with ` `).
    Context,
    /// Line present in the new version but not the old (prefixed with `+`).
    Addition,
    /// Line present in the old version but not the new (prefixed with `-`).
    Deletion,
}

/// A single line in a diff hunk, with its kind and text content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
}

/// A contiguous block of changed lines in a diff, bracketed by an `@@ ... @@` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The `@@ -a,b +c,d @@` header line.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// Represents the full diff for one file, containing one or more hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<DiffHunk>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_line_kind_debug_and_eq() {
        assert_eq!(DiffLineKind::Context, DiffLineKind::Context);
        assert_eq!(DiffLineKind::Addition, DiffLineKind::Addition);
        assert_eq!(DiffLineKind::Deletion, DiffLineKind::Deletion);
        assert_ne!(DiffLineKind::Context, DiffLineKind::Addition);
    }

    #[test]
    fn diff_line_construction() {
        let line = DiffLine {
            kind: DiffLineKind::Addition,
            content: "+added".into(),
        };
        assert_eq!(line.kind, DiffLineKind::Addition);
        assert_eq!(line.content, "+added");
    }

    #[test]
    fn diff_hunk_construction() {
        let hunk = DiffHunk {
            header: "@@ -1,3 +1,4 @@".into(),
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    content: "same".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    content: "old".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    content: "new".into(),
                },
            ],
        };
        assert_eq!(hunk.header, "@@ -1,3 +1,4 @@");
        assert_eq!(hunk.lines.len(), 3);
    }

    #[test]
    fn file_diff_round_trip() {
        let fd = FileDiff {
            old_path: "a/src/main.rs".into(),
            new_path: "b/src/main.rs".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "x".into(),
                }],
            }],
        };
        assert_eq!(fd.old_path, "a/src/main.rs");
        assert_eq!(fd.new_path, "b/src/main.rs");
        assert_eq!(fd.hunks.len(), 1);
    }

    #[test]
    fn file_diff_empty_hunks() {
        let fd = FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: vec![],
        };
        assert!(fd.hunks.is_empty());
    }
}
