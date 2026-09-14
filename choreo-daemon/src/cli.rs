use crate::config::load_daemon_config;
use crate::daemon::DaemonState;
use anyhow::Context;
use choreo_proto::socket_path;
use choreo_transport::key::ensure_transport_keypair;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

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
// `--version`/`-V` reports this crate's CARGO_PKG_VERSION (which the Homebrew
// formula test, installer, and smoke tests rely on) with the release name
// appended via `choreo_proto::release_name` — e.g. `0.2.0 (Lindy)`, or the
// bare version when the name file is empty. See `choreo-proto/release-name.txt`.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreographr",
    version = choreo_proto::release_name::version_string(env!("CARGO_PKG_VERSION")),
    about = "Choreographr AI daemon",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// Increase logging verbosity (-v debug, -vv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease logging verbosity (only errors and warnings)
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,

    /// Enable Prometheus metrics HTTP server on this socket address
    /// (e.g. 127.0.0.1:9464).  When absent no metrics server is started.
    ///
    /// Requires the `metrics` cargo feature (off by default; rebuild with
    /// `--features metrics`).  When the binary is built without it, passing
    /// this flag is a startup error rather than a silent no-op.
    #[arg(long = "metrics-addr")]
    metrics_addr: Option<String>,

    /// Enable TCP Noise IK listener on this socket address
    /// (e.g. 0.0.0.0:9443).  When absent no TCP listener is started.
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Write daemon logs to this file instead of stderr (ANSI styling is
    /// disabled for file output; RUST_LOG/-v/-q level selection is
    /// unchanged). The daemon refuses to start when the file cannot be
    /// created or opened.
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// Exit automatically when the last client disconnects (used by
    /// choreo-tui, which spawns a private daemon); without this flag the
    /// daemon runs until interrupted. A daemon with this flag that has never
    /// had a client still runs forever — there is no idle timeout.
    #[arg(long = "auto-exit")]
    auto_exit: bool,

    /// Optional utility subcommand. When absent (the overwhelmingly common
    /// case) the daemon runs — `choreographr --tcp-addr 0.0.0.0:9443` keeps
    /// working unchanged because the serve flags stay on the parent command.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Utility subcommands. The daemon itself is the default (no subcommand).
#[derive(clap::Subcommand)]
enum Command {
    /// Enroll a client key in the ACL: appends a `[[client]]` entry to
    /// `authorized_clients.toml` under the advisory file lock; a running
    /// daemon hot-reloads it, so the client can connect immediately without
    /// a restart. Works while the daemon is locked and needs no socket.
    AclAdd {
        /// Base64 of the client's 32-byte transport public key (the client
        /// prints it with `choreo-tui`'s help or reads its transport.pub)
        pubkey: String,
    },
    /// Print the human-comparable fingerprint of a transport public key —
    /// with no argument, this machine's own (read out to the client operator
    /// during enrollment); with a path, any key file (verify a copied
    /// server key or a pinned known-servers entry).
    Fingerprint {
        /// Path to a 32-byte raw transport public key file
        #[arg(default_value = None)]
        path: Option<String>,
    },
}

/// Enroll `pubkey_b64` into the ACL file at `path`. The testable core of the
/// `acl-add` subcommand: the CLI resolves the path from the standard config
/// dir and delegates here.
fn acl_add_to(path: &std::path::Path, pubkey_b64: &str) -> anyhow::Result<usize> {
    use base64::Engine as _;
    let key: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .map_err(|e| anyhow::anyhow!("invalid pubkey: not valid base64: {e}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid pubkey: must decode to exactly 32 bytes"))?;

    // Idempotency: an already-present key is a no-op (no duplicate entry).
    let existing = crate::server::acl::Acl::load(path);
    if existing.contains(&key) {
        info!("pubkey is already authorized; nothing to do");
        return Ok(existing.len());
    }

    // The advisory file lock makes this safe against a concurrent daemon
    // /acl add; the daemon's watcher + parse-compare reload makes the new
    // entry live without a restart.
    crate::server::acl::append_key_locked(path, &key).map_err(|e| anyhow::anyhow!(e))?;
    let count = crate::server::acl::Acl::load(path).len();
    info!(clients = count, "ACL: client key enrolled");
    Ok(count)
}

/// Print the fingerprint of a transport public key (see `Command::Fingerprint`).
fn fingerprint_cli(path: Option<&str>) -> anyhow::Result<()> {
    use choreo_transport::key::{fingerprint, fingerprint_of_file, read_server_pk};
    let fp = match path {
        Some(p) => fingerprint_of_file(std::path::Path::new(p))
            .with_context(|| format!("failed to fingerprint key file {p}"))?,
        None => {
            let pk = read_server_pk(None).context(
                "failed to read this machine's transport public key (has the daemon ever run here?)",
            )?;
            fingerprint(&pk)
        }
    };
    println!("{fp}");
    Ok(())
}

const DEFAULT_MAX_TURNS: u32 = 0;

/// Resolve the tool-loop iteration limit.
///
/// Resolution chain: `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 0 (unlimited).
/// A value of `0` means *unlimited* — the agent loop will run until the
/// model produces a final answer, is cancelled, or hits an error.
///
/// A `CHOREOGRAPHR_MAX_TURNS` that is set but not a valid `u32` is a
/// configuration error: failing startup beats silently running unbounded.
fn resolve_max_turns() -> anyhow::Result<u32> {
    match std::env::var("CHOREOGRAPHR_MAX_TURNS") {
        Ok(val) => return parse_max_turns_env(&val),
        Err(std::env::VarError::NotPresent) => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to read CHOREOGRAPHR_MAX_TURNS: {e}"
            ));
        }
    }
    if let Ok(config) = load_daemon_config()
        && let Some(n) = config.max_turns
    {
        return Ok(n);
    }
    Ok(DEFAULT_MAX_TURNS)
}

/// Parse the `CHOREOGRAPHR_MAX_TURNS` value. Kept as a pure function so the
/// parsing behavior is unit-testable without touching process-global env.
fn parse_max_turns_env(val: &str) -> anyhow::Result<u32> {
    val.parse::<u32>()
        .map_err(|e| anyhow::anyhow!("CHOREOGRAPHR_MAX_TURNS={val:?} is not a valid u32: {e}"))
}

/// Open (creating if absent) the `--log-file` for append, with the hardening
/// that suits a daemon log the TUI autostart writes into the shared temp dir.
///
/// Unix: created 0600, and the open uses `O_NOFOLLOW` so a symlink planted at
/// the (predictable, pid-keyed) log path cannot redirect the daemon's
/// diagnostics into an attacker-chosen file. The opened file is then verified
/// to be a REGULAR file owned by this process's euid — a pre-created file
/// owned by another user (or a FIFO/device) must never collect our logs — and
/// its mode is explicitly tightened to 0600, because the create mode applies
/// only on creation and a file left behind by an earlier run could be 0644.
///
/// Windows: ACLs are inherited from the parent directory (the user's own temp
/// dir), so a plain create+append is correct there.
#[cfg(unix)]
fn open_log_file(path: &str) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        // O_NOFOLLOW: fail rather than follow a symlink at the log path.
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .with_context(|| {
            format!(
                "failed to open --log-file {path} for writing; check that the \
                 directory exists and is writable"
            )
        })?;
    // Refuse to append into anything we do not control: a pre-created file
    // owned by another user, or a non-regular file, must not receive our
    // (potentially sensitive) diagnostics.
    let meta = file
        .metadata()
        .with_context(|| format!("failed to stat --log-file {path}"))?;
    let euid = rustix::process::geteuid().as_raw();
    if !meta.is_file() || meta.uid() != euid {
        anyhow::bail!(
            "refusing to write --log-file {path}: it is not a regular file owned by the \
             current user"
        );
    }
    // Tighten a pre-existing looser mode (the create mode above applies only
    // on creation; a file from an earlier run could be group/world-readable).
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set 0600 on --log-file {path}"))?;
    Ok(file)
}

/// Windows twin of [`open_log_file`]: create+append with inherited ACLs (the
/// parent directory is the user's own temp dir), so no mode/ownership work is
/// needed — and `mode`/`O_NOFOLLOW`/`geteuid` do not exist outside unix.
#[cfg(not(unix))]
fn open_log_file(path: &str) -> anyhow::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| {
            format!(
                "failed to open --log-file {path} for writing; check that the \
                 directory exists and is writable"
            )
        })
}

pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Determine log level: RUST_LOG env var takes precedence, otherwise use CLI flags
    let log_level = if std::env::var("RUST_LOG").is_ok() {
        if cli.verbose > 0 || cli.quiet > 0 {
            warn!("RUST_LOG is set; -v/-q CLI flags are ignored");
        }
        None // Use RUST_LOG as-is
    } else {
        let level = match (cli.verbose, cli.quiet) {
            (0, 0) => "info",
            (_, q) if q > 0 => "warn",
            (1, 0) => "debug",
            _ => "trace",
        };
        Some(level)
    };

    let env_filter = match log_level {
        Some(level) => EnvFilter::new(level),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };

    // Logging init happens HERE — before any subcommand/state work — because
    // everything after it wants to log. With --log-file, open the file first
    // and make failure fatal: a TUI-spawned daemon whose log path is bad must
    // fail loudly with the path, not silently lose all diagnostics. ANSI is
    // always off for file output (escape codes are unreadable in a log file).
    if let Some(path) = &cli.log_file {
        let file = open_log_file(path)?;
        // `Mutex<File>` is a `MakeWriter`: each tracing event locks the file
        // briefly, serializing writes without any extra plumbing.
        fmt()
            .with_env_filter(env_filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init();
    } else {
        fmt().with_env_filter(env_filter).init();
    }

    info!(effective_level = ?log_level.unwrap_or("from RUST_LOG"), "logging initialized");

    // Utility subcommands exit early — they are one-shot file operations and
    // never touch the DB, providers, or listeners below.
    match &cli.command {
        Some(Command::AclAdd { pubkey }) => {
            let path = choreo_keystore::paths::authorized_clients_path()
                .context("failed to resolve authorized_clients path")?;
            let count = acl_add_to(&path, pubkey)?;
            println!("client key authorized ({count} client(s) now trusted)");
            return Ok(());
        }
        Some(Command::Fingerprint { path }) => return fingerprint_cli(path.as_deref()),
        None => {}
    }

    // The blockchain tools (EVM/Substrate) run on a tokio sidecar runtime owned
    // by the `choreo-blockchain` crate. Initialize it once at startup when the
    // `blockchain` feature is enabled (off by default); without the feature the
    // tools — and tokio itself — are compiled out entirely.
    #[cfg(feature = "blockchain")]
    choreo_blockchain::runtime::init()
        .map_err(|e| anyhow::anyhow!("failed to initialize blockchain tokio runtime: {e}"))?;

    // The Choreographr Coordination Platform tools also run on a tokio sidecar
    // runtime (owned by the `choreo-content` crate) used only for signed chain
    // writes via subxt. Initialize it once at startup when the `content`
    // feature is enabled (off by default); without the feature the tools — and
    // the sidecar — are compiled out entirely. A failure is NOT fatal: read
    // tools and IPFS/indexer still work, while content write tools would be unavailable
    // until the sidecar can be built.
    #[cfg(feature = "content")]
    match choreo_content::init() {
        Ok(()) => {}
        Err(e) => {
            warn!(
                error = %e,
                "failed to initialize the coordination platform tokio runtime; \
                 content write tools will be unavailable"
            );
        }
    }

    // The state construction (DB open/migrate/backup dance, tombstone purge,
    // session index, accounts, tool registry, MCP) lives in
    // `DaemonState::open` so the embedded daemon can share the exact same
    // sequence. The CLI supplies the standard paths and the unrestricted tool
    // policy, so its behavior is unchanged.
    let max_turns = resolve_max_turns().context("failed to resolve tool-loop iteration limit")?;
    info!(max_turns, "tool loop iteration limit");
    // The release name (choreo-proto/release-name.txt) is part of the startup
    // banner alongside the crate version, so logs identify the exact series.
    info!(
        version = %choreo_proto::release_name::version_string(env!("CARGO_PKG_VERSION")),
        "choreographr starting (locked)"
    );

    let state = DaemonState::open(crate::daemon::OpenOptions {
        db_path: crate::db::db_path().context("failed to resolve database path")?,
        accounts_path: crate::accounts::accounts_config_path()
            .context("failed to resolve accounts config path")?,
        catalog_paths: crate::catalog::CatalogPaths::from_dirs(),
        tool_policy: crate::tools::ToolPolicy::Full,
        max_turns,
        // Desktop CLI: no platform-native tool host exists in this process.
        platform_tool_bridge: None,
    })
    .context("failed to open daemon state")?;

    // Load or generate the transport keypair for Noise IK.
    let (transport_sk, _transport_pk) =
        ensure_transport_keypair().context("failed to load/generate transport keypair")?;

    // Load the ACL of authorized client public keys.
    let acl_path = choreo_keystore::paths::authorized_clients_path()
        .context("failed to resolve authorized_clients path")?;
    let acl = crate::server::acl::SharedAcl::load(&acl_path);

    let socket_path = socket_path();
    crate::run_server(
        &socket_path,
        state,
        cli.metrics_addr,
        cli.tcp_addr,
        transport_sk,
        acl,
        cli.auto_exit,
    )
    .context("failed to run server")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── --log-file hardening ─────────────────────────────────────────

    /// A fresh log file is created 0600 (not the umask-derived 0644), so
    /// other users on a shared machine cannot read the daemon's diagnostics.
    #[cfg(unix)]
    #[test]
    fn open_log_file_creates_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let file = open_log_file(path.to_str().unwrap()).unwrap();
        drop(file);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh daemon logs must be owner-only");
    }

    /// A file left behind by an earlier run could be group/world-readable;
    /// opening it must tighten the mode to 0600 (the create mode only applies
    /// when the file is actually created).
    #[cfg(unix)]
    #[test]
    fn open_log_file_tightens_a_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.log");
        std::fs::write(&path, b"old log").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_log_file(path.to_str().unwrap()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an existing loose log must be tightened");
    }

    /// A symlink planted at the (predictable, pid-keyed) log path must fail
    /// the open (O_NOFOLLOW), never redirect our diagnostics into an
    /// attacker-chosen file.
    #[cfg(unix)]
    #[test]
    fn open_log_file_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.log");
        std::fs::write(&target, b"secret").unwrap();
        let link = dir.path().join("link.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            open_log_file(link.to_str().unwrap()).is_err(),
            "a symlink at the log path must be refused"
        );
        // The target must not have been touched by the refused open.
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");
    }

    // ── acl-add CLI core ──────────────────────────────────────────────

    const CLI_KEY_A: [u8; 32] = [1u8; 32];
    const CLI_KEY_B: [u8; 32] = [2u8; 32];

    fn cli_b64(key: &[u8; 32]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(key)
    }

    /// The CLI's idempotent enroll: a fresh file gains the entry; a second
    /// identical add is a no-op returning the SAME count; a different key
    /// appends alongside. These pin the behavior an operator relies on when
    /// scripting `choreographr acl-add` against a live daemon.
    #[test]
    fn acl_add_to_appends_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");

        // First add: file is created (with its parent dir), count = 1.
        let count = acl_add_to(&path, &cli_b64(&CLI_KEY_A)).unwrap();
        assert_eq!(count, 1);
        assert!(
            crate::server::acl::Acl::load(&path).contains(&CLI_KEY_A),
            "the enrolled key must authorize"
        );

        // Idempotent re-add: no duplicate entry.
        assert_eq!(acl_add_to(&path, &cli_b64(&CLI_KEY_A)).unwrap(), 1);
        let file = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            file.matches("pubkey").count(),
            1,
            "re-adding must not duplicate the entry"
        );

        // A second key appends alongside.
        assert_eq!(acl_add_to(&path, &cli_b64(&CLI_KEY_B)).unwrap(), 2);
        assert!(crate::server::acl::Acl::load(&path).contains(&CLI_KEY_B));
    }

    #[test]
    fn acl_add_to_rejects_bad_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");
        assert!(acl_add_to(&path, "not-base64!!!").is_err());
        // Valid base64, wrong decoded length.
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode([9u8; 16]);
        assert!(acl_add_to(&path, &short).is_err());
        // Nothing was written for the rejected keys.
        assert!(!path.exists(), "a rejected add must not create the file");
    }

    #[test]
    fn parse_max_turns_env_accepts_zero() {
        assert_eq!(parse_max_turns_env("0").unwrap(), 0);
    }

    #[test]
    fn parse_max_turns_env_accepts_positive() {
        assert_eq!(parse_max_turns_env("42").unwrap(), 42);
    }

    #[test]
    fn parse_max_turns_env_rejects_non_numeric() {
        assert!(parse_max_turns_env("abc").is_err());
    }

    #[test]
    fn parse_max_turns_env_rejects_negative() {
        assert!(parse_max_turns_env("-5").is_err());
    }

    #[test]
    fn parse_max_turns_env_rejects_empty() {
        assert!(parse_max_turns_env("").is_err());
    }

    /// `--version` is handled by clap before any real arg parsing: it exits
    /// with a `DisplayVersion` error whose message is the version string.
    /// Assert both so the flag stays wired to CARGO_PKG_VERSION (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let err = match Cli::try_parse_from(["choreographr", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
        // And the release name (from choreo-proto/release-name.txt) rides along.
        let expected = choreo_proto::release_name::version_string(env!("CARGO_PKG_VERSION"));
        assert!(err.to_string().contains(&expected));
    }
}
