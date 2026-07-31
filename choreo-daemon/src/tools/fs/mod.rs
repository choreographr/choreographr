mod delete_files;
mod edit_file;
mod line_count;
mod list_files;
mod write_file;

pub(crate) use delete_files::DeleteFiles;
pub(crate) use edit_file::EditFile;
pub(crate) use line_count::LineCount;
pub(crate) use list_files::ListFiles;
pub(crate) use write_file::WriteFile;

#[cfg(test)]
pub(crate) use edit_file::{EditFileArgs, TextEditArgs, execute_edit_file_tool};
#[cfg(test)]
pub(crate) use write_file::{WriteFileArgs, execute_write_file_tool};

use crate::tools::ToolExecError;
use std::{fs::OpenOptions, io::Write};
use std::{io, path::Path};

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
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
