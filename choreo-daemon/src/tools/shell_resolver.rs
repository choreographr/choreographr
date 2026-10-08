//! Startup resolution of the POSIX-compatible shell the `sh` tool runs under.
//!
//! The `sh` tool no longer asks the model to pick a shell: at daemon startup it
//! resolves the best available Unix-compatible shell, caches the decision, and
//! advertises the resolved shell (type + compatibility mode + version) and the
//! capability tier it provides in the tool description, so the model writes for
//! the dialect this machine actually runs.
//!
//! Resolution is **tier-major, lazy, and cached**: shell *types* are walked in a
//! fixed order (`bash >= 4 > zsh > ksh/mksh > dash > ash > busybox(ash)`) and
//! the FIRST tier that yields a suitable binary wins — later tiers are never
//! even probed. Within a tier every candidate is identified by running it (the
//! path name is never trusted), deduped by resolved real path, and the highest
//! version wins.
//!
//! The decision is computed once and held in a write-once [`OnceLock`] (an
//! immutable snapshot, deliberately not a `Mutex`/`RwLock`): the shell set of a
//! machine does not change under a running daemon, so a lock-free cached value
//! is both sufficient and the least shared state.
//!
//! The pure [`resolve`] function takes all of its inputs — the [`Platform`], a
//! PATH-lookup closure, a realpath closure, and a run-and-capture closure — as
//! parameters, so its tier/version selection is unit-testable across every
//! platform from any host with no real filesystem, subprocess, or sleep.
//!
//! The real run-and-capture closure ([`run_program`]) executes every probe
//! through the shared shell watchdog, so each run is bounded by
//! `PROBE_TIMEOUT_MS` and inherits the daemon's child hardening (env
//! sanitization + process-group isolation) rather than running unbounded.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use tracing::{debug, warn};

/// The platform a resolution runs against. Injected so the pure resolver can be
/// unit-tested for every platform from a single host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Linux,
    Macos,
    Bsd,
    Windows,
}

/// The shell family a resolved binary belongs to.
///
/// `BusyboxAsh` is kept distinct from `Ash` because it must be dispatched
/// through its `ash` applet (`argv[0] = "ash"`) rather than run directly, and
/// because its version is read from the multi-call banner rather than a shell
/// variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellKind {
    Bash,
    Zsh,
    Ksh,
    Dash,
    Ash,
    BusyboxAsh,
}

/// A resolved, ready-to-run POSIX shell.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedShell {
    /// The absolute path of the binary to exec (never surfaced to the model).
    pub program: PathBuf,
    /// `argv[0]` to force at spawn, if any. `Some("sh")` for zsh (its native
    /// defaults diverge from POSIX, so it is invoked in sh-emulation mode) and
    /// `Some("ash")` for busybox (applet dispatch by `argv[0]`). `None` runs the
    /// binary natively.
    pub argv0: Option<String>,
    /// The shell family, for compatibility treatment.
    pub kind: ShellKind,
    /// The version string reported by the shell, where obtainable.
    pub version: Option<String>,
    /// The model-facing tool description (shell type + compatibility + version,
    /// NEVER the filesystem path).
    pub description: String,
}

/// The terminal tier order. Uniform across every platform; each tier's
/// *candidate paths* are what differ per platform (see [`candidate_programs`]).
const TIER_ORDER: [ShellKind; 6] = [
    ShellKind::Bash,
    ShellKind::Zsh,
    ShellKind::Ksh,
    ShellKind::Dash,
    ShellKind::Ash,
    ShellKind::BusyboxAsh,
];

/// Injected environment for one resolution pass.
///
/// Every side effect is a closure so the resolver stays pure and testable:
/// * `which` maps a bare program name to the absolute paths PATH resolves it to,
///   in PATH order (the real implementation snapshots PATH once).
/// * `realpath` resolves a candidate to its symlink-resolved real path, or
///   `None` when it does not exist / is not a regular file.
/// * `run_capture` runs a program with the given args and returns its combined
///   stdout+stderr when it exits 0, or `None` on spawn failure / non-zero exit.
pub(crate) struct ShellEnv<'a> {
    pub platform: Platform,
    pub which: &'a dyn Fn(&str) -> Vec<PathBuf>,
    pub realpath: &'a dyn Fn(&Path) -> Option<PathBuf>,
    pub run_capture: &'a dyn Fn(&Path, &[&str]) -> Option<String>,
}

/// A candidate program slot in a tier: a literal absolute path (tried first, so
/// a minimal/no PATH still resolves a well-known install) or a bare name to be
/// looked up in PATH (an additive supplement).
enum Prog {
    Abs(&'static str),
    Name(&'static str),
}

/// Resolve the shell to use, honouring `CHOREO_SHELL` first.
///
/// A forced override is tried first and, when it yields a usable shell,
/// autodetection is skipped entirely. When the override is absent or cannot be
/// satisfied (unresolvable, unidentifiable, or e.g. a bash below the version
/// floor) this logs a warning and falls back to autodetection — it never fails
/// startup.
pub(crate) fn resolve(env: &ShellEnv<'_>, forced: Option<&str>) -> Option<ResolvedShell> {
    if let Some(spec) = forced {
        match resolve_forced(env, spec) {
            Some(resolved) => {
                debug!(
                    spec = spec,
                    kind = ?resolved.kind,
                    program = ?resolved.program,
                    "CHOREO_SHELL override honoured; skipping autodetection"
                );
                return Some(resolved);
            }
            None => {
                warn!(
                    spec = spec,
                    "CHOREO_SHELL override is not a usable POSIX shell; falling back to autodetection"
                );
            }
        }
    }

    for kind in TIER_ORDER {
        if let Some(resolved) = resolve_tier(env, kind) {
            debug!(
                kind = ?resolved.kind,
                program = ?resolved.program,
                version = ?resolved.version,
                "autodetected a POSIX shell"
            );
            return Some(resolved);
        }
    }
    None
}

/// Resolve and cache the default shell for this process.
///
/// The result is computed exactly once and memoized in a [`OnceLock`] — an
/// immutable snapshot with no lock held after init. Returns `None` when no
/// suitable shell is installed, in which case the `sh` tool is not registered
/// at all.
pub(crate) fn detect_default() -> Option<ResolvedShell> {
    static CACHE: OnceLock<Option<ResolvedShell>> = OnceLock::new();
    CACHE.get_or_init(compute_default).clone()
}

/// The real, side-effecting resolution used to seed the cache.
fn compute_default() -> Option<ResolvedShell> {
    let platform = current_platform();
    // Snapshot PATH once: every PATH lookup in this pass consults this value
    // rather than re-reading the environment per candidate.
    let path_env = std::env::var_os("PATH");
    let which = move |name: &str| -> Vec<PathBuf> { which_lookup(path_env.as_ref(), name) };
    let realpath = |p: &Path| -> Option<PathBuf> {
        std::fs::canonicalize(p).ok().filter(|real| real.is_file())
    };
    let run_capture = |p: &Path, args: &[&str]| -> Option<String> { run_program(p, args) };

    let env = ShellEnv {
        platform,
        which: &which,
        realpath: &realpath,
        run_capture: &run_capture,
    };

    let forced = std::env::var("CHOREO_SHELL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(spec) = forced.as_deref() {
        debug!(
            spec = spec,
            "CHOREO_SHELL is set; attempting a forced shell"
        );
    }

    let resolved = resolve(&env, forced.as_deref());
    if let Some(r) = &resolved {
        debug!(
            kind = ?r.kind,
            program = ?r.program,
            version = ?r.version,
            "resolved the `sh` tool's POSIX shell"
        );
    } else {
        warn!(
            "no suitable POSIX shell found (bash>=4, zsh, ksh/mksh, dash, ash, or busybox ash); \
             the `sh` tool will not be registered"
        );
    }
    resolved
}

/// Look a bare program name up in the given PATH value using the `which` crate
/// in-process (never spawning `which(1)`).
fn which_lookup(path_env: Option<&OsString>, name: &str) -> Vec<PathBuf> {
    match path_env {
        // `which_in_all` searches the supplied path list (our PATH snapshot)
        // rather than re-reading the environment.
        Some(paths) => {
            which::which_in_all(name, Some(paths.clone()), ".").map_or_default(Iterator::collect)
        }
        None => which::which_all(name).map_or_default(Iterator::collect),
    }
}

/// How long a single probe run may take before the watchdog kills its process
/// tree. A probe is a trivial `-c` command that answers in milliseconds; the
/// bound exists only so a wedged or hostile candidate reached through PATH (or
/// forced via `CHOREO_SHELL`) can never hang daemon startup.
const PROBE_TIMEOUT_MS: u64 = 5_000;

/// Run `program` with `args`, returning its combined stdout+stderr on a zero
/// exit status.
///
/// stdin is `/dev/null` so a no-argument shell probe (used for the `BusyBox`
/// banner) can never block reading a terminal, and the run is bounded by
/// `PROBE_TIMEOUT_MS` through the shared shell watchdog
/// (`shell_util::spawn_with_watchdog`) — which also applies the child hardening
/// (env sanitization, process-group isolation) and kills the whole process tree
/// on timeout — so a candidate that never exits cannot hang resolution.
fn run_program(program: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (output, _was_killed) =
        super::shell_util::spawn_with_watchdog(&mut cmd, PROBE_TIMEOUT_MS).ok()?;
    if !output.status.success() {
        return None;
    }
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    Some(String::from_utf8_lossy(&combined).into_owned())
}

/// The platform the running binary targets.
fn current_platform() -> Platform {
    if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(any(
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )) {
        Platform::Bsd
    } else {
        Platform::Linux
    }
}

/// The candidate program slots for one tier on one platform, in discovery order.
///
/// Absolute paths come first (they rescue the minimal-PATH launch case) and PATH
/// names follow as an additive supplement. `/bin/sh` and `/usr/bin/sh` are
/// deliberately absent (they are the platform's POSIX "alias", not a specific
/// shell), and `/etc/shells` is never consulted.
fn candidate_programs(platform: Platform, kind: ShellKind) -> Vec<Prog> {
    use Platform::{Bsd, Linux, Macos, Windows};
    use Prog::{Abs, Name};
    match (platform, kind) {
        (Linux, ShellKind::Bash) => vec![Abs("/bin/bash"), Abs("/usr/bin/bash"), Name("bash")],
        (Macos, ShellKind::Bash) => vec![
            Abs("/opt/homebrew/bin/bash"),
            Abs("/usr/local/bin/bash"),
            Abs("/bin/bash"),
            Name("bash"),
        ],
        (Bsd, ShellKind::Bash) => vec![Abs("/usr/local/bin/bash"), Abs("/bin/bash"), Name("bash")],
        (Windows, ShellKind::Bash) => vec![
            Abs(r"C:\Program Files\Git\bin\bash.exe"),
            Abs(r"C:\Program Files\Git\usr\bin\bash.exe"),
            Abs(r"C:\Program Files (x86)\Git\bin\bash.exe"),
            Abs(r"C:\msys64\usr\bin\bash.exe"),
            Abs(r"C:\cygwin64\bin\bash.exe"),
            Name("bash"),
        ],
        // zsh is the same on Linux/macOS/BSD; on Windows it is only ever a PATH
        // find (a native Windows zsh install).
        (_, ShellKind::Zsh) => {
            if platform == Windows {
                vec![Name("zsh")]
            } else {
                vec![Abs("/bin/zsh"), Abs("/usr/bin/zsh"), Name("zsh")]
            }
        }
        // ksh/dash/ash/busybox are Unix-only: this is a Unix-shell tool, so a
        // Windows host (whose only POSIX shell is Git/WSL bash) gets none of
        // these tiers.
        (Windows, _) => vec![],
        (_, ShellKind::Ksh) => vec![
            Abs("/bin/ksh"),
            Abs("/usr/bin/ksh"),
            Abs("/bin/mksh"),
            Name("mksh"),
        ],
        (_, ShellKind::Dash) => vec![Abs("/bin/dash"), Abs("/usr/bin/dash"), Name("dash")],
        (_, ShellKind::Ash) => vec![Abs("/bin/ash"), Abs("/usr/bin/ash")],
        (_, ShellKind::BusyboxAsh) => vec![Abs("/bin/busybox")],
    }
}

/// Resolve the best suitable binary within a single tier, or `None`.
///
/// Candidates are expanded (PATH names resolved via `which`), deduped by their
/// resolved real path, identified by running them, filtered by the tier's
/// identity rule and (for bash) the `>= 4` version floor, and finally ranked by
/// version — the highest version in the tier wins.
fn resolve_tier(env: &ShellEnv<'_>, kind: ShellKind) -> Option<ResolvedShell> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut best: Option<ResolvedShell> = None;

    for prog in candidate_programs(env.platform, kind) {
        let candidates = match prog {
            Prog::Abs(p) => vec![PathBuf::from(p)],
            Prog::Name(n) => (env.which)(n),
        };
        for candidate in candidates {
            let Some(real) = (env.realpath)(&candidate) else {
                continue;
            };
            // Dedupe by resolved real path: /bin/bash and /usr/bin/bash that
            // symlink to the same target are one candidate.
            if !seen.insert(real.clone()) {
                continue;
            }
            let Some((family, version)) = identify(env, &real) else {
                continue;
            };
            // The path name is never trusted: the binary must prove it is the
            // tier's shell family.
            if !tier_accepts(kind, family) {
                continue;
            }
            if kind == ShellKind::Bash && !bash_major_ok(version.as_deref()) {
                continue;
            }
            let resolved = build_resolved(kind, real, version);
            if best
                .as_ref()
                .is_none_or(|prev| version_is_higher(&resolved, prev))
            {
                best = Some(resolved);
            }
        }
    }
    best
}

/// Resolve a `CHOREO_SHELL` override.
///
/// The value is either a *type* name (`bash`/`zsh`/`ksh`/`dash`/`ash`/`busybox`)
/// — resolved to the best binary of that type, subject to the same bash version
/// floor — or a *binary* (an absolute path or a bare name), which is resolved
/// absolute-first then via PATH and identified by running it. A forced shell
/// gets the SAME compatibility treatment as autodetection (zsh → `argv[0] =
/// "sh"`, busybox → `argv[0] = "ash"`).
fn resolve_forced(env: &ShellEnv<'_>, spec: &str) -> Option<ResolvedShell> {
    if let Some(kind) = parse_type(spec) {
        return resolve_tier(env, kind);
    }

    // Otherwise treat the value as a binary (path or bare name).
    let mut candidates: Vec<PathBuf> = Vec::new();
    if spec.contains('/') || spec.contains('\\') {
        candidates.push(PathBuf::from(spec));
    }
    candidates.extend((env.which)(spec));

    let mut seen: HashSet<PathBuf> = HashSet::new();
    for candidate in candidates {
        let Some(real) = (env.realpath)(&candidate) else {
            continue;
        };
        if !seen.insert(real.clone()) {
            continue;
        }
        let Some((family, version)) = identify(env, &real) else {
            continue;
        };
        let kind = match family {
            Family::Bash => ShellKind::Bash,
            Family::Zsh => ShellKind::Zsh,
            Family::Ksh => ShellKind::Ksh,
            Family::BusyboxAsh => ShellKind::BusyboxAsh,
            // dash and ash are indistinguishable at runtime (neither exposes a
            // version variable); fall back to the operator-supplied name.
            Family::Posix => posix_kind_from_name(&real),
        };
        if kind == ShellKind::Bash && !bash_major_ok(version.as_deref()) {
            continue;
        }
        return Some(build_resolved(kind, real, version));
    }
    debug!(
        spec = spec,
        "CHOREO_SHELL did not resolve to any identifiable shell"
    );
    None
}

/// Map a `CHOREO_SHELL` *type* name to its [`ShellKind`], if it is one.
fn parse_type(spec: &str) -> Option<ShellKind> {
    match spec.trim().to_ascii_lowercase().as_str() {
        "bash" => Some(ShellKind::Bash),
        "zsh" => Some(ShellKind::Zsh),
        "ksh" => Some(ShellKind::Ksh),
        "dash" => Some(ShellKind::Dash),
        "ash" => Some(ShellKind::Ash),
        "busybox" => Some(ShellKind::BusyboxAsh),
        _ => None,
    }
}

/// The shell family a binary actually is, discovered by running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Bash,
    Zsh,
    Ksh,
    BusyboxAsh,
    /// A POSIX shell with no identifying version variable — dash or ash.
    Posix,
}

/// Identify a candidate binary by running it, returning its family and (where
/// obtainable) its version.
///
/// Each probe is written with syntax only its own family accepts, so a wrong
/// binary fails the probe (a non-zero exit maps to `None`) instead of being
/// misidentified. The `BusyBox` banner is checked first: a multi-call binary
/// reads its applet from `argv[0]`, so invoking it as `<path> -c …` would be
/// parsed as the bogus applet `-c`, and only a no-argument run prints the
/// recognizable version banner.
fn identify(env: &ShellEnv<'_>, path: &Path) -> Option<(Family, Option<String>)> {
    if let Some(banner) = (env.run_capture)(path, &[])
        && banner.contains("BusyBox")
    {
        // A multi-call binary is only useful if it can actually dispatch the
        // `ash` applet we will run through it.
        if (env.run_capture)(path, &["ash", "-c", "exit 0"]).is_some() {
            return Some((Family::BusyboxAsh, extract_version(&banner)));
        }
        return None;
    }

    if let Some(v) = (env.run_capture)(path, &["-c", BASH_VERSION_PROBE]) {
        let v = v.trim();
        if looks_like_version(v) {
            return Some((Family::Bash, Some(v.to_string())));
        }
    }
    if let Some(v) = (env.run_capture)(path, &["-c", ZSH_VERSION_PROBE]) {
        let v = v.trim();
        if !v.is_empty() {
            return Some((Family::Zsh, Some(v.to_string())));
        }
    }
    if let Some(v) = (env.run_capture)(path, &["-c", KSH_VERSION_PROBE]) {
        let v = v.trim();
        if !v.is_empty() {
            return Some((Family::Ksh, Some(v.to_string())));
        }
    }
    // ksh93 keeps its version in `.sh.version` rather than `$KSH_VERSION`; the
    // syntax is a ksh-only syntax error elsewhere, so it fails cleanly on dash.
    if let Some(v) = (env.run_capture)(path, &["-c", KSH93_VERSION_PROBE]) {
        let v = v.trim();
        if !v.is_empty() {
            return Some((Family::Ksh, Some(v.to_string())));
        }
    }
    // No identifying variable: a plain POSIX shell (dash/ash). Confirm it runs.
    if (env.run_capture)(path, &["-c", "exit 0"]).is_some() {
        return Some((Family::Posix, None));
    }
    None
}

/// Whether a tier accepts a binary of the discovered family. dash/ash share the
/// `Posix` family (unindistinguishable at runtime) so each accepts it.
fn tier_accepts(kind: ShellKind, family: Family) -> bool {
    match kind {
        ShellKind::Bash => family == Family::Bash,
        ShellKind::Zsh => family == Family::Zsh,
        ShellKind::Ksh => family == Family::Ksh,
        ShellKind::Dash | ShellKind::Ash => family == Family::Posix,
        ShellKind::BusyboxAsh => family == Family::BusyboxAsh,
    }
}

/// Assemble a [`ResolvedShell`], applying the per-kind compatibility treatment
/// (`argv0`) and building its model-facing description.
fn build_resolved(kind: ShellKind, program: PathBuf, version: Option<String>) -> ResolvedShell {
    let argv0 = match kind {
        // zsh's interactive/compat defaults diverge from POSIX; forcing
        // argv[0] = "sh" runs it in maximum-compatibility mode.
        ShellKind::Zsh => Some("sh".to_string()),
        // A multi-call binary dispatches its applet by argv[0].
        ShellKind::BusyboxAsh => Some("ash".to_string()),
        _ => None,
    };
    let description = build_description(kind, version.as_deref());
    ResolvedShell {
        program,
        argv0,
        kind,
        version,
        description,
    }
}

/// The bash version floor (the ONLY version floor in the resolver). bash 3.2
/// (macOS's /bin/bash) lacks the extensions the description advertises and is
/// excluded here.
fn bash_major_ok(version: Option<&str>) -> bool {
    version
        .and_then(|v| v.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 4)
}

/// Compare two resolved shells by version (higher wins). A known version beats
/// an unknown one; two unknowns keep the incumbent.
fn version_is_higher(candidate: &ResolvedShell, incumbent: &ResolvedShell) -> bool {
    match (&candidate.version, &incumbent.version) {
        (Some(a), Some(b)) => version_key(a) > version_key(b),
        (Some(_), None) => true,
        _ => false,
    }
}

/// Parse a version string into its numeric components for ordering, ignoring any
/// non-numeric suffix (`"5.2.15(1)-release"` → `[5, 2, 15]`).
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// Map a resolved dash/ash binary to a [`ShellKind`] by its file name, as a
/// best-effort label when runtime identification cannot tell the two apart.
fn posix_kind_from_name(path: &Path) -> ShellKind {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // "dash" contains "ash", so test the more specific name first.
    if name.contains("dash") {
        ShellKind::Dash
    } else if name.contains("ash") {
        ShellKind::Ash
    } else {
        ShellKind::Dash
    }
}

/// True when the trimmed probe output begins with a digit (a plausible version,
/// as opposed to the empty/".." output a non-bash shell yields).
fn looks_like_version(text: &str) -> bool {
    text.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Extract the first `N(.N)*` numeric token from free text, for the `BusyBox`
/// banner (`"BusyBox v1.36.1 (…) multi-call binary."` → `"1.36.1"`).
fn extract_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        let is_digit = b.is_ascii_digit();
        if start.is_none() && is_digit {
            start = Some(i);
        } else if let Some(s) = start {
            let keep = is_digit || b == b'.';
            if !keep {
                let token = text.get(s..i).unwrap_or_default();
                // A trailing dot is trimmed, not kept: "v1." reads as "1".
                let trimmed = token.trim_end_matches('.');
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
                start = None;
            }
        }
    }
    start.and_then(|s| text.get(s..).map(|t| t.trim_end_matches('.').to_string()))
}

/// The POSIX marker probe for bash: `$BASH_VERSINFO` is only defined by bash, and
/// the array expansion is a syntax error everywhere else, so this both
/// identifies and versions bash in one run.
const BASH_VERSION_PROBE: &str =
    r#"printf '%s' "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}.${BASH_VERSINFO[2]}""#;
/// `$ZSH_VERSION` is set only by zsh.
const ZSH_VERSION_PROBE: &str = r#"printf '%s' "$ZSH_VERSION""#;
/// `$KSH_VERSION` is set by mksh (and some ksh builds).
const KSH_VERSION_PROBE: &str = r#"printf '%s' "${KSH_VERSION:-}""#;
/// ksh93 keeps its version in the `.sh.version` expansion.
const KSH93_VERSION_PROBE: &str = r#"printf '%s' "${.sh.version}""#;

/// Build the model-facing tool description: shell type + compatibility mode +
/// version + the capability tier the resolved shell provides, and NEVER the
/// filesystem path (the model is told what it runs under and what that dialect
/// offers, not where the binary lives).
fn build_description(kind: ShellKind, version: Option<&str>) -> String {
    let shell = match kind {
        ShellKind::Bash => "bash",
        ShellKind::Zsh => "zsh",
        ShellKind::Ksh => "ksh",
        ShellKind::Dash => "dash",
        ShellKind::Ash | ShellKind::BusyboxAsh => "ash",
    };
    let subject = match kind {
        ShellKind::BusyboxAsh => match version {
            Some(v) => format!("ash (BusyBox {v})"),
            None => "ash (BusyBox)".to_string(),
        },
        _ => match version {
            Some(v) => format!("{shell} {v}"),
            None => shell.to_string(),
        },
    };
    let compat = match kind {
        ShellKind::Zsh => ", sh-compatibility mode",
        _ => "",
    };
    // The capability line states exactly what this resolved shell offers, so the
    // model writes for the dialect the machine actually runs rather than a
    // lowest-common-denominator POSIX subset. zsh is invoked in sh-emulation
    // mode (see [`build_resolved`]), so only POSIX syntax is safe there.
    let capability = match kind {
        ShellKind::Bash => {
            "bash >=4 features such as arrays, `[[ ]]`, and `mapfile` are available."
        }
        ShellKind::Zsh => "POSIX `sh` syntax only; zsh-specific syntax is disabled.",
        ShellKind::Ksh => {
            "POSIX `sh` plus ksh extensions such as arrays, `[[ ]]`, and `(( ))` are available."
        }
        ShellKind::Dash | ShellKind::Ash | ShellKind::BusyboxAsh => "POSIX `sh` only.",
    };

    format!(
        "Execute a POSIX shell command. On this system, commands run under \
         {subject}{compat}. {capability} Supports pipes, redirects, glob expansion, and \
         environment variables. Prefer this over `exec` when you need shell features. \
         Non-interactive only — commands that read from stdin will hang."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A simulated binary in the virtual filesystem.
    #[derive(Clone)]
    struct Sim {
        /// The shell family this binary behaves as.
        family: &'static str,
        /// Its version, reported by the matching probe.
        version: &'static str,
    }

    impl Sim {
        fn bash(version: &'static str) -> Self {
            Sim {
                family: "bash",
                version,
            }
        }
        fn zsh(version: &'static str) -> Self {
            Sim {
                family: "zsh",
                version,
            }
        }
        fn ksh(version: &'static str) -> Self {
            Sim {
                family: "ksh",
                version,
            }
        }
        fn dash() -> Self {
            Sim {
                family: "dash",
                version: "",
            }
        }
        fn ash() -> Self {
            Sim {
                family: "ash",
                version: "",
            }
        }
        fn busybox(version: &'static str) -> Self {
            Sim {
                family: "busybox",
                version,
            }
        }
    }

    /// A scripted virtual environment: a set of binaries at absolute paths, a
    /// PATH name→paths map, and symlink resolution. No real fs, no subprocess.
    struct Vfs {
        platform: Platform,
        /// real path → simulated binary.
        binaries: HashMap<PathBuf, Sim>,
        /// absolute symlink path → real path.
        links: HashMap<PathBuf, PathBuf>,
        /// PATH program name → absolute candidate paths, in order.
        path: HashMap<String, Vec<PathBuf>>,
    }

    impl Vfs {
        fn new(platform: Platform) -> Self {
            Vfs {
                platform,
                binaries: HashMap::new(),
                links: HashMap::new(),
                path: HashMap::new(),
            }
        }

        fn add(mut self, path: &str, sim: Sim) -> Self {
            self.binaries.insert(PathBuf::from(path), sim);
            self
        }

        fn link(mut self, link: &str, target: &str) -> Self {
            self.links
                .insert(PathBuf::from(link), PathBuf::from(target));
            self
        }

        fn on_path(mut self, name: &str, paths: &[&str]) -> Self {
            self.path
                .insert(name.to_string(), paths.iter().map(PathBuf::from).collect());
            self
        }

        /// Resolve a path through at most one symlink hop to a real binary path.
        fn realize(&self, path: &Path) -> Option<PathBuf> {
            let mut current = path.to_path_buf();
            for _ in 0..4 {
                if self.binaries.contains_key(&current) {
                    return Some(current);
                }
                let target = self.links.get(&current)?;
                current = target.clone();
            }
            None
        }

        /// Autodetect against this VFS (as `resolve` with no override).
        fn resolve(&self) -> Option<ResolvedShell> {
            self.resolve_forced_opt(None)
        }

        /// Run the pure resolver with an optional `CHOREO_SHELL` override.
        fn resolve_forced_opt(&self, forced: Option<&str>) -> Option<ResolvedShell> {
            let which =
                |name: &str| -> Vec<PathBuf> { self.path.get(name).cloned().unwrap_or_default() };
            let realpath = |p: &Path| -> Option<PathBuf> { self.realize(p) };
            let run_capture = |p: &Path, args: &[&str]| -> Option<String> {
                let real = self.realize(p)?;
                let sim = self.binaries.get(&real)?;
                sim.respond(args)
            };
            let env = ShellEnv {
                platform: self.platform,
                which: &which,
                realpath: &realpath,
                run_capture: &run_capture,
            };
            resolve(&env, forced)
        }

        fn resolve_forced(&self, spec: &str) -> Option<ResolvedShell> {
            self.resolve_forced_opt(Some(spec))
        }
    }

    impl Sim {
        /// Answer a probe invocation the way the real shell family would: `None`
        /// for a probe whose syntax this family does not accept, `Some(output)`
        /// otherwise.
        fn respond(&self, args: &[&str]) -> Option<String> {
            // No-argument run: only BusyBox prints a banner.
            if args.is_empty() {
                return Some(if self.family == "busybox" {
                    format!("BusyBox v{} (2024-01-01) multi-call binary.", self.version)
                } else {
                    String::new()
                });
            }
            // BusyBox applet dispatch: `ash -c …` / `ash -c exit`.
            if args.first() == Some(&"ash") {
                return if self.family == "busybox" {
                    Some(String::new())
                } else {
                    None
                };
            }
            let script = args.get(1).copied().unwrap_or("");
            match script {
                s if s == BASH_VERSION_PROBE => {
                    (self.family == "bash").then(|| self.version.to_string())
                }
                s if s == ZSH_VERSION_PROBE => {
                    if self.family == "zsh" {
                        Some(self.version.to_string())
                    } else {
                        Some(String::new())
                    }
                }
                s if s == KSH_VERSION_PROBE => {
                    if self.family == "ksh" {
                        Some(self.version.to_string())
                    } else {
                        Some(String::new())
                    }
                }
                s if s == KSH93_VERSION_PROBE => (self.family == "ksh"
                    && self.version.contains("ksh93"))
                .then(|| self.version.to_string()),
                // The generic `-c 'exit 0'` smoke / fallback probe: every POSIX
                // family runs it; bash/zsh/ksh do too.
                "exit 0" => Some(String::new()),
                _ => None,
            }
        }
    }

    fn assert_kind(result: Option<ResolvedShell>, kind: ShellKind) -> ResolvedShell {
        let resolved = result.expect("expected a resolved shell");
        assert_eq!(resolved.kind, kind, "wrong shell kind: {resolved:?}");
        resolved
    }

    #[test]
    fn linux_bash_5_resolves_to_bash() {
        let vfs = Vfs::new(Platform::Linux).add("/bin/bash", Sim::bash("5.2.15"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from("/bin/bash"));
        assert_eq!(resolved.version.as_deref(), Some("5.2.15"));
        assert_eq!(resolved.argv0, None);
    }

    #[test]
    fn linux_bash_3_2_only_resolves_to_none() {
        // macOS's /bin/bash is 3.2 and below the floor with no other shell.
        let vfs = Vfs::new(Platform::Linux).add("/bin/bash", Sim::bash("3.2.57"));
        assert!(vfs.resolve().is_none());
    }

    #[test]
    fn linux_bash_4_4_supported_boundary() {
        let vfs = Vfs::new(Platform::Linux).add("/bin/bash", Sim::bash("4.4.20"));
        assert_kind(vfs.resolve(), ShellKind::Bash);
    }

    #[test]
    fn macos_prefers_homebrew_bash_and_never_probes_zsh() {
        // Homebrew bash 5 satisfies tier 1, so tier 2 (zsh) must never be used.
        let vfs = Vfs::new(Platform::Macos)
            .add("/opt/homebrew/bin/bash", Sim::bash("5.2.15"))
            .add("/bin/bash", Sim::bash("3.2.57"))
            .add("/bin/zsh", Sim::zsh("5.9"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from("/opt/homebrew/bin/bash"));
    }

    #[test]
    fn macos_falls_to_zsh_with_sh_compat_argv0() {
        let vfs = Vfs::new(Platform::Macos)
            .add("/bin/bash", Sim::bash("3.2.57"))
            .add("/bin/zsh", Sim::zsh("5.9"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::Zsh);
        assert_eq!(resolved.argv0.as_deref(), Some("sh"));
    }

    #[test]
    fn ash_only_system_resolves_to_ash() {
        let vfs = Vfs::new(Platform::Linux).add("/bin/ash", Sim::ash());
        let resolved = assert_kind(vfs.resolve(), ShellKind::Ash);
        assert_eq!(resolved.argv0, None);
    }

    #[test]
    fn busybox_only_system_resolves_to_ash_applet() {
        let vfs = Vfs::new(Platform::Linux).add("/bin/busybox", Sim::busybox("1.36.1"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::BusyboxAsh);
        assert_eq!(resolved.argv0.as_deref(), Some("ash"));
        assert_eq!(resolved.version.as_deref(), Some("1.36.1"));
    }

    #[test]
    fn windows_git_bash_resolves_to_bash() {
        let git = r"C:\Program Files\Git\bin\bash.exe";
        let vfs = Vfs::new(Platform::Windows).add(git, Sim::bash("5.2.15"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from(git));
    }

    #[test]
    fn windows_with_nothing_resolves_to_none() {
        let vfs = Vfs::new(Platform::Windows);
        assert!(vfs.resolve().is_none());
    }

    #[test]
    fn tier_major_higher_tier_wins_over_lower_present() {
        // dash (tier 4) and bash (tier 1) both present → bash wins.
        let vfs = Vfs::new(Platform::Linux)
            .add("/bin/dash", Sim::dash())
            .add("/bin/bash", Sim::bash("5.2.15"));
        assert_kind(vfs.resolve(), ShellKind::Bash);
    }

    #[test]
    fn within_tier_highest_version_wins() {
        let vfs = Vfs::new(Platform::Linux)
            .add("/bin/bash", Sim::bash("5.2.15"))
            .add("/usr/bin/bash", Sim::bash("5.3.20"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from("/usr/bin/bash"));
        assert_eq!(resolved.version.as_deref(), Some("5.3.20"));
    }

    #[test]
    fn path_supplied_bash_is_found() {
        // bash is not at an absolute path, only in PATH.
        let vfs = Vfs::new(Platform::Linux)
            .add("/opt/custom/bin/bash", Sim::bash("5.1.16"))
            .on_path("bash", &["/opt/custom/bin/bash"]);
        let resolved = assert_kind(vfs.resolve(), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from("/opt/custom/bin/bash"));
    }

    #[test]
    fn symlinked_candidates_are_deduped() {
        // /usr/bin/bash is a symlink to /bin/bash: one candidate, one version.
        let vfs = Vfs::new(Platform::Linux)
            .add("/bin/bash", Sim::bash("5.2.15"))
            .link("/usr/bin/bash", "/bin/bash");
        let resolved = assert_kind(vfs.resolve(), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from("/bin/bash"));
    }

    #[test]
    fn path_name_is_not_trusted() {
        // A binary named /bin/bash that is really dash must be rejected for the
        // bash tier (and dash is the only other shell, so bash tier yields
        // nothing — but the dash tier then finds it via its own name).
        let vfs = Vfs::new(Platform::Linux).add("/bin/bash", Sim::dash());
        // No dash candidates exist, so nothing resolves.
        assert!(vfs.resolve().is_none());
    }

    #[test]
    fn forced_type_zsh_skips_bash() {
        let vfs = Vfs::new(Platform::Linux)
            .add("/bin/bash", Sim::bash("5.2.15"))
            .add("/bin/zsh", Sim::zsh("5.9"));
        let resolved = assert_kind(vfs.resolve_forced("zsh"), ShellKind::Zsh);
        assert_eq!(resolved.program, PathBuf::from("/bin/zsh"));
    }

    #[test]
    fn forced_bogus_falls_back_to_autodetect() {
        let vfs = Vfs::new(Platform::Linux).add("/bin/bash", Sim::bash("5.2.15"));
        let resolved = assert_kind(vfs.resolve_forced("bogus"), ShellKind::Bash);
        assert_eq!(resolved.program, PathBuf::from("/bin/bash"));
    }

    #[test]
    fn forced_binary_path_is_identified() {
        let vfs = Vfs::new(Platform::Linux).add("/opt/zsh", Sim::zsh("5.9"));
        let resolved = assert_kind(vfs.resolve_forced("/opt/zsh"), ShellKind::Zsh);
        assert_eq!(resolved.argv0.as_deref(), Some("sh"));
    }

    #[test]
    fn forced_bash_below_floor_is_rejected() {
        let vfs = Vfs::new(Platform::Linux)
            .add("/bin/bash", Sim::bash("3.2.57"))
            .add("/bin/zsh", Sim::zsh("5.9"));
        // The forced bash is unusable, so autodetection runs and finds zsh.
        let resolved = assert_kind(vfs.resolve_forced("bash"), ShellKind::Zsh);
        assert_eq!(resolved.program, PathBuf::from("/bin/zsh"));
    }

    #[test]
    fn linux_ksh_resolves_to_ksh() {
        let vfs = Vfs::new(Platform::Linux).add("/bin/ksh", Sim::ksh("1.0.10"));
        let resolved = assert_kind(vfs.resolve(), ShellKind::Ksh);
        assert_eq!(resolved.version.as_deref(), Some("1.0.10"));
        assert_eq!(resolved.argv0, None);
    }

    #[test]
    fn description_names_type_and_version_without_a_path() {
        let bash = build_resolved(
            ShellKind::Bash,
            PathBuf::from("/bin/bash"),
            Some("5.2.15".into()),
        );
        assert!(bash.description.contains("bash 5.2.15"));
        assert!(!bash.description.contains('/'));
        assert!(!bash.description.contains("/bin/bash"));

        let zsh = build_resolved(
            ShellKind::Zsh,
            PathBuf::from("/bin/zsh"),
            Some("5.9".into()),
        );
        assert!(zsh.description.contains("zsh 5.9"));
        assert!(zsh.description.contains("sh-compatibility mode"));
        assert!(!zsh.description.contains('/'));

        let busybox = build_resolved(
            ShellKind::BusyboxAsh,
            PathBuf::from("/bin/busybox"),
            Some("1.36.1".into()),
        );
        assert!(busybox.description.contains("ash (BusyBox 1.36.1)"));
        assert!(!busybox.description.contains('/'));

        let dash = build_resolved(ShellKind::Dash, PathBuf::from("/bin/dash"), None);
        assert!(dash.description.contains("dash"));

        let ksh = build_resolved(
            ShellKind::Ksh,
            PathBuf::from("/bin/ksh"),
            Some("1.0.10".into()),
        );
        assert!(ksh.description.contains("ksh 1.0.10"));
    }

    #[test]
    fn description_states_each_tier_capability() {
        // Every tier advertises exactly what its resolved shell offers, and the
        // shared tail is identical across tiers. The bash string is asserted in
        // full to pin the overall shape (subject → capability → shared tail).
        let bash = build_description(ShellKind::Bash, Some("5.3.20"));
        assert_eq!(
            bash,
            "Execute a POSIX shell command. On this system, commands run under bash 5.3.20. \
             bash >=4 features such as arrays, `[[ ]]`, and `mapfile` are available. Supports \
             pipes, redirects, glob expansion, and environment variables. Prefer this over \
             `exec` when you need shell features. Non-interactive only — commands that read \
             from stdin will hang."
        );

        assert_eq!(
            build_description(ShellKind::Zsh, Some("5.9")),
            "Execute a POSIX shell command. On this system, commands run under zsh 5.9, \
             sh-compatibility mode. POSIX `sh` syntax only; zsh-specific syntax is disabled. \
             Supports pipes, redirects, glob expansion, and environment variables. Prefer this \
             over `exec` when you need shell features. Non-interactive only — commands that \
             read from stdin will hang."
        );

        assert_eq!(
            build_description(ShellKind::Ksh, Some("1.0.10")),
            "Execute a POSIX shell command. On this system, commands run under ksh 1.0.10. \
             POSIX `sh` plus ksh extensions such as arrays, `[[ ]]`, and `(( ))` are \
             available. Supports pipes, redirects, glob expansion, and environment \
             variables. Prefer this over `exec` when you need shell features. \
             Non-interactive only — commands that read from stdin will hang."
        );

        // dash and ash share the minimal POSIX line; busybox's subject still names
        // the BusyBox version while its capability line stays minimal.
        for kind in [ShellKind::Dash, ShellKind::Ash] {
            let description = build_description(kind, None);
            assert!(
                description.contains("POSIX `sh` only."),
                "{kind:?}: {description}"
            );
            assert!(!description.contains("are available"));
        }
        let busybox = build_description(ShellKind::BusyboxAsh, Some("1.36.1"));
        assert!(busybox.contains("ash (BusyBox 1.36.1)"));
        assert!(busybox.contains("POSIX `sh` only."));
    }

    #[test]
    fn extract_version_pulls_numeric_token() {
        assert_eq!(
            extract_version("BusyBox v1.36.1 (2024-01-01) multi-call binary."),
            Some("1.36.1".to_string())
        );
        assert_eq!(extract_version("no numbers here"), None);
    }
}
