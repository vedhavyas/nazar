//! The household PIN gate: set once on first run, one unlock per browser.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};

/// Everything that can go wrong at the gate.
#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[error("pin io: {0}")]
    Io(#[from] std::io::Error),
    #[error("pin hash: {0}")]
    Hash(String),
    #[error("a PIN is already set")]
    AlreadySet,
    #[error("no PIN is set yet")]
    NotSet,
    #[error("wrong PIN")]
    Wrong,
    #[error("the PIN needs at least {MIN_PIN_LENGTH} digits")]
    TooShort,
}

/// Below this a short PIN is minutes of guessing, even behind a throttle.
pub const MIN_PIN_LENGTH: usize = 6;

/// The stored hash, when the file holds a parseable one.
fn stored_hash(path: &Path) -> Option<PasswordHash> {
    let stored = std::fs::read_to_string(path).ok()?;
    PasswordHash::new(stored.trim()).ok()
}

/// The stored PIN, and the browser sessions that have unlocked.
#[derive(Clone)]
pub struct PinGate {
    path: PathBuf,
    unlocked: Arc<Mutex<HashSet<String>>>,
}

impl PinGate {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("pin"),
            unlocked: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Whether a PIN has been set. A zero-length or unparseable file counts
    /// as not set: an interrupted write must leave the gate settable again
    /// rather than bricked.
    pub fn is_set(&self) -> bool {
        stored_hash(&self.path).is_some()
    }

    /// Set the PIN. First run only: there is no change-PIN flow in M1.
    pub fn set(&self, pin: &str) -> Result<(), PinError> {
        if pin.chars().count() < MIN_PIN_LENGTH {
            return Err(PinError::TooShort);
        }
        // A torn file from an interrupted write is not a set PIN: clear it
        // so the family can set one again.
        if self.path.exists() && !self.is_set() {
            tracing::warn!(path = %self.path.display(), "clearing an unusable PIN file");
            std::fs::remove_file(&self.path)?;
        }
        let hash: PasswordHash = Argon2::default()
            .hash_password(pin.as_bytes())
            .map_err(|err| PinError::Hash(err.to_string()))?;
        // Write-then-rename, owner-only: a torn PIN file would lock the
        // family out until the volume is fixed by hand. The temp file is
        // unique per writer, and `persist_noclobber` only lands it when no
        // PIN exists yet - the claim and the content are one step, so an
        // interruption leaves nothing half-written at the real path.
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(hash.to_string().as_bytes())?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&self.path) {
            Ok(_) => {}
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(PinError::AlreadySet);
            }
            Err(err) => return Err(err.error.into()),
        }
        tracing::info!(path = %self.path.display(), "household PIN set");
        Ok(())
    }

    /// Verify `pin` and mint the browser session that unlocks the gate.
    pub fn unlock(&self, pin: &str) -> Result<String, PinError> {
        let parsed = stored_hash(&self.path).ok_or(PinError::NotSet)?;
        Argon2::default()
            .verify_password(pin.as_bytes(), &parsed)
            .map_err(|_| PinError::Wrong)?;
        Ok(self.mint_session())
    }

    /// Mint an unlocked session. The login flow renews its cookie here.
    pub fn mint_session(&self) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.lock().insert(token.clone());
        token
    }

    /// Whether a cookie token has unlocked the gate.
    pub fn is_unlocked(&self, token: &str) -> bool {
        self.lock().contains(token)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.unlocked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
