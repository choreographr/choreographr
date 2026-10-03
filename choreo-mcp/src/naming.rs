//! Provider-safe naming for MCP tools and their catalogue groups.
//!
//! A server's own tool name is not constrained the way a provider's function
//! name is: MCP tool names may carry `.` and run to 128 characters, and a
//! server slug is an arbitrary config-file key. A provider, by contrast,
//! accepts a function name of at most [`MAX_TOOL_NAME_LEN`] characters drawn
//! from a restricted alphabet. Forwarding a server's name verbatim therefore
//! risks a rejected request (the whole tool list is invalid if one name is) or
//! a name the model cannot reliably echo.
//!
//! The naming scheme keeps the stable `mcp/<slug>/<tool>` shape but sanitizes
//! each *segment* to `[A-Za-z0-9_-]` and caps the whole name at the provider
//! limit. When sanitization or truncation would make two distinct tools
//! collide, the daemon appends a short hash of the original identity via
//! [`build_tool_name_with_suffix`]; [`build_tool_name`] on its own is
//! collision-free only for inputs that already sanitize distinctly.
//!
//! The hash is a fixed FNV-1a over the identity, implemented here rather than
//! pulled from `std` (`DefaultHasher`'s output is not guaranteed stable across
//! Rust releases) so a name a persisted transcript or a running model has seen
//! does not change when the toolchain is upgraded.

/// The longest tool name a provider accepts (`OpenAI`'s ceiling; Anthropic is
/// more generous, so 64 is the safe common limit).
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// The namespace prefix every MCP tool name carries.
pub const TOOL_NAME_PREFIX: &str = "mcp";

/// Number of hex digits in a collision/truncation suffix.
const SUFFIX_HEX_LEN: usize = 6;

/// Sanitize one identifier segment (a server slug or a tool name) to the
/// provider-safe alphabet `[A-Za-z0-9_-]`.
///
/// Every other character — including a space, a `.`, or a `/` — is replaced
/// with `_`. The mapping is many-to-one, so two distinct segments can collapse
/// to the same result; the caller resolves that with a hash suffix (see
/// [`build_tool_name_with_suffix`]).
#[must_use]
pub fn sanitize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the provider-safe tool name for `(slug, tool)`, capping it at
/// [`MAX_TOOL_NAME_LEN`].
///
/// The result is `mcp/<sanitized-slug>/<sanitized-tool>`. A name longer than
/// the cap is shortened and given a hash suffix derived from the name itself,
/// so a long tool name stays stable and distinct from its neighbours.
#[must_use]
pub fn build_tool_name(slug: &str, tool: &str) -> String {
    let base = format!(
        "{TOOL_NAME_PREFIX}/{}/{}",
        sanitize_segment(slug),
        sanitize_segment(tool)
    );
    cap_name(&base, &base)
}

/// Build the provider-safe tool name for `(slug, tool)` with a hash suffix
/// derived from `seed`, for use when [`build_tool_name`] would collide with an
/// already-registered name.
///
/// `seed` should be an identity that is unique to the tool (the originating
/// slug and tool name); the suffix then differs for every colliding pair.
#[must_use]
pub fn build_tool_name_with_suffix(slug: &str, tool: &str, seed: &str) -> String {
    let base = format!(
        "{TOOL_NAME_PREFIX}/{}/{}",
        sanitize_segment(slug),
        sanitize_segment(tool)
    );
    let with_suffix = format!("{base}-{}", short_hash(seed));
    cap_name(&with_suffix, &with_suffix)
}

/// Build the catalogue group name for `slug`: `mcp/<sanitized-slug>`.
///
/// The group is the internal `load_tools` grouping key, not a provider name,
/// so it is not length-capped.
#[must_use]
pub fn group_name(slug: &str) -> String {
    format!("{TOOL_NAME_PREFIX}/{}", sanitize_segment(slug))
}

/// Shorten `name` to [`MAX_TOOL_NAME_LEN`], appending a `-<hash>` suffix drawn
/// from `seed` when it must be cut.
///
/// The cut backs off to a UTF-8 character boundary, so a name carrying
/// multi-byte characters (which the sanitizer has already replaced, but the
/// cap also guards a pre-sanitized caller) never splits a code point.
fn cap_name(name: &str, seed: &str) -> String {
    if name.len() <= MAX_TOOL_NAME_LEN {
        return name.to_string();
    }
    let suffix = short_hash(seed);
    // The suffix plus its separating hyphen.
    let reserve = SUFFIX_HEX_LEN + 1;
    let keep = MAX_TOOL_NAME_LEN.saturating_sub(reserve);
    let mut boundary = keep.min(name.len());
    while boundary > 0 && !name.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let head = name.get(..boundary).unwrap_or(name);
    format!("{head}-{suffix}")
}

/// Lowercase-hex FNV-1a hash of `input`, truncated to six hex digits.
#[must_use]
pub fn short_hash(input: &str) -> String {
    // FNV-1a, 64-bit. `wrapping_mul` is inherent to the algorithm; the result
    // is taken modulo 2^64 by construction.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let full = format!("{hash:016x}");
    full.get(..SUFFIX_HEX_LEN).unwrap_or(&full).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_segment_replaces_disallowed_characters() {
        assert_eq!(sanitize_segment("my-tool.name"), "my-tool_name");
        assert_eq!(sanitize_segment("a b/c"), "a_b_c");
        assert_eq!(sanitize_segment("ok_name-1"), "ok_name-1");
        assert_eq!(sanitize_segment(""), "");
    }

    #[test]
    fn build_tool_name_uses_stable_format() {
        assert_eq!(build_tool_name("fixture", "echo"), "mcp/fixture/echo");
        assert_eq!(build_tool_name("my.srv", "a.b"), "mcp/my_srv/a_b");
    }

    #[test]
    fn build_tool_name_caps_long_names() {
        let long = "t".repeat(200);
        let name = build_tool_name("s", &long);
        assert!(name.len() <= MAX_TOOL_NAME_LEN, "name too long: {name}");
        assert!(name.starts_with("mcp/s/"));
        // Deterministic and distinct from a different long tool.
        assert_eq!(name, build_tool_name("s", &long));
        assert_ne!(name, build_tool_name("s", &"u".repeat(200)));
    }

    #[test]
    fn build_tool_name_with_suffix_disambiguates_collisions() {
        let a = build_tool_name("s", "a.b");
        let b = build_tool_name("s", "a_b");
        // Both sanitize to the same base, as expected.
        assert_eq!(a, b);
        // The suffix seed pulls them apart.
        let a2 = build_tool_name_with_suffix("s", "a.b", "s\u{0}a.b");
        let b2 = build_tool_name_with_suffix("s", "a_b", "s\u{0}a_b");
        assert_ne!(a2, b2);
        assert!(a2.len() <= MAX_TOOL_NAME_LEN);
    }

    #[test]
    fn group_name_sanitizes_the_slug() {
        assert_eq!(group_name("fixture"), "mcp/fixture");
        assert_eq!(group_name("my.srv"), "mcp/my_srv");
    }

    #[test]
    fn short_hash_is_stable_and_six_hex_digits() {
        let h = short_hash("hello");
        assert_eq!(h.len(), SUFFIX_HEX_LEN);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, short_hash("hello"));
        assert_ne!(h, short_hash("world"));
    }
}
