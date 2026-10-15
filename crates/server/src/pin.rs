//! The household PIN gate: set once on first run, one unlock per browser.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
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

    /// Whether a PIN has been set (first run has none).
    pub fn is_set(&self) -> bool {
        self.path.exists()
    }

    /// Set the PIN. First run only: there is no change-PIN flow in M1.
    pub fn set(&self, pin: &str) -> Result<(), PinError> {
        if self.is_set() {
            return Err(PinError::AlreadySet);
        }
        // Claim the path exclusively so a racing second setter fails
        // cleanly; the rename below replaces the empty claim atomically.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.path)
        {
            Ok(claim) => drop(claim),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(PinError::AlreadySet);
            }
            Err(err) => return Err(err.into()),
        }
        let hash: PasswordHash = Argon2::default()
            .hash_password(pin.as_bytes())
            .map_err(|err| PinError::Hash(err.to_string()))?;
        let hash = hash.to_string();
        // Write-then-rename, owner-only: a torn PIN file would lock the
        // family out until the volume is fixed by hand. The temp file is
        // unique per writer, so two setters cannot tear each other's.
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(hash.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|err| err.error)?;
        tracing::info!(path = %self.path.display(), "household PIN set");
        Ok(())
    }

    /// Verify `pin` and mint the browser session that unlocks the gate.
    pub fn unlock(&self, pin: &str) -> Result<String, PinError> {
        let stored = match std::fs::read_to_string(&self.path) {
            Ok(stored) => stored,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(PinError::NotSet),
            Err(err) => return Err(err.into()),
        };
        let parsed =
            PasswordHash::new(stored.trim()).map_err(|err| PinError::Hash(err.to_string()))?;
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
