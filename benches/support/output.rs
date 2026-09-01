use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_PARTIAL_PATH_ATTEMPTS: u64 = 1_024;
static NEXT_PARTIAL_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct AtomicOutput {
    final_path: PathBuf,
    partial_path: PathBuf,
    writer: BufWriter<File>,
    published: bool,
}

impl AtomicOutput {
    pub(crate) fn create(final_path: &Path) -> Result<Self, io::Error> {
        let parent = output_parent(final_path);
        fs::create_dir_all(parent)?;
        let process_id = std::process::id();
        let first_id = NEXT_PARTIAL_ID.fetch_add(MAX_PARTIAL_PATH_ATTEMPTS, Ordering::Relaxed);
        for offset in 0..MAX_PARTIAL_PATH_ATTEMPTS {
            let partial_path =
                partial_output_path(final_path, process_id, first_id.wrapping_add(offset))?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial_path)
            {
                Ok(file) => {
                    return Ok(Self {
                        final_path: final_path.to_owned(),
                        partial_path,
                        writer: BufWriter::new(file),
                        published: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique benchmark partial output path",
        ))
    }

    pub(crate) fn finish(mut self) -> Result<(), io::Error> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        fs::rename(&self.partial_path, &self.final_path)?;
        self.published = true;
        Ok(())
    }
}

impl Write for AtomicOutput {
    fn write(&mut self, buffer: &[u8]) -> Result<usize, io::Error> {
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.writer.flush()
    }
}

impl Drop for AtomicOutput {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        match fs::remove_file(&self.partial_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "failed to remove benchmark partial output {}: {error}",
                self.partial_path.display()
            ),
        }
    }
}

pub(crate) fn partial_output_path(
    final_path: &Path,
    process_id: u32,
    partial_id: u64,
) -> Result<PathBuf, io::Error> {
    let file_name = final_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "benchmark output path must name a file",
        )
    })?;
    let partial_name = format!(
        ".{}.partial-{process_id}-{partial_id}",
        file_name.to_string_lossy()
    );
    Ok(output_parent(final_path).join(partial_name))
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
