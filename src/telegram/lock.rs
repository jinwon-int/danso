use super::{DATA_DIR_ENV, data_dir_from_env, ensure_private_dir};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub const TOKEN_LOCK_FILE_NAME: &str = ".telegram-token.lock";

/// A process-lifetime exclusive owner for one Telegram bot token. The lock
/// file is intentionally retained on disk; the kernel lock, not file
/// deletion, is the ownership signal, so stale files do not block recovery.
#[derive(Debug)]
pub struct TokenLock {
    file: File,
    path: PathBuf,
}

impl TokenLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        ensure_private_dir(data_dir)?;
        let path = data_dir.join(TOKEN_LOCK_FILE_NAME);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("open Telegram token lock: {}", path.display()))?;
        validate_lock_file(&file)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file, path }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => bail!(
                "Telegram token lock is already held; refusing to start a second consumer ({})",
                path.display()
            ),
            Err(error) => Err(error).context("acquire Telegram token lock"),
        }
    }

    pub fn acquire_from_env() -> Result<Self> {
        Self::acquire(&data_dir_from_env().context(DATA_DIR_ENV)?)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn validate_lock_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Telegram token lock must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Telegram token lock must be owned by the current user"
        );
        ensure!(
            metadata.nlink() == 1,
            "Telegram token lock must not have hard links"
        );
        ensure!(
            metadata.mode() & 0o777 == 0o600,
            "Telegram token lock must have mode 0600"
        );
    }
    Ok(())
}

impl Drop for TokenLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
