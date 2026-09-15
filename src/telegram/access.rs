use super::{ALLOWED_USER_IDS_ENV, client::Update};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeSet, env, fmt};

/// Explicit Telegram numeric user-id allowlist. An empty allowlist is valid
/// and denies every update, which is the safe default when the env var is
/// absent.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    ids: BTreeSet<i64>,
}

impl fmt::Debug for Allowlist {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Allowlist")
            .field("configured", &(!self.ids.is_empty()))
            .finish()
    }
}

impl Allowlist {
    pub fn from_ids<I>(ids: I) -> Self
    where
        I: IntoIterator<Item = i64>,
    {
        Self {
            ids: ids.into_iter().collect(),
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut ids = BTreeSet::new();
        for item in raw.split(',') {
            let item = item.trim();
            ensure!(
                !item.is_empty(),
                "{ALLOWED_USER_IDS_ENV} contains an empty user id"
            );
            let id = item.parse::<i64>().map_err(|_| {
                anyhow::anyhow!("{ALLOWED_USER_IDS_ENV} must contain only numeric user ids")
            })?;
            ids.insert(id);
        }
        Ok(Self { ids })
    }

    pub fn from_env() -> Result<Self> {
        match env::var(ALLOWED_USER_IDS_ENV) {
            Ok(raw) => Self::parse(&raw),
            Err(env::VarError::NotPresent) => Ok(Self::default()),
            Err(error) => Err(error).context(ALLOWED_USER_IDS_ENV),
        }
    }

    pub fn contains(&self, user_id: i64) -> bool {
        self.ids.contains(&user_id)
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessControl {
    allowlist: Allowlist,
}

impl AccessControl {
    pub fn new(allowlist: Allowlist) -> Self {
        Self { allowlist }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self::new(Allowlist::from_env()?))
    }

    pub fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    /// Check admission without logging. The authorize method is the normal
    /// update boundary and records every rejection.
    pub fn is_allowed(&self, update: &Update) -> bool {
        update
            .user_id()
            .is_some_and(|user_id| self.allowlist.contains(user_id))
    }

    /// Admit an update and log a fail-closed rejection. This method has no
    /// reply capability; callers can only send a reply after it returns true.
    pub fn authorize(&self, update: &Update) -> bool {
        if self.is_allowed(update) {
            return true;
        }
        eprintln!("telegram update rejected");
        false
    }

    /// Return the chat id only for an admitted update. A rejected update
    /// therefore cannot accidentally flow into sendMessage.
    pub fn authorized_chat_id(&self, update: &Update) -> Option<i64> {
        if self.authorize(update) {
            update.chat_id()
        } else {
            None
        }
    }
}
