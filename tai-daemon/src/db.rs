use std::io;
use std::path::PathBuf;

pub fn db_path() -> io::Result<PathBuf> {
    if let Ok(override_path) = std::env::var("TAI_DB_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    let data_dir = dirs::data_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "could not determine data directory")
    })?;
    Ok(data_dir.join("tai-daemon").join("state.redb"))
}

pub fn open_db() -> io::Result<redb::Database> {
    redb::Database::create(db_path()?).map_err(|e| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("failed to open database: {e}"),
        )
    })
}
