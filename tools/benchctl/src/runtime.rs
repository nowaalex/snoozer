use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::error::BenchError;

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BenchError> {
    let parent = path
        .parent()
        .ok_or_else(|| BenchError::Usage("state path has no parent".to_owned()))?;
    fs::create_dir_all(parent)
        .map_err(|error| BenchError::io("creating state directory", error))?;
    path.file_name()
        .ok_or_else(|| BenchError::Usage("state path has no file name".to_owned()))?;
    let mut file = NamedTempFile::new_in(parent)
        .map_err(|error| BenchError::io("creating partial state", error))?;
    file.write_all(bytes)
        .map_err(|error| BenchError::io("writing partial state", error))?;
    file.as_file()
        .sync_all()
        .map_err(|error| BenchError::io("syncing partial state", error))?;
    file.persist(path)
        .map_err(|error| BenchError::io("publishing state", error.error))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| BenchError::io("syncing state directory", error))
}
