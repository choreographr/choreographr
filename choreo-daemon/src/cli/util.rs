//! Utility subcommands that expose an identity or relocate an install:
//! `pubkey` and `migrate`.
//!
//! Both are one-shot, offline operations — they never open the daemon state,
//! the DB, or a listener.

use anyhow::Context;
use choreo_transport::key::pubkey_base64;

/// This machine's transport public key as canonical base64 (44 characters):
/// the value `pubkey` prints.
///
/// Read-only by construction — [`load_transport_keypair`] derives the public
/// half from the identity the daemon wrote at startup and NEVER generates one,
/// so running this in an empty config dir reports the absence rather than
/// minting an identity that no daemon would ever serve. The secret is dropped
/// (zeroized) as soon as the public half is derived.
///
/// [`load_transport_keypair`]: choreo_transport::key::load_transport_keypair
///
/// # Errors
///
/// Returns an error when this config dir holds no transport identity, when the
/// secret is unreadable or does not decode, or when no config directory
/// resolves.
pub(super) fn pubkey_value() -> anyhow::Result<String> {
    use choreo_transport::key::load_transport_keypair;
    let (secret, public) = load_transport_keypair().context(
        "no transport identity in this config dir yet — start the daemon once to create it",
    )?;
    // Dropped here, not held: only the public half leaves this function, and
    // `TransportSecretKey` zeroizes its bytes on drop.
    drop(secret);
    Ok(pubkey_base64(&public))
}

/// Run the `migrate` subcommand, resolving the target base dir from
/// `--base-dir`/`CHOREOGRAPHR_BASE_DIR`.
///
/// # Errors
///
/// Returns an error when no base dir is configured, or when the migration
/// itself fails.
pub(super) fn run_migrate(move_: bool, dry_run: bool, force: bool) -> anyhow::Result<()> {
    let base = choreo_shared::paths::base_dir()
        .context("migrate requires --base-dir <PATH> (or CHOREOGRAPHR_BASE_DIR)")?;
    crate::migrate::run(&base, move_, dry_run, force)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pubkey` prints exactly the canonical base64 of the derived public key
    /// (44 chars, no newline in the value), and — with no identity present —
    /// reports the absence instead of generating one.
    #[test]
    fn pubkey_value_prints_derived_key_and_never_generates() {
        use base64::Engine as _;
        use choreo_transport::key::{ensure_transport_keypair, set_test_config_root};

        let dir = tempfile::tempdir().unwrap();
        let sec_path = dir.path().join("choreographr").join("transport.sec");
        // The keypair dir override is thread-local, so this redirects keypair
        // resolution on THIS test's thread only; nextest isolates the process.
        set_test_config_root(Some(dir.path().to_path_buf()));

        // No identity yet: an error, and — the point of the read-only accessor —
        // nothing was created by the query.
        let missing = pubkey_value();
        assert!(
            missing.is_err(),
            "a query must not mint the identity it prints"
        );
        assert!(!sec_path.exists(), "no secret may be created by `pubkey`");

        // Create the identity the way the daemon does at startup.
        let (_sk, pk) = ensure_transport_keypair().unwrap();
        let value = pubkey_value();
        set_test_config_root(None);

        let value = value.unwrap();
        assert_eq!(value.len(), 44, "a 32-byte key is 44 base64 chars");
        assert!(
            !value.contains('\n'),
            "the value itself carries no trailing newline (println adds it)"
        );
        let decoded: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(&value)
            .expect("the value must be valid base64")
            .try_into()
            .expect("the value must decode to 32 bytes");
        assert_eq!(
            decoded, pk,
            "the printed key is the one derived from the secret"
        );
    }
}
