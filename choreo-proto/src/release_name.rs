//! Release-name metadata: the single source of truth for a release's dance-style
//! name (e.g. *Lindy*).
//!
//! `choreo-proto/release-name.txt` — a single line sitting next to this crate's
//! `Cargo.toml` — is that source of truth. [`RELEASE_NAME`] pulls it in with
//! [`include_str!`], so the name is baked into every binary at compile time with
//! no `build.rs` (nothing is generated or probed at build time; the literal is
//! the file's bytes). The file deliberately lives *inside* the crate directory
//! so cargo packages it into the published `.crate`: a `cargo install` from
//! crates.io bakes exactly the same name as a workspace build.
//!
//! The same file is read by CI: the `release` workflow's "Create the GitHub
//! release" step lifts it into the release title, so the binaries' `--version`
//! output and the GitHub release title can never drift.
//!
//! **Per-minor-series semantics.** The name is an attribute of a *minor series*,
//! not of an individual release: a **major or minor** bump sets a new name,
//! while **patch** releases keep the current one. The name is release metadata
//! only — it never appears in the git tag, the crate versions, or any install
//! identifier.
//!
//! An empty file means *unnamed* (the pre-name 0.1.0 series), which
//! [`release_name`] reports as `None`.

/// The raw contents of `choreo-proto/release-name.txt`, included at compile time.
pub const RELEASE_NAME: &str = include_str!("../release-name.txt");

/// Parse the raw file contents: trim surrounding whitespace; empty → `None`.
fn parse(raw: &str) -> Option<&str> {
    let name = raw.trim();
    (!name.is_empty()).then_some(name)
}

/// The release name, or `None` when unnamed (empty file).
///
/// The `'static` lifetime is required (and intended): the value is a slice of
/// the compile-time [`RELEASE_NAME`] literal, so it borrows nothing from the
/// caller.
#[must_use]
pub fn release_name() -> Option<&'static str> {
    parse(RELEASE_NAME)
}

/// The `--version`/log string for a binary running `base` version:
/// `"0.2.0 (Lindy)"` when named, otherwise `base` unchanged (e.g. `"0.1.0"`).
///
/// `base` is the binary's OWN `CARGO_PKG_VERSION` (passed by the caller) so a
/// future per-crate version cannot be misreported by this shared crate.
#[must_use]
pub fn version_string(base: &str) -> String {
    match release_name() {
        Some(name) => format!("{base} ({name})"),
        None => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trims_surrounding_whitespace() {
        assert_eq!(parse(" Lindy\n"), Some("Lindy"));
    }

    #[test]
    fn parse_empty_is_none() {
        assert_eq!(parse(""), None);
    }

    #[test]
    fn parse_whitespace_only_is_none() {
        assert_eq!(parse("   "), None);
    }

    #[test]
    fn version_string_reflects_release_name() {
        assert_eq!(
            version_string("1.2.3"),
            match release_name() {
                Some(n) => format!("1.2.3 ({n})"),
                None => "1.2.3".to_owned(),
            }
        );
    }
}
