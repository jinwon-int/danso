use super::{client::Update, ensure_private_dir};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const CONVERSATIONS_DIR: &str = "conversations";
const MAX_RECORD_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRecord {
    pub chat_id: i64,
    /// -1 means that the record has no consumed update yet.
    pub last_update_id: i64,
    #[serde(default)]
    pub session_pointer: Option<String>,
}

impl ConversationRecord {
    pub fn new(chat_id: i64, last_update_id: i64, session_pointer: Option<String>) -> Self {
        Self {
            chat_id,
            last_update_id,
            session_pointer,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConversationStore {
    data_dir: PathBuf,
    records_dir: PathBuf,
}

impl ConversationStore {
    pub fn new(data_dir: &Path) -> Result<Self> {
        ensure_private_dir(data_dir)?;
        let records_dir = data_dir.join(CONVERSATIONS_DIR);
        ensure_private_dir(&records_dir)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            records_dir,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn record_path(&self, chat_id: i64) -> PathBuf {
        self.records_dir.join(format!("{chat_id}.json"))
    }

    pub fn load(&self, chat_id: i64) -> Result<Option<ConversationRecord>> {
        let path = self.record_path(chat_id);
        let Some(file) = open_record(&path)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD_BYTES,
            "Telegram conversation record exceeds its size limit"
        );
        let record: ConversationRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode Telegram conversation record: {}", path.display()))?;
        validate_record(&record)?;
        ensure!(
            record.chat_id == chat_id,
            "Telegram conversation record chat id does not match its path"
        );
        Ok(Some(record))
    }

    pub fn save(&self, record: &ConversationRecord) -> Result<()> {
        validate_record(record)?;
        if let Some(previous) = self.load(record.chat_id)? {
            ensure!(
                record.last_update_id >= previous.last_update_id,
                "Telegram conversation update id cannot move backwards"
            );
        }
        let payload = serde_json::to_vec(record)?;
        atomic_write(&self.record_path(record.chat_id), &payload)
    }

    pub fn save_record(&self, record: &ConversationRecord) -> Result<()> {
        self.save(record)
    }

    /// Advance a chat record monotonically after the caller has handled an
    /// update. A missing session pointer leaves an existing pointer intact.
    pub fn record_update(
        &self,
        update: &Update,
        session_pointer: Option<String>,
    ) -> Result<ConversationRecord> {
        let chat_id = update
            .chat_id()
            .context("Telegram update has no message chat")?;
        let mut record = self
            .load(chat_id)?
            .unwrap_or_else(|| ConversationRecord::new(chat_id, -1, None));
        if update.update_id > record.last_update_id {
            record.last_update_id = update.update_id;
        }
        if session_pointer.is_some() {
            record.session_pointer = session_pointer;
        }
        self.save(&record)?;
        Ok(record)
    }

    pub fn update(
        &self,
        chat_id: i64,
        last_update_id: i64,
        session_pointer: Option<String>,
    ) -> Result<ConversationRecord> {
        let mut record = self
            .load(chat_id)?
            .unwrap_or_else(|| ConversationRecord::new(chat_id, -1, None));
        ensure!(
            last_update_id >= record.last_update_id,
            "Telegram conversation update id cannot move backwards"
        );
        record.last_update_id = last_update_id;
        if session_pointer.is_some() {
            record.session_pointer = session_pointer;
        }
        self.save(&record)?;
        Ok(record)
    }
}

fn validate_record(record: &ConversationRecord) -> Result<()> {
    ensure!(
        record.last_update_id >= -1,
        "Telegram conversation update id is invalid"
    );
    if let Some(pointer) = &record.session_pointer {
        ensure!(
            !pointer.is_empty() && pointer.len() <= 4096,
            "Telegram session pointer must be 1..=4096 bytes"
        );
    }
    Ok(())
}

fn open_record(path: &Path) -> Result<Option<File>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "Telegram conversation record must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() },
                    "Telegram conversation record must be owned by the current user"
                );
                ensure!(
                    metadata.nlink() == 1,
                    "Telegram conversation record must not have hard links"
                );
                ensure!(
                    metadata.mode() & 0o777 == 0o600,
                    "Telegram conversation record must have mode 0600"
                );
            }
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(Some(options.open(path)?))
}

fn atomic_write(path: &Path, payload: &[u8]) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file(),
            "Telegram conversation record must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.nlink() == 1,
                "Telegram conversation record has hard links"
            );
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() },
                "Telegram conversation record has the wrong owner"
            );
            ensure!(
                metadata.mode() & 0o777 == 0o600,
                "Telegram conversation record must have mode 0600"
            );
        }
    }
    let dir = path
        .parent()
        .context("Telegram conversation record has no parent")?;
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .context("missing Telegram record name")?
        .to_string_lossy();
    let temp = dir.join(format!(".{name}.tmp-{}-{unique}", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temp)?;
        file.write_all(payload)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        File::open(dir)?.sync_all()?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
