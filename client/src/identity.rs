use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    client_id: String,
    signing_key_b64: String, // 32-byte seed, base64
}

pub struct Identity {
    pub client_id: String,
    pub signing_key: SigningKey,
}

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("delegation-agent")
}

fn identity_path() -> PathBuf {
    config_dir().join("identity.json")
}

/// Loads a saved identity, or — if none exists yet and a pairing code was
/// supplied — enrolls with the server and saves the result.
///
/// Linux-only for this scaffold (uses unix file permission bits directly
/// to lock the private key file down to the owner). The Windows/macOS
/// ports need their own equivalent before this agent runs there.
pub fn load_or_enroll(server_http_base: &str, pairing_code: Option<&str>) -> Result<Identity> {
    let path = identity_path();

    if path.exists() {
        let raw = fs::read_to_string(&path).context("reading identity file")?;
        let file: IdentityFile = serde_json::from_str(&raw).context("parsing identity file")?;
        let seed_bytes = STANDARD
            .decode(&file.signing_key_b64)
            .context("decoding stored signing key")?;
        let seed: [u8; 32] = seed_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored signing key is not 32 bytes"))?;
        let signing_key = SigningKey::from_bytes(&seed);
        println!("[identity] loaded existing identity, client_id={}", file.client_id);
        return Ok(Identity { client_id: file.client_id, signing_key });
    }

    let Some(code) = pairing_code else {
        bail!(
            "no saved identity at {:?} and no --pairing-code given. \
             Generate a pairing code on the server (npm run gen-code) and pass it with --pairing-code",
            path
        );
    };

    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let verifying_key: VerifyingKey = signing_key.verifying_key();
    let public_key_b64 = STANDARD.encode(verifying_key.to_bytes());

    let hostname = hostname_string();

    #[derive(Serialize)]
    struct EnrollRequest<'a> {
        pairing_code: &'a str,
        public_key: &'a str,
        hostname: &'a str,
    }
    #[derive(Deserialize)]
    struct EnrollResponse {
        client_id: String,
    }
    #[derive(Deserialize)]
    struct EnrollError {
        error: String,
    }

    let url = format!("{}/enroll", server_http_base.trim_end_matches('/'));
    println!("[identity] enrolling with {url} ...");

    let response = ureq::post(&url).send_json(ureq::json!(EnrollRequest {
        pairing_code: code,
        public_key: &public_key_b64,
        hostname: &hostname,
    }));

    let client_id = match response {
        Ok(resp) => {
            let body: EnrollResponse = resp.into_json().context("parsing enroll response")?;
            body.client_id
        }
        Err(ureq::Error::Status(_code, resp)) => {
            let body: EnrollError = resp
                .into_json()
                .unwrap_or(EnrollError { error: "unknown error".into() });
            bail!("enrollment rejected by server: {}", body.error);
        }
        Err(e) => bail!("enrollment request failed: {e}"),
    };

    let file = IdentityFile {
        client_id: client_id.clone(),
        signing_key_b64: STANDARD.encode(signing_key.to_bytes()),
    };
    fs::create_dir_all(config_dir()).context("creating config dir")?;
    let mut f = fs::File::create(&path).context("creating identity file")?;
    f.write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
    // Owner read/write only — this file holds a private key.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

    println!("[identity] enrolled, client_id={client_id} (saved to {:?})", path);
    Ok(Identity { client_id, signing_key })
}

fn hostname_string() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown-host".to_string())
}
