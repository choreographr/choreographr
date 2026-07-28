use crate::acp_jsonrpc::ClientCapabilities;

/// Stores the capabilities declared by the editor during `initialize`.
///
/// In v2 this will drive FS/terminal proxy decisions: when the editor
/// supports `fs.readTextFile`, `fs.writeTextFile`, or `terminal`, the
/// bridge can delegate those tool calls back to the editor instead of
/// executing them through the daemon.  In v1, all tool calls go through
/// the daemon (the default).
#[derive(Debug, Default)]
pub struct ClientCapabilitiesStore {
    pub capabilities: Option<ClientCapabilities>,
}

impl ClientCapabilitiesStore {
    pub fn new() -> Self {
        Self { capabilities: None }
    }

    /// Record the capabilities from the editor's `initialize` request.
    pub fn set(&mut self, caps: ClientCapabilities) {
        tracing::info!(?caps, "stored client capabilities");
        self.capabilities = Some(caps);
    }

    /// Whether the editor advertises `fs.readTextFile` support.
    pub fn fs_read_supported(&self) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|c| c.fs.as_ref())
            .and_then(|fs| fs.read_text_file.as_ref())
            .is_some()
    }

    /// Whether the editor advertises `fs.writeTextFile` support.
    pub fn fs_write_supported(&self) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|c| c.fs.as_ref())
            .and_then(|fs| fs.write_text_file.as_ref())
            .is_some()
    }

    /// Whether the editor advertises `terminal` support.
    pub fn terminal_supported(&self) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|c| c.terminal.as_ref())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_jsonrpc::{ClientCapabilities, FsCapabilities, PromptCapabilities};

    fn caps_with_fs(
        read_tf: Option<serde_json::Value>,
        write_tf: Option<serde_json::Value>,
    ) -> ClientCapabilities {
        ClientCapabilities {
            session: None,
            prompt: Some(PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
            }),
            fs: Some(FsCapabilities {
                read_text_file: read_tf,
                write_text_file: write_tf,
            }),
            terminal: None,
        }
    }

    #[test]
    fn new_has_no_capabilities() {
        let store = ClientCapabilitiesStore::new();
        assert!(store.capabilities.is_none());
        assert!(!store.fs_read_supported());
        assert!(!store.fs_write_supported());
        assert!(!store.terminal_supported());
    }

    #[test]
    fn set_stores_capabilities() {
        let mut store = ClientCapabilitiesStore::new();
        let caps = caps_with_fs(None, None);
        store.set(caps);
        assert!(store.capabilities.is_some());
    }

    #[test]
    fn fs_read_supported_when_present() {
        let mut store = ClientCapabilitiesStore::new();
        store.set(caps_with_fs(Some(serde_json::json!({})), None));
        assert!(store.fs_read_supported());
        assert!(!store.fs_write_supported());
    }

    #[test]
    fn fs_write_supported_when_present() {
        let mut store = ClientCapabilitiesStore::new();
        store.set(caps_with_fs(None, Some(serde_json::json!({}))));
        assert!(store.fs_write_supported());
        assert!(!store.fs_read_supported());
    }

    #[test]
    fn fs_both_supported_when_both_present() {
        let mut store = ClientCapabilitiesStore::new();
        store.set(caps_with_fs(
            Some(serde_json::json!({})),
            Some(serde_json::json!({})),
        ));
        assert!(store.fs_read_supported());
        assert!(store.fs_write_supported());
    }

    #[test]
    fn fs_neither_supported_without_fs_block() {
        let mut store = ClientCapabilitiesStore::new();
        store.set(ClientCapabilities {
            session: None,
            prompt: None,
            fs: None,
            terminal: None,
        });
        assert!(!store.fs_read_supported());
        assert!(!store.fs_write_supported());
    }

    #[test]
    fn terminal_supported_when_present() {
        let mut store = ClientCapabilitiesStore::new();
        store.set(ClientCapabilities {
            session: None,
            prompt: None,
            fs: None,
            terminal: Some(serde_json::json!({})),
        });
        assert!(store.terminal_supported());
    }

    #[test]
    fn terminal_not_supported_when_absent() {
        let mut store = ClientCapabilitiesStore::new();
        store.set(ClientCapabilities {
            session: None,
            prompt: None,
            fs: None,
            terminal: None,
        });
        assert!(!store.terminal_supported());
    }

    #[test]
    fn capabilities_default_to_none() {
        let caps = ClientCapabilities::default();
        assert!(caps.session.is_none());
        assert!(caps.prompt.is_none());
        assert!(caps.fs.is_none());
        assert!(caps.terminal.is_none());
    }
}
