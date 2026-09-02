mod cache;
mod clipboard;
mod connection;
mod diff_render;
mod markdown_render;
mod render;
mod scrollbar;
mod selection;
mod state;
mod syntax;

pub use choreo_client_core::{ClientError, ShellCommand, parse_input_line};
pub use choreo_markdown::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use choreo_proto::ImageMetadata;
use image::imageops::FilterType;
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::StatefulProtocol};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Resize filter used for all image rendering and encoding jobs.
pub const IMAGE_RESIZE: Resize = Resize::Scale(Some(FilterType::Lanczos3));

pub use crate::image_worker::ImageId;
use crate::image_worker::ImageResult;

/// A rendered (or pending) image in the chat history.
///
/// Raw image bytes are stored behind an `Arc` so the same data can be sent
/// to the background worker thread for encoding at multiple sizes (inline
/// and fullscreen) without a deep copy of the byte buffer.
pub struct RenderedImage {
    pub metadata: ImageMetadata,
    /// Raw image bytes (SVG or raster).  Kept permanently so the image
    /// can be re-encoded at any display resolution via the worker thread.
    /// Shared via `Arc` to avoid cloning the buffer when submitting jobs.
    pub data: Arc<[u8]>,
    /// Encoded terminal protocols keyed by terminal cell size.
    /// Both inline and fullscreen encodings coexist so toggling between
    /// them is a hashmap lookup — no re-encoding needed.
    pub protocols: HashMap<Size, StatefulProtocol>,
    /// Sizes for which encoding previously failed.
    /// The render path will not re-submit jobs for these sizes.
    pub failed_sizes: HashSet<Size>,
    /// Job ID of a currently-pending encoding request, or `None` when
    /// no encoding is in flight (idle or cached).
    pub pending_job: Option<ImageId>,
}

impl RenderedImage {
    /// Create a placeholder image.  The render path will submit an encoding
    /// job to the background worker when the image becomes visible — no job
    /// is enqueued here so there is nothing to cancel.
    pub fn new_placeholder(metadata: ImageMetadata, data: Arc<[u8]>) -> Self {
        RenderedImage {
            metadata,
            data,
            protocols: HashMap::new(),
            failed_sizes: HashSet::new(),
            pending_job: None,
        }
    }

    /// Apply a completed [`ImageResult`].
    ///
    /// On success the encoded protocol is inserted into the cache keyed by
    /// its cell size — old entries at other sizes are preserved so that
    /// switching between inline and fullscreen never requires re-encoding.
    /// On failure the size is recorded in `failed_sizes` so the render path
    /// does not re-submit a job for that size every frame, but other sizes
    /// remain eligible.
    pub fn apply_result(&mut self, result: ImageResult) {
        if let Some(protocol) = result.protocol {
            self.protocols.insert(result.cell_size, protocol);
        } else {
            self.failed_sizes.insert(result.cell_size);
        }
        self.pending_job = None;
    }
}

pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub mod image_worker;
pub mod terminal_progress;

use anyhow::Context;
use clap::Parser;

/// Shared clap [`Styles`] for this crate's CLI binary.
///
/// Each CLI crate keeps its own copy (choreo-proto is the wire protocol and
/// must not host CLI styling); if this ever grows, promote it to a dedicated
/// micro-crate instead of putting it in choreo-proto.
///
/// Uses real ANSI hues (green headers/usage, cyan literals/placeholders) rather
/// than bold/underline only, so help output stays legible even in terminals whose
/// bold text isn't visually distinct (e.g. themes that don't remap the bold color).
/// `Styles::styled()` keeps clap's default error/invalid/valid coloring; the
/// overrides colorize the help elements.
fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser)]
// Bare `version` makes `--version` print the crate version (CARGO_PKG_VERSION);
// clap handles it before the app starts, so it works headless too.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreo-tui",
    version,
    about = "Choreographr terminal UI",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// Connect via TCP/Noise IK at this address (e.g. 127.0.0.1:9443)
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Path to the server's Noise IK public key (defaults to the pinned key
    /// in known_servers.toml; on first contact the key is learned via the
    /// Noise XX handshake and must be confirmed interactively)
    #[arg(long = "server-pk")]
    server_pk: Option<String>,

    /// Pre-approve the server fingerprint for first contact (headless mode:
    /// the fingerprint, with or without spaces, must match exactly —
    /// including letter case — the key the daemon presents during the XX
    /// probe; compare against the value the daemon operator reads out with
    /// 'choreographr fingerprint'). Ignored when --server-pk is given.
    #[arg(long = "trust-fingerprint")]
    trust_fingerprint: Option<String>,
}

/// Resolve the connection mode for a TCP address, implementing the trust
/// flow. Resolution order:
///
/// 1. explicit `--server-pk` file — operator-provided pin, used directly
///    (IK against it; this is the original out-of-band mechanism and wins
///    over everything);
/// 2. a pin in `known_servers.toml` — IK against the pinned key, with the
///    loud re-pair guidance on handshake failure (TcpPinned);
/// 3. otherwise FIRST CONTACT: probe the daemon's key with the Noise XX
///    handshake and require the human to confirm its fingerprint (either
///    `expected_fingerprint` for headless use, or an interactive y/N prompt
///    — this runs in main() while the terminal is still in cooked mode, so
///    the prompt is a plain stdout/stdin exchange, no TUI involved) before
///    pinning and connecting.
///
/// This runs BEFORE the TUI starts, so no `Unlock` — which carries the
/// daemon's private key — can ever flow over an unconfirmed channel.
///
/// Every TCP path here ALSO ends with an authorization preflight
/// ([`preflight_daemon_authorization`]): a full Noise IK handshake that the
/// daemon's ACL judges. This is what DENIES the TUI from starting when this
/// client's key is not enrolled — the XX probe used for first contact CANNOT
/// detect that (its handshake completes client-side before the daemon's ACL
/// check runs), so without the preflight an un-enrolled client used to get
/// all the way into the TUI and only fail later with a cryptic I/O error.
fn resolve_connect_mode(
    addr: &str,
    server_pk_path: Option<&str>,
    expected_fingerprint: Option<&str>,
) -> anyhow::Result<choreo_client_core::ConnectionMode> {
    use choreo_client_core::ConnectionMode;

    // 1. Explicit operator-provided key file wins over the store.
    if let Some(path) = server_pk_path {
        let pk = choreo_client_core::read_server_pk(Some(path))
            .context("failed to read server public key")?;
        tracing::info!(addr, path, "using explicitly provided server public key");
        preflight_daemon_authorization(addr, &pk)?;
        return Ok(ConnectionMode::Tcp {
            addr: addr.to_string(),
            server_pk: pk,
        });
    }

    // 2. A pinned key from a previous confirmed first contact.
    let mut known = choreo_client_core::KnownServers::load()?;
    if let Some(pk) = known.lookup(addr)? {
        tracing::info!(
            addr,
            fingerprint = %choreo_client_core::fingerprint(&pk),
            "connecting with pinned server key"
        );
        preflight_daemon_authorization(addr, &pk)?;
        return Ok(ConnectionMode::TcpPinned(addr.to_string()));
    }

    // 3. First contact: learn the key, confirm the fingerprint, pin.
    let learned = choreo_client_core::probe_server_key(addr)
        .context("first-contact probe failed (is the daemon running with --tcp-addr, and is this client's key enrolled in its ACL?)")?;
    let fp = choreo_client_core::fingerprint(&learned);

    let confirmed = match expected_fingerprint {
        // Headless: compare against the pre-approved fingerprint (whitespace
        // is insignificant — accept either the grouped or the plain form).
        Some(expected) => {
            let matches = strip_fp_whitespace(expected) == strip_fp_whitespace(&fp);
            if !matches {
                tracing::error!(
                    addr,
                    expected = %expected,
                    actual = %fp,
                    "--trust-fingerprint does not match the server's key"
                );
            }
            matches
        }
        // Interactive: ask the human. The daemon operator should have sent
        // this fingerprint out-of-band in the enrollment conversation.
        None => {
            let mut stdin = std::io::stdin().lock();
            let mut stdout = std::io::stdout();
            confirm_first_contact(addr, &fp, &mut stdin, &mut stdout)?
        }
    };

    if !confirmed {
        anyhow::bail!(
            "first contact with {addr} not confirmed — refusing to connect; \
             verify the daemon's fingerprint with its operator and retry"
        );
    }

    known
        .pin(addr, &learned)
        .context("failed to persist the confirmed server key")?;
    tracing::info!(addr, fingerprint = %fp, "server key confirmed and pinned");
    preflight_daemon_authorization(addr, &learned)?;
    Ok(ConnectionMode::Tcp {
        addr: addr.to_string(),
        server_pk: learned,
    })
}

/// Preflight the daemon's authorization decision: run a full Noise IK
/// handshake against the server key we are about to connect with, so the
/// daemon's ACL verdict is known BEFORE the TUI starts. On refusal this
/// exits with a remediation message instead of letting the TUI start and
/// die on its first prompt.
fn preflight_daemon_authorization(addr: &str, server_pk: &[u8; 32]) -> anyhow::Result<()> {
    if let Err(e) = choreo_client_core::verify_daemon_authorization(addr, server_pk) {
        // The enrollment-remediation message embeds THIS client's identity,
        // so a TUI-only user (who has no choreographr binary) can copy-paste
        // their key straight to the daemon operator. Reading the keypair is
        // best-effort: it exists whenever the preflight ran, and a failure
        // degrades to the generic wording rather than masking the real
        // error.
        let client_pk = choreo_client_core::own_transport_pubkey().ok();
        anyhow::bail!(preflight_failure_message(addr, &e, client_pk.as_ref()));
    }
    Ok(())
}

/// Render a preflight failure as a human-actionable message. Factored out
/// of [`preflight_daemon_authorization`] so tests can pin the wording.
fn preflight_failure_message(
    addr: &str,
    err: &choreo_client_core::PreflightError,
    client_pk: Option<&[u8; 32]>,
) -> String {
    match err {
        choreo_client_core::PreflightError::Unreachable(io) => format!(
            "cannot reach the daemon at {addr}: {io}\n\
             is the daemon running, and did it start with --tcp-addr?",
        ),
        choreo_client_core::PreflightError::Rejected(_) => {
            // The identity block: fingerprint for human comparison, plus the
            // raw base64 pubkey the operator's enrollment command consumes —
            // everything the TUI-only user needs, nothing left for them to
            // run on this machine.
            let identity = client_pk
                .map(|pk| {
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(pk);
                    format!(
                        "your client fingerprint:\n\
                         {}\n\
                         your public key (send this to the daemon operator):\n\
                         {b64}\n\
                         the operator enrolls it on the daemon machine with:\n\
                         choreographr acl-add {b64}\n\
                         (or '/acl add {b64}' from a local TUI connection there)\n",
                        choreo_client_core::fingerprint(pk),
                    )
                })
                .unwrap_or_else(|| {
                    "your client key could not be read from this machine's config directory.\n"
                        .to_string()
                });
            format!(
                "the daemon at {addr} did not authorize this client's transport key.\n\
                 {identity}\
                 once enrolled, reconnect.\n\
                 if you pinned this server before and its key has since changed, remove the\n\
                 entry for {addr} from known_servers.toml and reconnect to re-confirm.\n\
                 ({err})",
            )
        }
    }
}

/// Normalize a fingerprint for comparison: drop the grouping spaces, keep
/// case EXACTLY. Base64's standard alphabet is case-sensitive, and the
/// fingerprint gate is the only thing standing between a first-contact MITM
/// and the daemon's private key — so the comparison must not widen the
/// match: a case-insensitive compare would let an attacker whose key
/// differs from the real one only in letter case pass the check after
/// ~2^28 key-generation tries. Whitespace stays insignificant (copy-paste
/// with or without the 4-char grouping must both work), but any case
/// difference is a mismatch — a transcribed-but-mistyped fingerprint
/// SHOULD be refused and re-read rather than fuzzily accepted.
fn strip_fp_whitespace(fp: &str) -> String {
    fp.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Interactive first-contact confirmation: render the fingerprint, ask for
/// y/N, read the answer. Factored with injected I/O so tests can drive it
/// without a real terminal.
fn confirm_first_contact(
    addr: &str,
    fingerprint: &str,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> anyhow::Result<bool> {
    writeln!(output, "First contact with {addr}.")?;
    writeln!(output, "Server fingerprint:")?;
    writeln!(output, "  {fingerprint}")?;
    writeln!(
        output,
        "Compare this with the fingerprint the daemon operator sent you,"
    )?;
    writeln!(output, "then trust and pin this server? [y/N]")?;
    output.flush()?;

    let mut line = String::new();
    let n = input
        .read_line(&mut line)
        .context("failed to read the trust confirmation answer")?;
    if n == 0 {
        // EOF on stdin (piped/redirected input without an answer): treat as
        // a refusal — an absent human never confirms trust.
        tracing::warn!(
            addr,
            "stdin closed during first-contact confirmation; refusing"
        );
        return Ok(false);
    }
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Entry point for the `choreo-tui` binary wrapper.
///
/// The root `choreographr` package declares `choreo-tui` as a thin binary
/// that calls this function, so the TUI lives entirely in this library crate.
pub fn main() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let cli = Cli::parse();

    let mode = if let Some(addr) = cli.tcp_addr {
        resolve_connect_mode(
            &addr,
            cli.server_pk.as_deref(),
            cli.trust_fingerprint.as_deref(),
        )?
    } else {
        choreo_client_core::ConnectionMode::UnixSocket(choreo_proto::socket_path())
    };

    let log_path = format!("/tmp/choreo-tui-{}.log", std::process::id());
    let log_file = std::fs::File::create(&log_path)?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(log_file)
        .with_ansi(false);
    tracing_subscriber::registry().with(file_layer).init();

    connection::run_app(mode)?;
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// `--version` is handled by clap before any real arg parsing: it exits
    /// with a `DisplayVersion` error whose message is the version string.
    /// Assert both so the flag stays wired to CARGO_PKG_VERSION (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let err = match Cli::try_parse_from(["choreo-tui", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
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
        let err = choreo_client_core::PreflightError::Rejected(
            choreo_transport::error::TransportError::AuthFailed,
        );
        let pk = [
            0x3F, 0x2A, 0x9C, 0x11, 0x7B, 0x04, 0xE5, 0xD8, 0xA1, 0xC6, 0x42, 0xD9, 0x08, 0xF3,
            0xB7, 0xE2, 0x5D, 0x60, 0x19, 0xAB, 0xCC, 0x37, 0x84, 0x0E, 0x71, 0xFA, 0x92, 0x63,
            0xDD, 0x4B, 0x26, 0x50,
        ];
        use base64::Engine as _;
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
}

#[cfg(test)]
mod ai_providers_tests;
#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod daemon_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod lib_tests;
#[cfg(test)]
mod model_selector_tests;
#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod test_util;
