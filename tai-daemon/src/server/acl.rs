use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::path::Path;
use tracing::{error, info, warn};

/// ACL of authorized client public keys for Noise IK authentication.
///
/// Loads from the TOML file at `authorized_clients_path()` (typically
/// `~/.config/tai-daemon/authorized_clients.toml`).  If the file does
/// not exist, all remote connections are rejected.
pub struct Acl {
    keys: Vec<[u8; 32]>,
}

impl Acl {
    /// Load the ACL from an optional TOML file path.
    ///
    /// If the file does not exist, returns an empty ACL (no remote
    /// connections allowed).  Parse errors are logged and also result
    /// in an empty ACL.
    pub fn load(path: &Path) -> Self {
        let toml_str = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("no authorized_clients.toml found, defaulting to empty ACL");
                return Acl { keys: Vec::new() };
            }
            Err(e) => {
                error!("failed to read authorized_clients.toml: {e}, using empty ACL");
                return Acl { keys: Vec::new() };
            }
        };

        #[derive(serde::Deserialize)]
        struct ClientEntry {
            pubkey: String,
        }

        #[derive(serde::Deserialize)]
        struct AclFile {
            client: Vec<ClientEntry>,
        }

        let parsed: AclFile = match toml::from_str(&toml_str) {
            Ok(p) => p,
            Err(e) => {
                error!("failed to parse authorized_clients.toml: {e}, using empty ACL");
                return Acl { keys: Vec::new() };
            }
        };

        let mut keys = Vec::new();
        for entry in parsed.client {
            let bytes = match BASE64.decode(&entry.pubkey) {
                Ok(b) if b.len() == 32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    arr
                }
                _ => {
                    warn!(
                        "invalid pubkey in authorized_clients.toml: {}",
                        entry.pubkey
                    );
                    continue;
                }
            };
            keys.push(bytes);
        }

        info!(count = keys.len(), "loaded authorized clients ACL");
        Acl { keys }
    }

    /// Check whether a client's public key is authorized.
    pub fn contains(&self, pubkey: &[u8; 32]) -> bool {
        self.keys.contains(pubkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_nonexistent_file_returns_empty_acl() {
        let acl = Acl::load(Path::new("/nonexistent/acl.toml"));
        assert!(!acl.contains(&[0u8; 32]));
    }

    #[test]
    fn load_valid_file_loads_keys() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            tmp,
            r#"
[[client]]
pubkey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
"#
        )
        .expect("write");
        let acl = Acl::load(tmp.path());

        // The base64-decoded key is 0x01..0x20 (32 bytes).
        let mut expected = [0u8; 32];
        for i in 0..32 {
            expected[i] = (i as u8) + 1;
        }
        assert!(acl.contains(&expected));

        // A different key should not match.
        assert!(!acl.contains(&[0u8; 32]));
    }

    #[test]
    fn load_invalid_toml_returns_empty_acl() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "not valid toml").expect("write");
        let acl = Acl::load(tmp.path());
        assert!(!acl.contains(&[0u8; 32]));
    }

    #[test]
    fn load_invalid_base64_skips_entry() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            tmp,
            r#"
[[client]]
pubkey = "not-valid-base64!!"

[[client]]
pubkey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
"#
        )
        .expect("write");
        let acl = Acl::load(tmp.path());

        let mut expected = [0u8; 32];
        for i in 0..32 {
            expected[i] = (i as u8) + 1;
        }
        assert!(acl.contains(&expected));
    }

    #[test]
    fn load_wrong_length_key_skips_entry() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        // b64 of a 16-byte value — wrong length.
        write!(
            tmp,
            r#"
[[client]]
pubkey = "c29tZSAxNiBieXRlIG9r"

[[client]]
pubkey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
"#
        )
        .expect("write");
        let acl = Acl::load(tmp.path());

        let mut expected = [0u8; 32];
        for i in 0..32 {
            expected[i] = (i as u8) + 1;
        }
        assert!(acl.contains(&expected));
    }

    #[test]
    fn load_empty_file_returns_empty_acl() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let acl = Acl::load(tmp.path());
        assert!(!acl.contains(&[0u8; 32]));
    }
}
