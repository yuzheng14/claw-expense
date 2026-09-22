use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use crate::error::{AppError, Result};

/// A new database must not be paired with a leftover SQLite journal.
pub(crate) fn ensure_unused_database_path(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let candidate = PathBuf::from(name);
        match candidate.symlink_metadata() {
            Ok(_) => {
                return Err(AppError::new(
                    "FILE_EXISTS",
                    format!("目标或数据库日志已存在，不会覆盖：{}", candidate.display()),
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
