use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

use fs2::FileExt;

use crate::{store::Store, Error, Result};

/// An advisory, process-scoped lock.  It is deliberately held for the entire
/// mutation of a Job, including its final rename.
pub struct JobLease {
    #[allow(dead_code)]
    file: File,
}

impl JobLease {
    pub fn acquire(store: &Store, id: i64) -> Result<Self> {
        fs::create_dir_all(store.lock_dir())?;
        let path: PathBuf = store.lock_dir().join(format!("{id}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(Error::User(
                format!("Job {id} está ativo em outro processo; aguardando pausa"),
            )),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for JobLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}
