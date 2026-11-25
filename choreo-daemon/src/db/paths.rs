//! Where the store lives.
//!
//! Resolution sits apart from the open sequence because both halves of the
//! module use it on different schedules: [`open`](super::open) resolves the path
//! when it creates or opens the file, while
//! [`run_migrations`](super::run_migrations) resolves the same path to name the
//! pre-migration backup. Folding it into either would make the two modules
//! import each other.

use std::io;
use std::path::PathBuf;

/// Resolve the database file path: the `CHOREOGRAPHR_DB_PATH` override when
/// set, otherwise the standard data directory (platform `dirs::data_dir()` by
/// default, or `{base}/data` under `--base-dir`).
///
/// # Errors
///
/// Returns Err if the data directory cannot be determined.
pub fn db_path() -> io::Result<PathBuf> {
    if let Ok(override_path) = std::env::var("CHOREOGRAPHR_DB_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    choreo_shared::paths::data_file("state.redb")
}
