use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{SigningKey, VerifyingKey};
use log::{info, warn};
use protocol::{EnrollError, EnrollRequest, EnrollResponse};
use rand::{rngs::SysRng, TryRng as _};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    client_id: String,
    #[serde(flatten)]
    key: StoredKey,
}

/// A signing key as it's kept on disk (the 32-byte Ed25519 seed).
///
/// On Windows it's encrypted with DPAPI for the current Windows user, so
/// only that user, on that machine, can read it back: copying the file
/// elsewhere, or reading it as another account, gets nothing usable. On
/// Unix it's plain base64 in a file only its owner can read (0600), as SSH
/// keys are.
///
/// A key saved in plain form on Windows (by an earlier version) still
/// loads, and [`StoredKey::needs_sealing`] says to save it again protected.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct StoredKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signing_key_b64: Option<String>,
    /// Windows: the seed encrypted with DPAPI (`CryptProtectData`), base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signing_key_dpapi: Option<String>,
}

impl std::fmt::Debug for StoredKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let form = match (&self.signing_key_dpapi, &self.signing_key_b64) {
            (Some(_), _) => "dpapi",
            (None, Some(_)) => "plain",
            (None, None) => "none",
        };
        write!(f, "StoredKey({form})")
    }
}

impl StoredKey {
    /// Protects `key` the way this platform stores keys.
    pub fn seal(key: &SigningKey) -> Result<StoredKey> {
        #[cfg(windows)]
        {
            let sealed = dpapi::protect(&key.to_bytes()).context("protecting the signing key with DPAPI")?;
            Ok(StoredKey {
                signing_key_b64: None,
                signing_key_dpapi: Some(STANDARD.encode(sealed)),
            })
        }
        #[cfg(not(windows))]
        {
            Ok(StoredKey {
                signing_key_b64: Some(STANDARD.encode(key.to_bytes())),
                signing_key_dpapi: None,
            })
        }
    }

    /// The key back, or `None` if there isn't one.
    pub fn open(&self) -> Result<Option<SigningKey>> {
        if let Some(sealed) = &self.signing_key_dpapi {
            let sealed = STANDARD.decode(sealed).context("decoding stored signing key")?;
            return seed_to_key(&unprotect(&sealed)?).map(Some);
        }
        match &self.signing_key_b64 {
            Some(b64) => seed_to_key(&STANDARD.decode(b64).context("decoding stored signing key")?).map(Some),
            None => Ok(None),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.signing_key_b64.is_none() && self.signing_key_dpapi.is_none()
    }

    /// A plain key on Windows, from before keys were protected: save it
    /// again (with [`StoredKey::seal`]) to protect it.
    pub fn needs_sealing(&self) -> bool {
        cfg!(windows) && self.signing_key_dpapi.is_none() && self.signing_key_b64.is_some()
    }
}

#[cfg(windows)]
fn unprotect(sealed: &[u8]) -> Result<Vec<u8>> {
    dpapi::unprotect(sealed)
        .context("decrypting the stored signing key (DPAPI): only the Windows user that saved it can read it")
}

#[cfg(not(windows))]
fn unprotect(_sealed: &[u8]) -> Result<Vec<u8>> {
    bail!("the stored signing key is protected with Windows DPAPI and can only be read on Windows")
}

fn seed_to_key(seed: &[u8]) -> Result<SigningKey> {
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored signing key is not 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The Windows Data Protection API: encryption keyed to the current user's
/// login, managed by Windows.
#[cfg(windows)]
mod dpapi {
    use anyhow::Result;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Extra input to the encryption, so another program running as the
    /// same user can't decrypt the blob just by handing it to DPAPI
    /// without knowing it came from here.
    const ENTROPY: &[u8] = b"delegation-client signing key v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copies out DPAPI's buffer, wipes it, and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: DPAPI allocated `cbData` bytes at `pbData` (LocalAlloc),
        // and they're ours to read, clear and free.
        unsafe {
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            LocalFree(Some(HLOCAL(out.pbData.cast())));
            bytes
        }
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
        let input = blob(plain);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: the input blobs point at live slices for the call's
        // duration; DPAPI only reads them.
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                Some(&entropy as *const _),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(take(out))
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>> {
        let input = blob(sealed);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy as *const _),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(take(out))
    }
}

#[derive(Clone)]
pub struct Identity {
    pub client_id: String,
    pub signing_key: SigningKey,
}

impl Identity {
    /// From a plain base64 seed: how the "Add server" form hands a new
    /// identity to the tray (over a pipe, never via disk).
    pub fn from_b64(client_id: String, signing_key_b64: &str) -> Result<Identity> {
        let seed = STANDARD.decode(signing_key_b64).context("decoding signing key")?;
        Ok(Identity {
            client_id,
            signing_key: seed_to_key(&seed)?,
        })
    }

    pub fn signing_key_b64(&self) -> String {
        STANDARD.encode(self.signing_key.to_bytes())
    }
}

/// The CLI's single-identity file. The tray app keeps one identity per
/// server in its own store instead.
pub fn identity_path() -> PathBuf {
    crate::paths::config_dir().join("identity.json")
}

/// Generates a fresh keypair and enrolls it with `server_http_base` using a
/// one-time pairing code. Blocking (plain HTTP via ureq). Nothing is saved.
pub fn enroll(server_http_base: &str, pairing_code: &str) -> Result<Identity> {
    // Straight from the OS RNG; built from bytes so ed25519-dalek and rand
    // don't have to agree on a rand_core version.
    let mut secret = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut secret)
        .map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
    let signing_key = SigningKey::from_bytes(&secret);
    let verifying_key: VerifyingKey = signing_key.verifying_key();
    let public_key_b64 = STANDARD.encode(verifying_key.to_bytes());

    let url = format!("{}/enroll", server_http_base.trim_end_matches('/'));
    info!(target: "identity", "enrolling with {url} ...");

    let response = http_agent().post(&url).send_json(ureq::json!(EnrollRequest {
        pairing_code: pairing_code.trim().to_string(),
        public_key: public_key_b64,
        hostname: crate::hostname(),
    }));

    let client_id = match response {
        Ok(resp) => {
            let body: EnrollResponse = resp.into_json().context("parsing enroll response")?;
            body.client_id
        }
        Err(ureq::Error::Status(_code, resp)) => {
            let body: EnrollError = resp.into_json().unwrap_or(EnrollError {
                error: "unknown error".into(),
            });
            bail!("enrollment rejected by server: {}", body.error);
        }
        Err(e) => bail!("enrollment request failed: {e}"),
    };
    info!(target: "identity", "enrolled, client_id={client_id}");
    Ok(Identity { client_id, signing_key })
}

#[cfg(not(windows))]
fn http_agent() -> ureq::Agent {
    ureq::agent()
}

/// ureq only uses native-tls when handed a connector explicitly.
#[cfg(windows)]
fn http_agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new();
    if let Ok(tls) = native_tls::TlsConnector::new() {
        builder = builder.tls_connector(std::sync::Arc::new(tls));
    }
    builder.build()
}

pub fn load(path: &Path) -> Result<Option<Identity>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).context("reading identity file")?;
    let file: IdentityFile = serde_json::from_str(&raw).context("parsing identity file")?;
    let signing_key = file.key.open()?.context("the identity file has no signing key")?;
    let identity = Identity {
        client_id: file.client_id,
        signing_key,
    };
    if file.key.needs_sealing() {
        match save(path, &identity) {
            Ok(()) => info!(target: "identity", "protected the saved signing key with DPAPI"),
            Err(e) => warn!(target: "identity", "couldn't protect the saved signing key: {e:#}"),
        }
    }
    Ok(Some(identity))
}

pub fn save(path: &Path, identity: &Identity) -> Result<()> {
    let file = IdentityFile {
        client_id: identity.client_id.clone(),
        key: StoredKey::seal(&identity.signing_key)?,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).context("creating config dir")?;
    }
    // A temp file renamed over the old one: loading an old plain key
    // rewrites the file, and a crash mid-write mustn't lose the identity.
    let tmp = path.with_extension("json.tmp");
    let mut f = fs::File::create(&tmp).context("creating identity file")?;
    restrict_to_owner(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
    f.sync_all().context("writing identity file")?;
    drop(f);
    fs::rename(&tmp, path).context("replacing identity file")?;
    Ok(())
}

/// Owner read/write only, since the file holds a private key. On Windows
/// the key itself is DPAPI-protected (and `%APPDATA%` is private to the
/// user anyway).
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

/// Loads the CLI's saved identity, or, if none exists yet and a pairing
/// code was supplied, enrolls with the server and saves the result.
pub fn load_or_enroll(server_http_base: &str, pairing_code: Option<&str>) -> Result<Identity> {
    let path = identity_path();
    if let Some(identity) = load(&path)? {
        info!(target: "identity", "loaded existing identity, client_id={}", identity.client_id);
        return Ok(identity);
    }
    let Some(code) = pairing_code else {
        bail!(
            "no saved identity at {:?} and no --pairing-code given. \
             Generate a pairing code from the dashboard's \"Add client\" button and pass it with --pairing-code",
            path
        );
    };
    let identity = enroll(server_http_base, code)?;
    save(&path, &identity)?;
    info!(target: "identity", "saved to {:?}", path);
    Ok(identity)
}

/// Wipes the CLI's saved identity so the next run requires a fresh
/// `--pairing-code`. Called when the server no longer recognizes our
/// client_id.
pub fn clear() {
    let _ = fs::remove_file(identity_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("delegation-identity-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("identity.json")
    }

    fn sample() -> Identity {
        Identity {
            client_id: "c1".into(),
            signing_key: SigningKey::from_bytes(&[7u8; 32]),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp_path("round-trip");
        let identity = sample();
        save(&path, &identity).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back.client_id, "c1");
        assert_eq!(back.signing_key.to_bytes(), identity.signing_key.to_bytes());
        let raw = fs::read_to_string(&path).unwrap();
        if cfg!(windows) {
            assert!(raw.contains("signing_key_dpapi"), "{raw}");
            assert!(
                !raw.contains(&identity.signing_key_b64()),
                "the plain key mustn't be on disk"
            );
        } else {
            assert!(raw.contains("signing_key_b64"), "{raw}");
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_plain_key_from_an_older_version_loads() {
        let path = temp_path("legacy");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let identity = sample();
        let legacy = format!(
            r#"{{"client_id":"c1","signing_key_b64":"{}"}}"#,
            identity.signing_key_b64()
        );
        fs::write(&path, legacy).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back.signing_key.to_bytes(), identity.signing_key.to_bytes());
        // Windows rewrites it protected; elsewhere it stays as it was.
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.contains("signing_key_dpapi"), cfg!(windows), "{raw}");
        assert_eq!(load(&path).unwrap().unwrap().client_id, "c1");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn stored_keys_never_print_the_key() {
        let key = StoredKey::seal(&sample().signing_key).unwrap();
        let shown = format!("{key:?}");
        assert!(!shown.contains(&sample().signing_key_b64()), "{shown}");
        assert!(StoredKey::default().open().unwrap().is_none());
        assert!(StoredKey::default().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_blobs_only_open_with_our_entropy_and_differ_each_time() {
        let a = dpapi::protect(b"secret").unwrap();
        let b = dpapi::protect(b"secret").unwrap();
        assert_ne!(a, b, "DPAPI salts each encryption");
        assert_eq!(dpapi::unprotect(&a).unwrap(), b"secret");
        let mut tampered = a.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(dpapi::unprotect(&tampered).is_err());
    }
}
