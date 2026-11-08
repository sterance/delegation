//! Argon2id password hashing, stored as PHC strings. Argon2 is slow and
//! memory-hungry on purpose, so it runs on the blocking pool, a few at a
//! time: a burst of logins can't eat the server's memory or stall the
//! async threads.

use crate::config::Argon2Settings;
use anyhow::{anyhow, Context, Result};
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use rand::Rng;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Hashes computed at once. Each takes `AUTH_ARGON2_MEMORY_KIB` of memory.
const CONCURRENT_HASHES: usize = 4;

pub struct Hasher {
    argon: Argon2<'static>,
    settings: Argon2Settings,
    permits: Semaphore,
    /// Verified against when the username doesn't exist, so a login takes
    /// as long whether or not it does.
    dummy: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    pub ok: bool,
    /// The stored hash used weaker parameters than the configured ones.
    pub needs_rehash: bool,
}

impl Hasher {
    pub fn new(settings: Argon2Settings) -> Result<Arc<Self>> {
        let params = Params::new(settings.memory_kib, settings.iterations, settings.parallelism, None)
            .map_err(|e| anyhow!("invalid Argon2 parameters: {e}"))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let random: [u8; 16] = rand::thread_rng().gen();
        let dummy = hash_with(&argon, &format!("{random:?}"))?;
        Ok(Arc::new(Hasher {
            argon,
            settings,
            permits: Semaphore::new(CONCURRENT_HASHES),
            dummy,
        }))
    }

    pub fn hash_blocking(&self, password: &str) -> Result<String> {
        hash_with(&self.argon, password)
    }

    /// `stored` None means there's no such user: the dummy hash is checked
    /// instead, and the result is always a failure.
    pub fn verify_blocking(&self, stored: Option<&str>, password: &str) -> Verified {
        let Some(stored) = stored else {
            let _ = self.check(&self.dummy, password);
            return Verified {
                ok: false,
                needs_rehash: false,
            };
        };
        let ok = self.check(stored, password);
        Verified {
            ok,
            needs_rehash: ok && self.weaker_than_configured(stored),
        }
    }

    fn check(&self, stored: &str, password: &str) -> bool {
        // The stored string carries its own parameters; verify uses those.
        PasswordHash::new(stored).is_ok_and(|parsed| self.argon.verify_password(password.as_bytes(), &parsed).is_ok())
    }

    fn weaker_than_configured(&self, stored: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(stored) else {
            return true;
        };
        if parsed.algorithm != argon2::ARGON2ID_IDENT || parsed.version != Some(Version::V0x13.into()) {
            return true;
        }
        let Ok(params) = Params::try_from(&parsed) else {
            return true;
        };
        params.m_cost() < self.settings.memory_kib
            || params.t_cost() < self.settings.iterations
            || params.p_cost() < self.settings.parallelism
    }

    pub async fn hash(self: &Arc<Self>, password: String) -> Result<String> {
        let _permit = self.permits.acquire().await.context("hasher closed")?;
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.hash_blocking(&password))
            .await
            .context("hashing task panicked")?
    }

    pub async fn verify(self: &Arc<Self>, stored: Option<String>, password: String) -> Result<Verified> {
        let _permit = self.permits.acquire().await.context("hasher closed")?;
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.verify_blocking(stored.as_deref(), &password))
            .await
            .context("verifying task panicked")
    }
}

fn hash_with(argon: &Argon2<'_>, password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing failed: {e}"))?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const FAST: Argon2Settings = Argon2Settings {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    };

    #[test]
    fn hashes_verify_and_unknown_users_always_fail() {
        let hasher = Hasher::new(FAST).unwrap();
        let hash = hasher.hash_blocking("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=64,t=1,p=1$"));
        assert_eq!(
            hasher.verify_blocking(Some(&hash), "correct horse battery"),
            Verified {
                ok: true,
                needs_rehash: false
            }
        );
        assert!(!hasher.verify_blocking(Some(&hash), "wrong").ok);
        assert!(!hasher.verify_blocking(None, "anything").ok);
        assert!(!hasher.verify_blocking(Some("not a hash"), "x").ok);
    }

    #[test]
    fn weaker_hashes_are_flagged_for_rehash() {
        let weak = Hasher::new(FAST).unwrap();
        let hash = weak.hash_blocking("pw").unwrap();
        let strong = Hasher::new(Argon2Settings {
            memory_kib: 128,
            ..FAST
        })
        .unwrap();
        assert_eq!(
            strong.verify_blocking(Some(&hash), "pw"),
            Verified {
                ok: true,
                needs_rehash: true
            }
        );
        // Stronger than configured is left alone.
        let hash = strong.hash_blocking("pw").unwrap();
        assert!(!weak.verify_blocking(Some(&hash), "pw").needs_rehash);
    }
}
