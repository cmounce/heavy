use std::fs::{self, File, OpenOptions};
use std::future::pending;
use std::io;
use std::path::{Path, PathBuf};

use fs4::FileExt;

/// Runs the background trainer task.
///
/// This will hold ML training logic in the future, but for now all we do is lock the site's data
/// directory for writing.
pub async fn run(data_dir: PathBuf) {
    let lock = match lock_data_dir(&data_dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!(
                "heavy: WARNING: ML trainer disabled for {}: {e}",
                data_dir.display()
            );
            return;
        }
    };

    // Sleep forever. We never actually `drop(lock)`; that's there to hold a reference to it, so
    // that the lock stays active for the duration of the task.
    pending::<()>().await;
    drop(lock);
}

fn lock_data_dir(data_dir: &Path) -> io::Result<File> {
    let create_missing_dir = |path: &Path| -> io::Result<()> {
        match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    };

    // Make sure Heavy's data folder exists, e.g., "/var/lib/heavy/"
    if let Some(base_dir) = data_dir.parent() {
        create_missing_dir(base_dir)?;
    }

    // Make sure the site's data folder exists inside of Heavy's folder. We do this instead of using
    // the recursive `create_dir_all` just to be on the safe side, so we don't create a bunch of
    // folders in the event the data dir is misconfigured.
    create_missing_dir(data_dir)?;

    // Lock the data dir
    let lock_path = data_dir.join(".heavy.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    FileExt::try_lock(&lock).map_err(io::Error::from)?;
    Ok(lock)
}
