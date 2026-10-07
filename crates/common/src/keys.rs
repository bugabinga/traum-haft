//! The gateway's signing key (ES256) and its public JWK.
//!
//! SpacetimeDB verifies app tokens through OIDC discovery, so the public half
//! is published at `/.well-known/jwks.json`. The private half never leaves
//! the gateway's data directory.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{DecodingKey, EncodingKey};
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub struct SigningKey {
    pub kid: String,
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
    pub jwk: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("reading or writing the key file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid key: {0}")]
    Invalid(String),
}

impl SigningKey {
    /// Loads the PKCS#8 PEM key at `path`, or creates one there (mode 0600).
    pub fn load_or_create(path: &Path) -> Result<Self, KeyError> {
        let pem = match std::fs::read_to_string(path) {
            Ok(pem) => pem,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let pem = generate_pem()?;
                write_private(path, &pem)?;
                pem
            }
            Err(e) => return Err(e.into()),
        };
        Self::from_pem(&pem)
    }

    pub fn from_pem(pem: &str) -> Result<Self, KeyError> {
        let secret =
            SecretKey::from_pkcs8_pem(pem).map_err(|e| KeyError::Invalid(e.to_string()))?;
        let point = secret.public_key().to_sec1_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().ok_or_else(|| KeyError::Invalid("no x".into()))?);
        let y = URL_SAFE_NO_PAD.encode(point.y().ok_or_else(|| KeyError::Invalid("no y".into()))?);
        // RFC 7638 thumbprint as key id.
        let thumbprint_input = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let kid = URL_SAFE_NO_PAD.encode(Sha256::digest(thumbprint_input.as_bytes()));
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes())
            .map_err(|e| KeyError::Invalid(e.to_string()))?;
        let decoding = DecodingKey::from_ec_components(&x, &y)
            .map_err(|e| KeyError::Invalid(e.to_string()))?;
        let jwk = json!({ "kty": "EC", "crv": "P-256", "x": x, "y": y, "kid": kid, "use": "sig", "alg": "ES256" });
        Ok(Self {
            kid,
            encoding,
            decoding,
            jwk,
        })
    }
}

fn generate_pem() -> Result<String, KeyError> {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| KeyError::Invalid(e.to_string()))?;
        // Rejects the (astronomically rare) out-of-range scalars.
        if let Ok(secret) = SecretKey::from_slice(&bytes) {
            let pem = secret
                .to_pkcs8_pem(LineEnding::LF)
                .map_err(|e| KeyError::Invalid(e.to_string()))?;
            return Ok(pem.to_string());
        }
    }
}

/// Writes a file only its owner can read (0600). Creates it new: fails if
/// `path` exists.
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)?.write_all(contents.as_bytes())
}
