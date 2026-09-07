//! Explicit transfer of Codex file credentials and single-writer bounded refresh.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};
const MANAGED: &str = "danso-auth.json";
const PENDING: &str = ".danso-refresh-pending";
struct Credentials {
    token: String,
    account: String,
    expiry: i64,
}
pub(super) fn private_file(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "ChatGPT auth path must be absolute");
    let mut dir = File::open("/")?;
    let parts: Vec<_> = path.components().collect();
    ensure!(parts.len() > 2, "invalid ChatGPT auth path");
    for (index, part) in parts.iter().enumerate().skip(1) {
        let Component::Normal(name) = part else {
            bail!("ChatGPT auth path must be normalized")
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid ChatGPT auth path"))?;
        let last = index == parts.len() - 1;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // Walk relative to pinned directory descriptors; no path component follows symlinks.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "cannot open ChatGPT auth path safely");
        let next = unsafe { File::from_raw_fd(fd) };
        let meta = next.metadata()?;
        if last || index == parts.len() - 2 {
            ensure!(
                meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
                "ChatGPT auth file and parent must be owner-only and owned by current user"
            );
        }
        if last {
            ensure!(
                meta.is_file() && meta.nlink() == 1 && meta.len() <= 64 * 1024,
                "ChatGPT auth must be a private regular file up to 64 KiB"
            );
            return Ok(next);
        }
        dir = next;
    }
    bail!("invalid ChatGPT auth path")
}

fn read_value(path: &Path) -> Result<Value> {
    let mut file = private_file(path)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read ChatGPT auth")?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() <= 64 * 1024
            && before.len() == after.len()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.mtime() == after.mtime(),
        "ChatGPT auth changed while reading"
    );
    let v: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid ChatGPT auth JSON"))?;
    Ok(v)
}

fn parse_credentials(v: &Value, allow_expired: bool) -> Result<Credentials> {
    ensure!(
        (v["auth_mode"].is_null() || v["auth_mode"] == "chatgpt") && v["OPENAI_API_KEY"].is_null(),
        "ChatGPT file authentication required; API keys are not subscription credentials"
    );
    let token = v["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing ChatGPT access token")?;
    let account = v["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing ChatGPT account ID")?;
    let parts: Vec<_> = token.split('.').collect();
    ensure!(parts.len() == 3, "invalid ChatGPT access token format");
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT token metadata"))?;
    let claims: Value = serde_json::from_slice(&payload)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT token metadata"))?;
    // Metadata is not signature verification: the service authenticates the bearer.
    ensure!(
        claims["https://api.openai.com/auth"]["chatgpt_account_id"] == account,
        "ChatGPT account metadata mismatch"
    );
    let exp = claims["exp"]
        .as_i64()
        .context("missing ChatGPT token expiry")?;
    ensure!(
        allow_expired || exp > chrono::Utc::now().timestamp().saturating_add(60),
        "ChatGPT authentication expired or expires within 60 seconds; renew using Codex login then retry explicitly"
    );
    Ok(Credentials {
        token: token.into(),
        account: account.into(),
        expiry: exp,
    })
}

struct Directory(File);
impl Directory {
    fn open(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "auth directory must be absolute");
        let mut dir = File::open("/")?;
        for part in path.components().skip(1) {
            let Component::Normal(name) = part else {
                bail!("auth directory must be normalized")
            };
            let name = CString::new(name.as_bytes())?;
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
                )
            };
            ensure!(fd >= 0, "cannot open auth directory safely");
            dir = unsafe { File::from_raw_fd(fd) };
        }
        let m = dir.metadata()?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "auth directory must be owner-only"
        );
        Ok(Self(dir))
    }
    fn open_file(&self, name: &str, flags: i32) -> Result<File> {
        let name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        ensure!(fd >= 0, "cannot open managed auth file safely");
        let f = unsafe { File::from_raw_fd(fd) };
        let m = f.metadata()?;
        ensure!(
            m.is_file()
                && m.nlink() == 1
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o777 == 0o600,
            "unsafe managed auth file"
        );
        Ok(f)
    }
    fn exists(&self, name: &str) -> Result<bool> {
        let name = CString::new(name)?;
        let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
        let rc = unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                s.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc == 0 {
            return Ok(true);
        }
        ensure!(
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT),
            "cannot inspect managed auth entry"
        );
        Ok(false)
    }
    fn lock(&self) -> Result<File> {
        let f = self.open_file(".danso-auth.lock", libc::O_RDWR | libc::O_CREAT)?;
        f.try_lock_exclusive()
            .context("ChatGPT credentials busy; retry the run explicitly")?;
        Ok(f)
    }
    fn read(&self, name: &str) -> Result<Value> {
        let mut f = self.open_file(name, libc::O_RDONLY)?;
        let before = f.metadata()?;
        ensure!(before.len() <= 64 * 1024, "managed auth exceeds 64 KiB");
        let mut data = Vec::new();
        (&mut f).take(64 * 1024 + 1).read_to_end(&mut data)?;
        let after = f.metadata()?;
        ensure!(
            data.len() <= 64 * 1024
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "managed auth changed while reading"
        );
        serde_json::from_slice(&data).map_err(|_| anyhow::anyhow!("invalid managed auth JSON"))
    }
    fn write_new(&self, name: &str, v: &Value) -> Result<()> {
        let data = serde_json::to_vec(v)?;
        ensure!(data.len() <= 64 * 1024, "managed auth exceeds 64 KiB");
        let mut f = self.open_file(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        ensure!(
            unsafe { libc::fchmod(f.as_raw_fd(), 0o600) } == 0,
            "cannot set auth permissions"
        );
        f.write_all(&data)?;
        f.sync_all()?;
        self.0.sync_all()?;
        Ok(())
    }
    fn rename_new(&self, old: &str, new: &str) -> Result<()> {
        let old = CString::new(old)?;
        let new = CString::new(new)?;
        ensure!(
            unsafe {
                libc::renameat2(
                    self.0.as_raw_fd(),
                    old.as_ptr(),
                    self.0.as_raw_fd(),
                    new.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } == 0,
            "cannot move auth recovery artifact"
        );
        self.0.sync_all()?;
        Ok(())
    }
    fn replace(&self, v: &Value) -> Result<()> {
        let nonce = uuid::Uuid::new_v4();
        let temp = format!(".danso-auth-new-{nonce}.json");
        self.write_new(&temp, v)?;
        // Both generations survive; a crash in the rename gap fails closed.
        self.rename_new(MANAGED, &format!("danso-auth-archive-{nonce}.json"))?;
        self.rename_new(&temp, MANAGED)
    }
    fn managed(&self) -> Result<Value> {
        ensure!(
            !self.exists("auth.json")?,
            "Codex auth.json reappeared; resolve credential ownership before using Danso"
        );
        ensure!(
            !self.exists(PENDING)?,
            "ChatGPT refresh outcome is uncertain; retain recovery artifacts and reauthenticate into a new store"
        );
        let v = self.read(MANAGED)?;
        ensure!(
            v["dansoManagedChatGPT"] == 1,
            "auth file was not adopted by Danso"
        );
        Ok(v)
    }
}
fn managed_path(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == MANAGED)
}

/// Explicit local-only transfer. The operator must stop all Codex users of this
/// isolated login home first. A running third-party process cannot be revoked here.
pub fn adopt(source: &Path) -> Result<PathBuf> {
    ensure!(
        source.file_name().is_some_and(|n| n == "auth.json"),
        "adoption requires isolated Codex auth.json"
    );
    let parent = source.parent().context("missing auth parent")?;
    let dir = Directory::open(parent)?;
    let _lock = dir.lock()?;
    ensure!(
        !dir.exists(MANAGED)? && !dir.exists(PENDING)?,
        "managed store already exists"
    );
    let mut value = dir.read("auth.json")?;
    parse_credentials(&value, true)?;
    ensure!(
        value.get("dansoManagedChatGPT").is_none(),
        "credentials already managed"
    );
    ensure!(
        value["tokens"]["refresh_token"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "missing ChatGPT refresh token"
    );
    value["dansoManagedChatGPT"] = json!(1);
    let nonce = uuid::Uuid::new_v4();
    let staged = format!(".danso-adopt-new-{nonce}.json");
    dir.write_new(&staged, &value)?;
    dir.rename_new("auth.json", &format!("codex-auth-imported-{nonce}.json"))?;
    dir.rename_new(&staged, MANAGED)?;
    Ok(parent.join(MANAGED))
}

pub(super) fn inspect(path: &Path) -> Result<(String, String)> {
    let c = if managed_path(path) {
        let dir = Directory::open(path.parent().context("missing auth parent")?)?;
        let _lock = dir.lock()?;
        parse_credentials(&dir.managed()?, true)?
    } else {
        let v = read_value(path)?;
        ensure!(
            v.get("dansoManagedChatGPT").is_none(),
            "managed credentials require their canonical store filename"
        );
        parse_credentials(&v, false)?
    };
    Ok((c.token, c.account))
}

pub(super) async fn access(path: &Path, base: &str, timeout: u64) -> Result<(String, String)> {
    if !managed_path(path) {
        return inspect(path);
    }
    let dir = Directory::open(path.parent().context("missing auth parent")?)?;
    let _lock = dir.lock()?;
    let mut v = dir.managed()?;
    let old = parse_credentials(&v, true)?;
    if old.expiry > chrono::Utc::now().timestamp().saturating_add(60) {
        return Ok((old.token, old.account));
    }
    let refresh = v["tokens"]["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing ChatGPT refresh token")?;
    let body = json!({"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_token":refresh});
    let endpoint = if base == "https://chatgpt.com/backend-api/codex" {
        "https://auth.openai.com/oauth/token".to_string()
    } else {
        let u =
            reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("invalid refresh fixture"))?;
        ensure!(
            u.scheme() == "http"
                && matches!(u.host_str(), Some("127.0.0.1" | "[::1]"))
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none(),
            "refresh requires fixed issuer or loopback fixture"
        );
        format!("{}/oauth/token", base.trim_end_matches('/'))
    };
    ensure!((1..=300).contains(&timeout), "invalid refresh timeout");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout))
        .connect_timeout(Duration::from_secs(timeout.min(10)))
        .build()
        .map_err(|_| anyhow::anyhow!("cannot construct refresh client"))?;
    let request = client
        .post(endpoint)
        .json(&body)
        .build()
        .map_err(|_| anyhow::anyhow!("cannot construct refresh request"))?;
    // Persist uncertainty BEFORE the remotely rotating operation. Never retry an
    // ambiguous exchange, even if the old access token still appears usable.
    dir.write_new(
        PENDING,
        &json!({"version":1,"operation":uuid::Uuid::new_v4().to_string()}),
    )?;
    let mut response = client.execute(request).await.map_err(|_| {
        anyhow::anyhow!(
            "ChatGPT refresh transport failed; outcome uncertain, reauthentication required"
        )
    })?;
    ensure!(
        response.status().is_success(),
        "ChatGPT refresh failed: HTTP {}; reauthentication required",
        response.status().as_u16()
    );
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("ChatGPT refresh response failed; outcome uncertain"))?
    {
        ensure!(
            data.len() + chunk.len() <= 64 * 1024,
            "ChatGPT refresh response exceeds 64 KiB"
        );
        data.extend_from_slice(&chunk);
    }
    let reply: Value = serde_json::from_slice(&data)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT refresh response"))?;
    let token = reply["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing refreshed access token")?;
    v["tokens"]["access_token"] = json!(token);
    if let Some(refresh) = reply.get("refresh_token") {
        ensure!(
            refresh.as_str().is_some_and(|s| !s.is_empty()),
            "invalid rotated refresh token"
        );
        v["tokens"]["refresh_token"] = refresh.clone();
    }
    let updated = parse_credentials(&v, false)?;
    ensure!(
        updated.account == old.account,
        "ChatGPT refresh changed account"
    );
    v["last_refresh"] = json!(chrono::Utc::now().to_rfc3339());
    // The exchange yielded to other processes: re-check ownership before any
    // refreshed token is installed or used for inference. Keep pending on drift.
    ensure!(
        !dir.exists("auth.json")?,
        "Codex auth.json reappeared during refresh; reauthentication into a new isolated store required"
    );
    dir.replace(&v)?;
    dir.rename_new(
        PENDING,
        &format!("danso-refresh-receipt-{}.json", uuid::Uuid::new_v4()),
    )?;
    Ok((updated.token, updated.account))
}
