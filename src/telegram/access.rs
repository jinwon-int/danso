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
        Self::from_env_or_ids(std::iter::empty())
    }

    /// Environment first; when the env var is absent the given ids (from
    /// `telegram.allowed_user_ids` in config.toml) configure the allowlist.
    /// An env var that is set always wins, including when it parses to an
    /// empty (deny-all) allowlist.
    pub fn from_env_or_ids<I>(fallback: I) -> Result<Self>
    where
        I: IntoIterator<Item = i64>,
    {
        match env::var(ALLOWED_USER_IDS_ENV) {
            Ok(raw) => Self::parse(&raw),
            Err(env::VarError::NotPresent) => Ok(Self::from_ids(fallback)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The env var is process-global, so these cases share one guard instead
    /// of racing each other. The unsafe blocks are the only sanctioned way
    /// tests mutate the environment.
    fn with_env<T>(raw: Option<&str>, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = env::var_os(ALLOWED_USER_IDS_ENV);
        match raw {
            Some(raw) => unsafe { env::set_var(ALLOWED_USER_IDS_ENV, raw) },
            None => unsafe { env::remove_var(ALLOWED_USER_IDS_ENV) },
        }
        let outcome = body();
        match previous {
            Some(value) => unsafe { env::set_var(ALLOWED_USER_IDS_ENV, value) },
            None => unsafe { env::remove_var(ALLOWED_USER_IDS_ENV) },
        }
        outcome
    }

    #[test]
    fn env_absent_configures_the_allowlist_from_fallback_ids() {
        let allowlist = with_env(None, || Allowlist::from_env_or_ids([7, 3])).unwrap();
        assert!(allowlist.contains(3));
        assert!(allowlist.contains(7));
        assert!(!allowlist.contains(42));
    }

    #[test]
    fn env_set_wins_over_fallback_ids() {
        let allowlist = with_env(Some("42"), || Allowlist::from_env_or_ids([7, 3])).unwrap();
        assert!(allowlist.contains(42));
        assert!(!allowlist.contains(7));
        assert!(!allowlist.contains(3));
    }

    #[test]
    fn env_set_but_empty_denies_every_update_despite_fallback_ids() {
        for raw in ["", "   "] {
            let allowlist = with_env(Some(raw), || Allowlist::from_env_or_ids([7, 3])).unwrap();
            assert!(allowlist.is_empty());
            assert!(!allowlist.contains(7));
            assert!(!allowlist.contains(3));
        }
    }

    #[test]
    fn from_env_without_the_variable_stays_deny_all() {
        let allowlist = with_env(None, Allowlist::from_env).unwrap();
        assert!(allowlist.is_empty());
    }

    #[test]
    fn env_parse_errors_propagate_despite_fallback_ids() {
        let error = with_env(Some("7,oops"), || Allowlist::from_env_or_ids([3])).unwrap_err();
        assert!(error.to_string().contains("numeric user ids"));
    }
}
