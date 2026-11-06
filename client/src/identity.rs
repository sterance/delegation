use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{SigningKey, VerifyingKey};
use log::info;
use protocol::{EnrollError, EnrollRequest, EnrollResponse};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    client_id: String,
    signing_key_b64: String, // 32-byte seed, base64
}

#[derive(Clone)]
pub struct Identity {
    pub client_id: String,
    pub signing_key: SigningKey,
}

impl Identity {
    pub fn from_b64(client_id: String, signing_key_b64: &str) -> Result<Identity> {
        let seed_bytes = STANDARD
            .decode(signing_key_b64)
            .context("decoding stored signing key")?;
        let seed: [u8; 32] = seed_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored signing key is not 32 bytes"))?;
        Ok(Identity {
            client_id,
            signing_key: SigningKey::from_bytes(&seed),
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
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
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
    Identity::from_b64(file.client_id, &file.signing_key_b64).map(Some)
}

pub fn save(path: &Path, identity: &Identity) -> Result<()> {
    let file = IdentityFile {
        client_id: identity.client_id.clone(),
        signing_key_b64: identity.signing_key_b64(),
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).context("creating config dir")?;
    }
    let mut f = fs::File::create(path).context("creating identity file")?;
    f.write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
    restrict_to_owner(path)?;
    Ok(())
}

/// Owner read/write only, since the file holds a private key. Files under
/// `%APPDATA%` are already private to the user on Windows.
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
