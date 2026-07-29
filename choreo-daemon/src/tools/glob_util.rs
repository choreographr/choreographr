use std::path::Path;
use zlob::{ZlobFlags, ZlobPattern};

/// A compiled glob pattern with a flag indicating whether to match against
/// the file's basename (filename only) or the full path.
///
/// Following gitignore convention:
/// - Patterns **without** a `/` are matched against the file's basename,
///   so `*.log` matches `.log` files at any directory depth.
/// - Patterns **with** a `/` are matched against the full path starting
///   from the working directory.
pub(crate) struct GlobFilter {
    pattern: ZlobPattern,
    match_basename: bool,
}

impl GlobFilter {
    /// Compile a glob string into a `GlobFilter`.
    ///
    /// The `match_basename` flag is determined automatically:
    /// `true` when the pattern contains no `/`, `false` otherwise.
    pub(crate) fn compile(glob: &str) -> Result<Self, String> {
        let match_basename = !glob.contains('/');
        // PERIOD is added so `**` matches through dot-prefixed directories
        // (e.g. temp dirs, `.hidden`) — the walker controls which entries are
        // visible; the glob should match whatever the walker yields.
        let pattern = ZlobPattern::compile(glob, ZlobFlags::RECOMMENDED | ZlobFlags::PERIOD)
            .map_err(|e| format!("invalid glob pattern '{glob}': {e}"))?;
        Ok(Self {
            pattern,
            match_basename,
        })
    }

    /// Returns `true` if the file at `path` matches this glob filter.
    ///
    /// When `match_basename` is `true`, only the file's name (last component)
    /// is tested against the pattern. Otherwise the full path is used.
    pub(crate) fn matches(&self, path: &Path) -> bool {
        let name = if self.match_basename {
            // file_name() returns None only for the root path "/",
            // which the walker never yields. Return false defensively.
            match path.file_name() {
                Some(name) => name.to_string_lossy(),
                None => return false,
            }
        } else {
            path.to_string_lossy()
        };
        self.pattern.matches_default(name.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn compile_basic_glob() {
        let filter = GlobFilter::compile("*.rs").expect("valid glob");
        assert!(filter.match_basename);
    }

    #[test]
    fn compile_pattern_with_separator_uses_full_path() {
        let filter = GlobFilter::compile("src/*.rs").expect("valid glob");
        assert!(!filter.match_basename);
    }

    #[test]
    fn compile_accepts_all_globs() {
        // zlob's compile is infallible for valid UTF-8 (only errors on OOM).
        // Verify that common patterns succeed.
        assert!(GlobFilter::compile("foo.*").is_ok());
        assert!(GlobFilter::compile("**/a/**").is_ok());
        assert!(GlobFilter::compile("[abc]").is_ok());
        assert!(GlobFilter::compile("?oo").is_ok());
    }

    #[test]
    fn compile_empty_glob_succeeds() {
        // An empty glob is technically valid in zlob (matches empty string).
        let filter = GlobFilter::compile("").expect("empty glob");
        assert!(filter.match_basename);
    }

    #[test]
    fn compile_pattern_with_only_separator() {
        let filter = GlobFilter::compile("/").expect("single slash pattern");
        assert!(!filter.match_basename);
    }

    #[test]
    fn basename_matches_file_at_root() {
        let filter = GlobFilter::compile("data.txt").expect("valid glob");
        // match_basename is true since pattern has no '/'
        assert!(filter.matches(Path::new("/some/dir/data.txt")));
        assert!(filter.matches(Path::new("data.txt")));
    }

    #[test]
    fn basename_does_not_match_different_name() {
        let filter = GlobFilter::compile("data.txt").expect("valid glob");
        assert!(!filter.matches(Path::new("/some/dir/other.txt")));
        assert!(!filter.matches(Path::new("other.txt")));
    }

    #[test]
    fn basename_wildcard_matches_at_any_depth() {
        let filter = GlobFilter::compile("*.log").expect("valid glob");
        assert!(filter.matches(Path::new("/a/b/c/foo.log")));
        assert!(filter.matches(Path::new("foo.log")));
        assert!(!filter.matches(Path::new("/a/b/c/foo.txt")));
    }

    #[test]
    fn full_path_pattern() {
        // zlob's * matches /, so */src/*.rs matches files under any src/ directory.
        let filter = GlobFilter::compile("*/src/*.rs").expect("valid glob");
        assert!(filter.matches(Path::new("/workspace/src/main.rs")));
        assert!(!filter.matches(Path::new("/workspace/tests/main.rs")));
    }

    #[test]
    fn full_path_pattern_matches_subdir() {
        // zlob's * matches /, so */src/*.rs matches
        // any file under any src/ directory at any depth.
        let filter = GlobFilter::compile("*/src/*.rs").expect("valid glob");
        assert!(filter.matches(Path::new("/workspace/src/sub/mod.rs")));
    }

    #[test]
    fn matches_root_path_returns_false() {
        // Regression: file_name() returns None for root path "/".
        let filter = GlobFilter::compile("*").expect("valid glob");
        assert!(!filter.matches(Path::new("/")));
    }

    #[test]
    fn basename_pattern_with_dot() {
        let filter = GlobFilter::compile(".hidden").expect("valid glob");
        assert!(filter.matches(Path::new("/dir/.hidden")));
        assert!(!filter.matches(Path::new("/dir/file")));
    }

    #[test]
    fn pattern_with_path_sep_and_wildcard() {
        let filter = GlobFilter::compile("**/*.txt").expect("valid glob");
        assert!(!filter.match_basename);
        // zlob ** matches all path components
        assert!(filter.matches(Path::new("/a/b/c/file.txt")));
        assert!(filter.matches(Path::new("/file.txt")));
    }
}
