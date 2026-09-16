//! Issued tokens: the credential a client carries when the key stays here.
//!
//! With access off, the client sends a key of its own and this module is never
//! consulted. With access on, the account's key is read from a file the deployment
//! names and every client is handed a token instead. The difference is not
//! cosmetic: a forwarded key cannot be revoked, cannot be limited, and cannot be
//! told apart from the same key in somebody else's hands — which is what makes a
//! shared subscription unaccountable rather than merely shared.
//!
//! What a token buys is a name. The access line records it, sessions are filed
//! under it, and the account's key never leaves this machine. So "who is using
//! this" is answerable, and "stop using this" is an edit to one file.
//!
//! Tokens are stored hashed. The file is not a credential store: it holds the
//! sha256 of each token, so it can be read, copied and backed up without handing
//! anyone the ability to spend the account, and the token itself is shown once,
//! when it is issued.
//!
//! The file is re-read when it changes, which is what makes issuing and revoking
//! something an operator does to a running deployment rather than to a stopped
//! one. A file that cannot be read is a reason to keep the tokens already loaded
//! and warn; a file that is gone is a deployment with no tokens, which refuses
//! everything — the loudest thing an accidental deletion can do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bifrost_config::AccessConfig;
use bifrost_core::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::log;

/// The prefix every issued token carries.
///
/// A prefix rather than a bare random string so that a client's credential can be
/// told from a key at a glance — in a configuration file, in a paste, and in the
/// refusal an operator reads when someone sends the wrong kind of credential.
pub const TOKEN_PREFIX: &str = "bfr_";

/// The randomness behind one token, in bytes.
///
/// 32 bytes because a token is a bearer credential: the only thing standing
/// between guessing one and spending the account is that there is nothing to
/// guess. It comes from the operating system rather than from the clock-and-pid
/// source the wire layer uses for session ids, which is documented as unfit for
/// anything that has to resist prediction.
const TOKEN_BYTES: usize = 32;

/// The one token-file schema this build reads and writes.
pub const TOKEN_FILE_VERSION: u32 = 1;

/// A fresh token, in the only moment it exists in the clear.
pub fn mint() -> Result<String, String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| format!("could not read the system's randomness: {error}"))?;
    Ok(format!("{TOKEN_PREFIX}{}", hex::encode(bytes)))
}

/// The sha256 of a token, which is what the file holds and what is compared.
fn digest_of(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

/// Whether `name` is one this deployment files a token under.
///
/// A name lands in an access line as `token=<name>` and is the argument to
/// `--token-revoke`, so it is one word: letters, digits, `-`, `_` and `.`.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

/// One token, as the file holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Issued {
    pub name: String,
    /// The sha256 of the token, lowercase hex. Never the token.
    pub hash: String,
    /// When it was issued, in seconds since the epoch, for a reader.
    pub created_at: i64,
    /// Requests per minute, refilled continuously. `0` is no limit.
    pub rpm: u32,
    /// Turns at once. `0` is no limit.
    pub concurrency: u32,
    /// Revoked rather than deleted, so that a name stays taken and an operator can
    /// see that a token was taken away rather than wonder whether they ever issued
    /// one under that name.
    pub revoked: bool,
}

/// The whole token file, which is what both the CLI and the server read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Document {
    pub version: u32,
    pub tokens: Vec<Issued>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: TOKEN_FILE_VERSION,
            tokens: Vec::new(),
        }
    }
}

impl Document {
    /// Read the file, or answer with no tokens when there is no file.
    ///
    /// A missing file is not an error: it is a deployment that has not issued one
    /// yet, which is a state an operator is in between enabling access and handing
    /// out the first token. A file that exists and cannot be read or understood is
    /// an error, because it is a file somebody meant.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let document: Self =
            serde_json::from_str(&text).map_err(|error| format!("could not parse {}: {error}", path.display()))?;
        document.check(path)?;
        Ok(document)
    }

    /// Reject a file this build cannot serve from.
    fn check(&self, path: &Path) -> Result<(), String> {
        if self.version != TOKEN_FILE_VERSION {
            return Err(format!(
                "{} says version {}, and this build reads version {TOKEN_FILE_VERSION}",
                path.display(),
                self.version
            ));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.tokens.len());
        for token in &self.tokens {
            if !valid_name(&token.name) {
                return Err(format!(
                    "{} has a token named {:?}, and a name is one word of letters, digits, `-`, `_` or `.`",
                    path.display(),
                    token.name
                ));
            }
            if decode_hash(&token.hash).is_none() {
                return Err(format!(
                    "{} has a token named {:?} whose hash is not 32 bytes of lowercase hex",
                    path.display(),
                    token.name
                ));
            }
            if seen.contains(&token.name.as_str()) {
                return Err(format!(
                    "{} names two tokens {:?}, and a name is how one of them is revoked",
                    path.display(),
                    token.name
                ));
            }
            seen.push(&token.name);
        }
        Ok(())
    }

    /// Write the file, replacing it in one step.
    ///
    /// Written beside the target and renamed over it, because the serving process
    /// reads this file while an operator writes it: a rename is the only way it
    /// reads one version or the other and never half of each.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|error| format!("could not encode the token file: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, format!("{text}\n"))
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        restrict(&temporary)?;
        std::fs::rename(&temporary, path).map_err(|error| format!("could not replace {}: {error}", path.display()))?;
        Ok(())
    }

    /// Issue a token under `name` and answer with it, once.
    pub fn issue(&mut self, name: &str, rpm: u32, concurrency: u32) -> Result<String, String> {
        if !valid_name(name) {
            return Err(format!(
                "`{name}` is not a name a token can be filed under: use one word of letters, digits, `-`, `_` or `.`"
            ));
        }
        if let Some(existing) = self.find(name) {
            return Err(if existing.revoked {
                format!("`{name}` was revoked and is still taken; issue under another name")
            } else {
                format!("`{name}` already has a token; revoke it first, or issue under another name")
            });
        }
        let token = mint()?;
        self.tokens.push(Issued {
            name: name.to_owned(),
            hash: hex::encode(digest_of(&token)),
            created_at: now_secs(),
            rpm,
            concurrency,
            revoked: false,
        });
        self.tokens.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(token)
    }

    /// Take a token away, keeping its name and its record.
    pub fn revoke(&mut self, name: &str) -> Result<(), String> {
        let Some(token) = self.tokens.iter_mut().find(|token| token.name == name) else {
            return Err(format!("no token named `{name}`; --token-list shows what is issued"));
        };
        if token.revoked {
            return Err(format!("`{name}` is already revoked"));
        }
        token.revoked = true;
        Ok(())
    }

    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Issued> {
        self.tokens.iter().find(|token| token.name == name)
    }

    /// The tokens as the serving process holds them, carrying usage over from
    /// `previous` for the entries that are unchanged.
    ///
    /// A revoked token keeps its slot until the next request notices it, and a
    /// re-issued name starts its minute over: the carried-over state belongs to
    /// the credential, not to the string that names it.
    fn held(&self, previous: &HashMap<String, Held>) -> HashMap<String, Held> {
        self.tokens
            .iter()
            .filter_map(|token| {
                let hash = decode_hash(&token.hash)?;
                let usage =
                    previous
                        .get(&token.name)
                        .filter(|old| old.hash == hash)
                        .map_or_else(Usage::default, |old| Usage {
                            allowance: old.usage.allowance,
                            at: old.usage.at,
                            requests: old.usage.requests,
                            inflight: old.usage.inflight,
                            last_used: old.usage.last_used,
                        });
                Some((
                    token.name.clone(),
                    Held {
                        hash,
                        rpm: token.rpm,
                        concurrency: token.concurrency,
                        revoked: token.revoked,
                        usage,
                    },
                ))
            })
            .collect()
    }
}

/// The upstream key, read from the file the deployment named.
///
/// Two shapes are accepted because two are worth accepting: the JSON object
/// `cmdc login` writes, and a file whose whole content is the key, which is what
/// an operator writing one by hand produces.
pub fn read_key(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read access.key_file {}: {error}", path.display()))?;
    let key = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("apiKey")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| text.trim().to_owned());
    if key.trim().is_empty() {
        return Err(format!(
            "access.key_file {} holds no key; it must be a JSON object with an `apiKey` field, or the key alone",
            path.display()
        ));
    }
    Ok(key)
}

/// A key that does not look like one is worth saying out loud, but it is not a
/// reason to refuse to start: the shape belongs to the upstream's client, and
/// guessing wrong about it should not stop a deployment that would otherwise work.
#[must_use]
pub fn key_looks_unusual(key: &str) -> bool {
    !key.starts_with("user_")
}

/// One issued token, as the serving process holds it.
struct Held {
    /// The digest the presented token has to hash to.
    hash: [u8; 32],
    rpm: u32,
    concurrency: u32,
    revoked: bool,
    usage: Usage,
}

/// What one token has been used for, since this process started.
#[derive(Default)]
struct Usage {
    /// Requests left in this token's minute.
    allowance: f64,
    /// When the allowance was last refilled. `None` until the token is first used,
    /// so a token issued weeks ago starts full rather than starting with whatever
    /// its idle time would have earned it.
    at: Option<Instant>,
    requests: u64,
    inflight: usize,
    last_used: Option<Instant>,
}

/// What one token has been used for, as `/status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub name: String,
    pub requests: u64,
    pub inflight: usize,
    /// Milliseconds since this token last served a turn, absent when it never has.
    pub idle_ms: Option<u64>,
    pub revoked: bool,
}

/// The state of the token file as it was last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

/// The tokens a deployment has issued, and the limits it enforces on them.
pub struct Access {
    /// The account key every turn this deployment serves is sent upstream with.
    key: String,
    path: PathBuf,
    stamp: Mutex<Option<Stamp>>,
    tokens: Mutex<HashMap<String, Held>>,
}

impl Access {
    /// Read the key and the tokens this deployment serves from.
    pub fn open(config: &AccessConfig) -> Result<Arc<Self>, String> {
        let key_file = config
            .key_file
            .as_ref()
            .ok_or("access.key_file is required when access.enabled is true")?;
        let key = read_key(key_file)?;
        let document = Document::read(&config.tokens_file)?;
        if document.tokens.is_empty() {
            log::warn(format!(
                "access is on and {} holds no token yet: every request will be refused until --token-new issues one",
                config.tokens_file.display()
            ));
        }
        Ok(Arc::new(Self {
            key,
            stamp: Mutex::new(stamp_of(&config.tokens_file)),
            tokens: Mutex::new(document.held(&HashMap::new())),
            path: config.tokens_file.clone(),
        }))
    }

    /// The account key every turn this deployment serves is sent with.
    ///
    /// Exposed because a catalogue fetch is the deployment's own call rather than a
    /// caller's, and it is the one path that needs the key without having
    /// authenticated anybody. Nothing that answers a client returns it.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// How many tokens are in force, revoked ones included.
    #[must_use]
    pub fn count(&self) -> usize {
        self.tokens.lock().expect("tokens poisoned").len()
    }

    /// Authenticate a presented token and reserve this turn's place.
    ///
    /// The reservation is what enforces the per-token ceiling, and it is returned
    /// to the token when the [`Caller`] is dropped, so every exit path — success,
    /// refusal, a client that disappeared — returns it exactly once.
    pub fn caller(self: &Arc<Self>, presented: &str) -> Result<Caller, Error> {
        self.reload_if_changed();
        let digest = digest_of(presented.trim());
        let mut tokens = self.tokens.lock().expect("tokens poisoned");

        // Every entry is compared, rather than stopping at the match: a loop that
        // exits early answers faster for the token it finds first, and the cost of
        // not leaking that is one comparison per token issued.
        let mut found: Option<String> = None;
        for (name, held) in tokens.iter() {
            if held.revoked {
                continue;
            }
            if constant_time_eq(&held.hash, &digest) && found.is_none() {
                found = Some(name.clone());
            }
        }

        // One message for both a token that was never issued and one that was
        // taken away: whether a name existed is not a stranger's to learn.
        let name = found.ok_or_else(|| {
            Error::authentication(
                "This deployment issues its own tokens; send one in Authorization: Bearer <token> or x-api-key, or ask its operator for one",
            )
        })?;

        let held = tokens.get_mut(&name).expect("the entry just found");
        // Turns at once first: a request refused for being one too many has not
        // been served, so it does not also cost a request of the token's minute.
        if held.concurrency > 0 && held.usage.inflight >= usize::try_from(held.concurrency).unwrap_or(usize::MAX) {
            return Err(Error {
                retry_after: Some(5),
                ..Error::unavailable(format!(
                    "Token `{name}` is limited to {} requests at once, retry shortly",
                    held.concurrency
                ))
            });
        }
        if let Some(error) = spend(&name, held) {
            return Err(error);
        }
        held.usage.inflight += 1;
        held.usage.last_used = Some(Instant::now());
        drop(tokens);

        Ok(Caller {
            key: self.key.clone(),
            identity: format!("token:{name}"),
            token: Some(name.clone()),
            _permit: Some(Permit {
                access: Arc::clone(self),
                name,
            }),
        })
    }

    /// What each token has been used for, by name.
    #[must_use]
    pub fn usage(&self) -> Vec<TokenUsage> {
        let now = Instant::now();
        let tokens = self.tokens.lock().expect("tokens poisoned");
        let mut usage: Vec<TokenUsage> = tokens
            .iter()
            .map(|(name, held)| TokenUsage {
                name: name.clone(),
                requests: held.usage.requests,
                inflight: held.usage.inflight,
                idle_ms: held
                    .usage
                    .last_used
                    .map(|at| u64::try_from(now.duration_since(at).as_millis()).unwrap_or(u64::MAX)),
                revoked: held.revoked,
            })
            .collect();
        usage.sort_by(|left, right| left.name.cmp(&right.name));
        usage
    }

    /// Re-read the file when it has changed since it was last read.
    ///
    /// One `stat` per request buys issuance and revocation without a restart,
    /// which is the half of access control an operator actually needs: a token
    /// that can only be taken away by restarting the deployment is a token that
    /// keeps working until somebody remembers.
    fn reload_if_changed(&self) {
        let stamp = stamp_of(&self.path);
        {
            let mut seen = self.stamp.lock().expect("stamp poisoned");
            if *seen == stamp {
                return;
            }
            *seen = stamp;
        }
        match Document::read(&self.path) {
            Ok(document) => {
                let count = document.tokens.len();
                let mut tokens = self.tokens.lock().expect("tokens poisoned");
                *tokens = document.held(&tokens);
                log::info(format!("access: {count} token(s) read from {}", self.path.display()));
            }
            // Keeping what is loaded rather than refusing everyone: a file an
            // operator is halfway through writing is not a reason to stop serving
            // the callers who are already authenticated.
            Err(error) => log::warn(format!("access: keeping the tokens already loaded; {error}")),
        }
    }
}

/// Enforce a token's per-minute allowance, spending one request of it.
///
/// A bucket that refills continuously rather than a counter that resets on the
/// minute: a caller at the limit waits for the fraction of a minute its next
/// request is worth instead of waiting for a wall-clock boundary, and two callers
/// cannot put twice the allowance through in the same instant by sitting on
/// either side of that boundary.
fn spend(name: &str, held: &mut Held) -> Option<Error> {
    if held.rpm == 0 {
        held.usage.requests += 1;
        return None;
    }
    let now = Instant::now();
    let capacity = f64::from(held.rpm);
    let allowance = match held.usage.at {
        Some(at) => {
            let earned = now.duration_since(at).as_secs_f64() * capacity / 60.0;
            (held.usage.allowance + earned).min(capacity)
        }
        None => capacity,
    };
    held.usage.at = Some(now);
    if allowance < 1.0 {
        held.usage.allowance = allowance;
        let wait = (60.0 * (1.0 - allowance) / capacity).ceil().max(1.0);
        return Some(Error {
            retry_after: Some(u32::try_from(wait.round() as u64).unwrap_or(u32::MAX)),
            ..Error::rate_limit(format!("Token `{name}` is limited to {} requests per minute", held.rpm))
        });
    }
    held.usage.allowance = allowance - 1.0;
    held.usage.requests += 1;
    None
}

/// A caller this deployment authenticated, and the slot it holds while it works.
pub struct Caller {
    key: String,
    identity: String,
    token: Option<String>,
    _permit: Option<Permit>,
}

impl Caller {
    /// A caller that brought its own key, in a deployment that forwards it.
    #[must_use]
    pub fn passthrough(key: String) -> Self {
        Self {
            identity: key.clone(),
            key,
            token: None,
            _permit: None,
        }
    }

    /// The key this turn is sent upstream with.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What sessions are filed under.
    ///
    /// The credential, not the key: with access on, every caller shares one key,
    /// and sessions filed under it would put a stranger's conversation in the
    /// middle of this one's context.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// The name the access line records, when this deployment issued one.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Hand the reserved place over, for a turn that outlives this call.
    ///
    /// A streamed answer is still being written when the handler that started it
    /// has returned, so the slot has to travel with the stream: a place released
    /// when the response was produced would let one caller open as many streams as
    /// it liked while holding none of them.
    pub fn take_slot(&mut self) -> Option<Permit> {
        self.token.as_ref()?;
        self._permit.take()
    }
}

/// One token's reserved place, returned when it is dropped.
pub struct Permit {
    access: Arc<Access>,
    name: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut tokens = self.access.tokens.lock().expect("tokens poisoned");
        if let Some(held) = tokens.get_mut(&self.name) {
            held.usage.inflight = held.usage.inflight.saturating_sub(1);
        }
    }
}

/// Whether two digests are equal, without answering early.
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

fn decode_hash(text: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(text).ok()?;
    let hash: [u8; 32] = bytes.try_into().ok()?;
    Some(hash)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

/// Keep the token file to the operator who wrote it.
///
/// It holds digests rather than tokens, so it is not a credential — but it is the
/// list of who may spend the account, and a list of names is not one to publish
/// either.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not restrict {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), String> {
    Ok(())
}
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use std::path::Path;

    fn scratch(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path = std::env::temp_dir().join(format!("bifrost-access-{}-{label}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn config(root: &Path) -> AccessConfig {
        let key_file = root.join("auth.json");
        std::fs::create_dir_all(root).expect("create");
        std::fs::write(&key_file, r#"{"apiKey":"user_testkey","userName":"t"}"#).expect("write key");
        AccessConfig {
            enabled: true,
            key_file: Some(key_file),
            tokens_file: root.join("tokens.json"),
        }
    }

    #[test]
    fn a_minted_token_authenticates_and_carries_the_deployment_key() {
        let root = scratch("mint");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        assert!(token.starts_with(TOKEN_PREFIX));

        let access = Access::open(&config).expect("open");
        let caller = access.caller(&token).expect("authenticated");
        assert_eq!(caller.key(), "user_testkey");
        assert_eq!(caller.token(), Some("laptop"));
        drop(caller);

        let usage = access.usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].requests, 1);
        assert_eq!(usage[0].inflight, 0, "the slot is returned when the caller drops");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_file_holds_a_digest_and_never_the_token() {
        let root = scratch("digest");
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 0).expect("issue");
        let path = root.join("tokens.json");
        document.write(&path).expect("write");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains(&token), "the token itself is not in the file");
        assert!(text.contains(&hex::encode(digest_of(&token))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_token_is_refused_and_an_issued_one_is_not() {
        let root = scratch("unknown");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");

        assert!(access.caller(&token).is_ok());
        let error = access
            .caller(&format!("{TOKEN_PREFIX}{}", "0".repeat(64)))
            .err()
            .expect("refused");
        assert_eq!(error.status, 401);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_revoked_token_stops_working_without_a_restart() {
        let root = scratch("revoke");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");
        assert!(access.caller(&token).is_ok());

        document.revoke("laptop").expect("revoke");
        document.write(&config.tokens_file).expect("write");
        assert!(access.caller(&token).is_err(), "the file changed, so the token is gone");
        assert!(access.usage()[0].revoked, "and the record says so");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn issuing_a_token_reaches_a_running_deployment() {
        let root = scratch("issue-live");
        let config = config(&root);
        let mut document = Document::default();
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");

        let token = document.issue("phone", 0, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        assert!(access.caller(&token).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_cannot_be_read_keeps_the_tokens_already_loaded() {
        let root = scratch("broken");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");

        std::fs::write(&config.tokens_file, "{ not json").expect("break it");
        assert!(
            access.caller(&token).is_ok(),
            "a file mid-edit is not a reason to refuse the callers already authenticated"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_token_with_no_limit_is_not_limited() {
        let root = scratch("unlimited");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");
        for _ in 0..50 {
            assert!(access.caller(&token).is_ok());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_minute_is_enforced_and_says_how_long_to_wait() {
        let root = scratch("rpm");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 2, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");

        assert!(access.caller(&token).is_ok());
        assert!(access.caller(&token).is_ok());
        let error = access.caller(&token).err().expect("the third is over the line");
        assert_eq!(error.status, 429);
        assert!(error.retry_after.is_some_and(|wait| (1..=60).contains(&wait)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_allowance_comes_back_with_time() {
        let root = scratch("refill");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 60, 0).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");

        for _ in 0..60 {
            assert!(access.caller(&token).is_ok());
        }
        assert!(access.caller(&token).is_err(), "the minute is spent");
        // One second of a 60-a-minute bucket is worth one request, and the sleep
        // is a little over that so the assertion is not a coin toss.
        std::thread::sleep(Duration::from_millis(1100));
        assert!(access.caller(&token).is_ok(), "and it comes back");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn turns_at_once_are_capped_per_token() {
        let root = scratch("concurrency");
        let config = config(&root);
        let mut document = Document::default();
        let token = document.issue("laptop", 0, 2).expect("issue");
        document.write(&config.tokens_file).expect("write");
        let access = Access::open(&config).expect("open");

        let first = access.caller(&token).expect("first");
        let second = access.caller(&token).expect("second");
        let error = access.caller(&token).err().expect("the third waits");
        assert_eq!(error.status, 503);
        drop(first);
        assert!(access.caller(&token).is_ok(), "and the slot comes back");
        drop(second);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_that_would_break_an_access_line_is_refused() {
        let mut document = Document::default();
        for name in ["", "two words", "with=sign", "with\nnewline"] {
            assert!(document.issue(name, 0, 0).is_err(), "{name:?} is not a name");
        }
        assert!(document.issue("laptop-1.2", 0, 0).is_ok());
    }

    #[test]
    fn the_same_name_cannot_be_issued_twice() {
        let mut document = Document::default();
        document.issue("laptop", 0, 0).expect("issue");
        assert!(document.issue("laptop", 0, 0).is_err());
        document.revoke("laptop").expect("revoke");
        assert!(document.issue("laptop", 0, 0).is_err(), "a revoked name stays taken");
        assert!(
            document.revoke("laptop").is_err(),
            "and revoking twice is a mistake worth saying"
        );
    }

    #[test]
    fn a_file_this_build_cannot_serve_from_is_refused() {
        let root = scratch("version");
        let path = root.join("tokens.json");
        std::fs::create_dir_all(&root).expect("create");
        std::fs::write(&path, r#"{"version":2,"tokens":[]}"#).expect("write");
        assert!(Document::read(&path).is_err());

        std::fs::write(&path, r#"{"version":1,"tokens":[{"name":"a b","hash":"00"}]}"#).expect("write");
        assert!(Document::read(&path).is_err(), "a name and a digest are checked too");

        std::fs::write(&path, r#"{"version":1,"tokens":[{"name":"laptop","hash":"00"}]}"#).expect("write");
        assert!(
            Document::read(&path).is_err(),
            "a hash that is not 32 bytes is not a hash"
        );

        std::fs::write(&path, r#"{"version":1,"tokens":[],"extra":1}"#).expect("write");
        assert!(
            Document::read(&path).is_err(),
            "an unknown key is a typo, not a setting"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_file_is_a_deployment_with_no_tokens() {
        let root = scratch("missing");
        let path = root.join("nothing.json");
        assert_eq!(Document::read(&path).expect("read").tokens.len(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_key_file_is_read_in_both_shapes() {
        let root = scratch("keyfile");
        std::fs::create_dir_all(&root).expect("create");
        let json = root.join("auth.json");
        std::fs::write(&json, r#"{"apiKey":"user_fromjson","authenticatedAt":"x"}"#).expect("write");
        assert_eq!(read_key(&json).expect("read"), "user_fromjson");

        let plain = root.join("key.txt");
        std::fs::write(&plain, "user_fromtext\n").expect("write");
        assert_eq!(read_key(&plain).expect("read"), "user_fromtext");

        let empty = root.join("empty.txt");
        std::fs::write(&empty, "\n").expect("write");
        assert!(read_key(&empty).is_err());

        assert!(read_key(&root.join("absent.json")).is_err());
        assert!(key_looks_unusual("sk-something"));
        assert!(!key_looks_unusual("user_something"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_digests_are_compared_without_stopping_at_the_first_difference() {
        let left = digest_of("bfr_one");
        let right = digest_of("bfr_two");
        assert!(constant_time_eq(&left, &left));
        assert!(!constant_time_eq(&left, &right));
    }
}
