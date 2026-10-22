use crate::*;

/// `--version` is handled by clap before any real arg parsing: it exits
/// with a `DisplayVersion` error whose message is the version string.
/// Assert both so the flag stays wired to `CARGO_PKG_VERSION` (it breaks
/// silently if the derive attribute loses the bare `version` marker).
#[test]
fn version_flag_displays_package_version() {
    // clap returns the version as a `DisplayVersion` error instead of a
    // value; match it out by hand (Cli doesn't derive Debug, so
    // `unwrap_err()`'s Debug bound doesn't apply).
    let Err(err) = Cli::try_parse_from(["choreo-tui", "--version"]) else {
        panic!("--version should short-circuit before arg validation");
    };
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    // And the release name (from choreo-shared/release-name.txt) rides along.
    let expected = choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION"));
    assert!(err.to_string().contains(&expected));
}

// ── First-contact trust confirmation ─────────────────────────────

/// Drive `confirm_first_contact` with in-memory I/O.
fn confirm_with(answer: &str) -> (bool, String) {
    let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
    let mut output: Vec<u8> = Vec::new();
    let confirmed = confirm_first_contact("host:9443", "ABCD EFGH", &mut input, &mut output)
        .expect("confirmation prompt");
    (confirmed, String::from_utf8(output).expect("utf8 output"))
}

#[test]
fn confirm_accepts_y_and_yes_case_insensitively() {
    for answer in ["y", "Y", "yes", "YES", "  y  "] {
        let (confirmed, _) = confirm_with(&format!("{answer}\n"));
        assert!(confirmed, "'{answer}' must confirm trust");
    }
}

#[test]
fn confirm_rejects_n_and_garbage() {
    for answer in ["n", "N", "no", "", "maybe", "yy"] {
        let (confirmed, _) = confirm_with(&format!("{answer}\n"));
        assert!(!confirmed, "'{answer}' must refuse trust");
    }
}

#[test]
fn confirm_rejects_on_eof() {
    // Empty input = stdin closed: an absent human never confirms.
    let (confirmed, _) = confirm_with("");
    assert!(!confirmed, "EOF must be treated as a refusal");
}

#[test]
fn confirm_prompt_renders_the_fingerprint() {
    let (_, output) = confirm_with("y\n");
    assert!(output.contains("host:9443"), "the address must be shown");
    assert!(
        output.contains("ABCD EFGH"),
        "the fingerprint must be shown"
    );
    assert!(output.contains("[y/N]"), "the default must be refuse");
}

#[test]
fn fingerprint_comparison_ignores_whitespace_but_not_case() {
    // The headless --trust-fingerprint comparison accepts the grouped
    // form and the plain form interchangeably (whitespace is
    // insignificant), but case is EXACT: base64 is case-sensitive, and
    // loosening case would widen the MITM match window on the trust
    // gate. A case-different "match" must be refused.
    assert_eq!(
        strip_fp_whitespace("AB CD EF"),
        strip_fp_whitespace("ABCDEF")
    );
    assert_eq!(strip_fp_whitespace("  AbCd  Ef  "), "AbCdEf");
    assert_ne!(
        strip_fp_whitespace("AB CD EF"),
        strip_fp_whitespace("AB CD EE")
    );
    // Case slips must NOT match — the compare is case-sensitive.
    assert_ne!(
        strip_fp_whitespace("AB CD EF"),
        strip_fp_whitespace("abcdef")
    );
    assert_ne!(
        strip_fp_whitespace("AB CD EF"),
        strip_fp_whitespace("ab cd ef")
    );
}

// ── Preflight failure messages ─────────────────────────

/// An UNREACHABLE daemon (dial failure) must read as a network problem,
/// never as an enrollment problem.
#[test]
fn preflight_unreachable_message_points_at_the_network() {
    let err = choreo_client_core::PreflightError::Unreachable(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "connection refused",
    ));
    let msg = preflight_failure_message("host:9443", &err, None);
    assert!(
        msg.contains("cannot reach the daemon at host:9443"),
        "unreachable must name the address, got: {msg}"
    );
    assert!(
        msg.contains("--tcp-addr"),
        "unreachable must hint at the daemon's flags, got: {msg}"
    );
    assert!(
        !msg.contains("acl-add"),
        "unreachable must NOT suggest enrollment remediation, got: {msg}"
    );
}

/// A REJECTED handshake (daemon up, ACL miss / key change) must read as
/// an authorization problem and carry the enrollment remediation with
/// THIS client's identity EMBEDDED — the TUI user has no choreographr
/// binary, so the message itself must carry the fingerprint and the
/// copy-pasteable base64 pubkey.
#[test]
fn preflight_rejected_message_carries_enrollment_remediation() {
    use base64::Engine as _;

    let err = choreo_client_core::PreflightError::Rejected(
        choreo_transport::error::TransportError::AuthFailed,
    );
    let pk = [
        0x3F, 0x2A, 0x9C, 0x11, 0x7B, 0x04, 0xE5, 0xD8, 0xA1, 0xC6, 0x42, 0xD9, 0x08, 0xF3, 0xB7,
        0xE2, 0x5D, 0x60, 0x19, 0xAB, 0xCC, 0x37, 0x84, 0x0E, 0x71, 0xFA, 0x92, 0x63, 0xDD, 0x4B,
        0x26, 0x50,
    ];
    let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk);
    let msg = preflight_failure_message("host:9443", &err, Some(&pk));
    assert!(
        msg.contains("did not authorize this client's transport key"),
        "rejected must state the authorization failure, got: {msg}"
    );
    // The client's identity is IN the message — nothing to run locally.
    assert!(
        msg.contains(&choreo_client_core::fingerprint(&pk)),
        "rejected must embed the client's fingerprint, got: {msg}"
    );
    assert!(
        msg.contains(&pk_b64),
        "rejected must embed the copy-pasteable base64 pubkey, got: {msg}"
    );
    assert!(
        msg.contains(&format!("choreographr acl-add {pk_b64}")),
        "the enrollment command must be ready to run with the actual key, got: {msg}"
    );
    assert!(
        msg.contains("known_servers.toml"),
        "rejected must cover the server-key-change re-pair path, got: {msg}"
    );
}

/// Degradation: if the client keypair cannot be read, the message must
/// still state the authorization failure (never mask the real error
/// with a key-reading failure).
#[test]
fn preflight_rejected_message_degrades_without_the_client_key() {
    let err = choreo_client_core::PreflightError::Rejected(
        choreo_transport::error::TransportError::AuthFailed,
    );
    let msg = preflight_failure_message("host:9443", &err, None);
    assert!(
        msg.contains("did not authorize this client's transport key"),
        "the failure statement must survive a missing key, got: {msg}"
    );
    assert!(
        msg.contains("could not be read"),
        "the missing key must be mentioned, got: {msg}"
    );
}

// ── File logging setup (the Termux /tmp regression) ────

/// The log file must be pid-keyed and land in the resolved log directory
/// (the test override here, so the test does not touch the developer's real
/// state dir; `$XDG_STATE_HOME/choreographr` in production) — never a bare
/// hardcoded path.
#[test]
fn log_file_path_is_pid_keyed_in_the_log_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = choreo_shared::paths::TestLogDirGuard::set(Some(dir.path().to_path_buf()));
    let path = choreo_shared::paths::log_file("tui", std::process::id());
    assert_eq!(path.parent(), Some(dir.path()));
    assert_eq!(
        path,
        dir.path().join(format!("tui-{}.log", std::process::id()))
    );
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("utf8 name");
    assert!(
        name.starts_with("tui-")
            && std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log")),
        "the log name must be tui-<pid>.log, got {name}"
    );
}

/// The shared `-v`/`-q` flags are flattened into the TUI's CLI, so `-vv`
/// must parse as verbose == 2 and the bare form as 0/0.
#[test]
fn cli_parses_the_shared_verbosity_flags() {
    let cli = Cli::try_parse_from(["choreo-tui", "-vv", "-q"]).expect("flags parse");
    assert_eq!(cli.verbosity.verbose, 2);
    assert_eq!(cli.verbosity.quiet, 1);

    let default = Cli::try_parse_from(["choreo-tui"]).expect("no flags parse");
    assert_eq!(default.verbosity.verbose, 0);
    assert_eq!(default.verbosity.quiet, 0);
}
