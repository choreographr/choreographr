mod delete_files;
mod edit_file;
mod line_count;
mod list_files;
mod write_file;

pub(crate) use delete_files::DeleteFiles;
pub(crate) use edit_file::EditFile;
pub(crate) use line_count::LineCount;
pub(crate) use list_files::ListFiles;
// Public so crate-level integration tests (tests/*_integration.rs) can drive
// the tool through the same API the registry uses, mirroring how find.rs
// exposes FindArgs/execute_find_tool.
pub use edit_file::{EditFileArgs, TextEditArgs, execute_edit_file_tool};
pub use list_files::{ListFilesArgs, execute_list_files_tool};
pub(crate) use write_file::WriteFile;
pub use write_file::{WriteFileArgs, execute_write_file_tool};

use crate::tools::ToolExecError;
use std::{fs::OpenOptions, io::Write};
use std::{io, path::Path};
use tracing::debug;

fn validate_nonempty_path(path: &str) -> Result<String, ToolExecError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ))
    } else {
        Ok(trimmed.to_string())
    }
}

fn ensure_parent_directories(path: &Path, create_parents: bool) -> Result<(), ToolExecError> {
    if !create_parents {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_text_file(path: &Path, content: &str, overwrite: bool) -> io::Result<()> {
    if !overwrite {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        return Ok(());
    }

    atomic_write_text_file(path, content)
}

fn atomic_write_text_file(path: &Path, content: &str) -> io::Result<()> {
    // Resolve symlinks before the swap: persisting the temp file over a
    // symlink would replace the link itself with a regular file, while
    // editing a symlinked file should update the real target in place and
    // keep the link. A missing target has no link to preserve, so fall back
    // to the literal path (canonicalize requires the file to exist).
    let target = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(_) => path.to_path_buf(),
    };
    let dir = target.parent().unwrap_or(Path::new("."));
    // Capture the target's permissions BEFORE the atomic swap: NamedTempFile
    // is created 0600 on Unix, so persisting it over an existing file would
    // silently strip the original mode (e.g. the +x bit on a script) and
    // leave a 0600 copy behind. A missing target (new file) keeps the
    // tempfile default — there are no pre-existing permissions to honor.
    let original_permissions = match std::fs::metadata(&target) {
        Ok(m) => Some(m.permissions()),
        // NotFound is the normal new-file case (keep the tempfile default);
        // any other metadata error (e.g. EACCES on a parent directory) means
        // the write cannot succeed either, so surface it now with a clear
        // message rather than later at persist with a confusing one.
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    // Snapshot the flag before the move into the if-let below; it also feeds
    // the debug event so operators can tell preserved-mode swaps from the
    // new-file (tempfile default) case.
    let preserved_mode = original_permissions.is_some();
    if let Some(perms) = original_permissions {
        // Apply before persist so the rename lands with the right mode — no
        // window where the destination has stripped permissions. This is a
        // best-effort snapshot: if the file's mode changes concurrently, the
        // swap applies the stale mode (an accepted TOCTOU for a local tool).
        tmp.as_file().set_permissions(perms)?;
    }
    debug!(path = %target.display(), preserved_mode, "atomic write: replacing file");
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}
