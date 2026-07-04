use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn init() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("failed to create tokio runtime"))
}

pub fn get() -> &'static tokio::runtime::Runtime {
    RUNTIME.get().expect("tokio runtime not initialized")
}
