/// Generate a unified diff between two strings, formatted like `diff -u`.
/// Uses gix-imara-diff for line-by-line comparison.
pub fn generate_diff(old: &str, new: &str, old_path: &str, new_path: &str) -> String {
    if old == new {
        return String::new();
    }

    use gix_imara_diff::{Algorithm, BasicLineDiffPrinter, Diff, InternedInput, UnifiedDiffConfig};

    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let mut out =
        format!("diff --git a/{old_path} b/{new_path}\n--- a/{old_path}\n+++ b/{new_path}\n");

    let printer = BasicLineDiffPrinter(&input.interner);
    let rendered = diff.unified_diff(&printer, UnifiedDiffConfig::default(), &input);
    out.push_str(&rendered.to_string());

    // Remove trailing newline if present for cleaner output
    if out.ends_with('\n') {
        out.pop();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_diff_returns_empty_when_no_change() {
        let result = generate_diff("same content", "same content", "f", "f");
        assert!(result.is_empty());
    }

    #[test]
    fn generate_diff_has_correct_headers() {
        let result = generate_diff("old", "new", "a/file.txt", "b/file.txt");
        assert!(result.starts_with("diff --git a/a/file.txt b/b/file.txt\n"));
        assert!(result.contains("--- a/a/file.txt"));
        assert!(result.contains("+++ b/b/file.txt"));
    }

    #[test]
    fn generate_diff_shows_addition() {
        let result = generate_diff("", "added line", "f", "f");
        assert!(result.contains("+added line"));
    }

    #[test]
    fn generate_diff_shows_deletion() {
        let result = generate_diff("removed line", "", "f", "f");
        assert!(result.contains("-removed line"));
    }

    #[test]
    fn generate_diff_shows_context_and_change() {
        let result = generate_diff("keep\nold\nkeep2", "keep\nnew\nkeep2", "f", "f");
        assert!(result.contains("-old"));
        assert!(result.contains("+new"));
        assert!(result.contains(" keep"));
        assert!(result.contains(" keep2"));
    }
}
