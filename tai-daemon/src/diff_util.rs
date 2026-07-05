/// Generate a unified diff between two strings, formatted like `diff -u`.
/// Uses gix-imara-diff for line-by-line comparison.
pub fn generate_diff(old: &str, new: &str, old_path: &str, new_path: &str) -> String {
    if old == new {
        return String::new();
    }

    use gix_imara_diff::{BasicLineDiffPrinter, Diff, InternedInput, Algorithm, UnifiedDiffConfig};

    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let mut out = format!("diff --git a/{old_path} b/{new_path}\n--- a/{old_path}\n+++ b/{new_path}\n");

    let printer = BasicLineDiffPrinter(&input.interner);
    let rendered = diff.unified_diff(
        &printer,
        UnifiedDiffConfig::default(),
        &input,
    );
    out.push_str(&rendered.to_string());

    // Remove trailing newline if present for cleaner output
    if out.ends_with('\n') {
        out.pop();
    }

    out
}
