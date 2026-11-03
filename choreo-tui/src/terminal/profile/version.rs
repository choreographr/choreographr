//! The dotted/packed version triple the capability table's floors are compared
//! against.

use std::fmt;

/// One dotted version triple, parsed from an environment value that carries
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// Build a triple directly (used for the capability table's version
    /// floors).
    #[must_use]
    pub(crate) const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse a dotted `MAJOR[.MINOR[.PATCH]]` value.
    ///
    /// A component's trailing non-digit suffix is dropped, so `3.5a` (tmux's
    /// style) and `1.2.0` both parse. A value whose first component has no
    /// leading digits (`unknown`, `a.b`) is malformed and yields `None`, so the
    /// caller records "version unknown" instead of inventing one; a malformed
    /// *later* component only truncates the triple (`3.x` is 3.0.0), which is
    /// still a truthful lower bound for a version floor.
    #[must_use]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut components = text.trim().split('.');
        let major = leading_number(components.next()?)?;
        let minor = components.next().and_then(leading_number);
        // A malformed component ends the triple: reading a later component past
        // it would turn `3.x.1` into `3.0.1`, claiming a patch release the value
        // never named.
        let patch = minor
            .and_then(|_| components.next())
            .and_then(leading_number);
        Some(Self::new(major, minor.unwrap_or(0), patch.unwrap_or(0)))
    }

    /// Decode the packed decimal integer some terminals export instead of a
    /// dotted string: the last two digits are the patch, the two before those
    /// the minor, and the rest the major — so `KONSOLE_VERSION=260400` is 26.04
    /// and `VTE_VERSION=7400` is 0.74.
    #[must_use]
    pub(crate) const fn from_packed(packed: u32) -> Self {
        Self::new(packed / 10_000, (packed / 100) % 100, packed % 100)
    }

    /// Whether this version is at or above `floor`.
    ///
    /// Spelled out rather than derived from `PartialOrd` so the comparison is
    /// usable from the `const` literals of the capability table and states the
    /// ordering exactly once.
    #[must_use]
    pub(crate) const fn at_least(self, floor: Self) -> bool {
        if self.major != floor.major {
            return self.major > floor.major;
        }
        if self.minor != floor.minor {
            return self.minor > floor.minor;
        }
        self.patch >= floor.patch
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The leading decimal run of one dotted component, or `None` when it has none
/// or does not fit a `u32` (not a version we can compare, so no answer is
/// better than a wrapped one).
fn leading_number(component: &str) -> Option<u32> {
    component
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Parse a version value that may be dotted (`26.04.0`) or the packed decimal
/// integer `KONSOLE_VERSION` / `VTE_VERSION` use (`260400`, `7400`).
///
/// Shared with [`super::detect`], which is where the two forms actually arrive.
pub(crate) fn parse_packed_or_dotted(raw: &str) -> Option<Version> {
    if raw.contains('.') {
        Version::parse(raw)
    } else {
        raw.parse::<u32>().ok().map(Version::from_packed)
    }
}
