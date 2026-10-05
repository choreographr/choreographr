//! The code-injection environment variables stripped from spawned child
//! processes.
//!
//! Every child the daemon spawns — the shell/exec tool and an MCP stdio server
//! — inherits the daemon's whole environment unless a variable is removed
//! first. A subset of that environment is a code-injection vector into an
//! untrusted child: the dynamic-loader (`LD_*`, `DYLD_*`) and language-runtime
//! (`PYTHONPATH`, `PERL5LIB`, `RUBYLIB`) variables name code the loader or
//! runtime executes on start-up. Keeping the canonical list here, in the leaf
//! crate both spawn paths already depend on, is what makes the shell tool and
//! the MCP stdio transport strip the *same* set instead of two lists that must
//! be kept in step by hand.

/// The code-injection environment variables stripped from spawned child
/// processes — the shell/exec tool and the MCP stdio child.
///
/// These variables make the dynamic loader (`LD_*`, `DYLD_*`) or a language
/// runtime (`PYTHONPATH`, `PERL5LIB`, `RUBYLIB`) load attacker-chosen code into
/// a child. A child spawned by the daemon inherits the daemon's whole
/// environment, so an operator-exported or profile-set value would otherwise
/// introduce code into a freshly downloaded, untrusted binary; the daemon
/// removes this set before every spawn.
pub const INJECTION_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "LD_DEBUG",
    "PYTHONPATH",
    "PERL5LIB",
    "RUBYLIB",
    "DYLD_INSERT_LIBRARIES",
];

/// Remove every [`INJECTION_ENV_VARS`] entry from `cmd`'s environment.
///
/// Both spawn paths funnel through this: the daemon's shell/exec tool
/// (`choreo-daemon`'s `tools::shell_util::sanitize_env`) and the MCP stdio
/// transport (`choreo-mcp`'s `sanitize_child_env`). It operates on the
/// `std::process::Command` — for the MCP child, the one a
/// `tokio::process::Command` wraps — before any config-specified `env`
/// additions, so a config value can still set a variable it names.
pub fn strip_injection_env(cmd: &mut std::process::Command) {
    for var in INJECTION_ENV_VARS {
        cmd.env_remove(var);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_every_injection_var() {
        // A command seeded with the whole list must come out with none of it,
        // while an unrelated variable passes through untouched.
        let mut cmd = std::process::Command::new("sh");
        for var in INJECTION_ENV_VARS {
            cmd.env(var, "injected");
        }
        cmd.env("CHOREO_KEEP_ME", "kept");
        strip_injection_env(&mut cmd);

        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        for var in INJECTION_ENV_VARS {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(var)),
                Some(&None),
                "{var} must be removed"
            );
        }
        assert_eq!(
            envs.get(std::ffi::OsStr::new("CHOREO_KEEP_ME")),
            Some(&Some(std::ffi::OsStr::new("kept"))),
            "unrelated variables must be left alone"
        );
    }
}
