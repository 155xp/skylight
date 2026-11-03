//! Short-lived, single-use, Ed25519-signed tickets.
//!
//! The host app (your web app) holds the private key and mints a ticket per connection after its own
//! login (passkeys). Skylight only holds the public key, so a leak of Skylight's config cannot
//! be turned into access.
//!
//! ```text
//! ticket  = "sk1." base64url(claims-json) "." base64url(ed25519(signing-input))
//! signing-input = "sk1." base64url(claims-json)
//! claims  = {"v":1,"aud":"my-desktop","sub":"alice","iat":1762140000,"exp":1762140060,
//!            "jti":"<random>","scope":["view","input","clipboard","audio"],
//!            "mon":"HDMI-A-1" (optional), "ses":43200 (optional, max session seconds)}
//! ```

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "sk1";
/// Longest lifetime (exp - iat) a ticket may claim.
pub const MAX_TTL: u64 = 120;
/// Allowed clock skew between the host app and this server.
pub const SKEW: u64 = 30;
/// Longest session a ticket can open, whatever it asks for.
pub const MAX_SESSION: u64 = 24 * 3600;
pub const SCOPES: [&str; 4] = ["view", "input", "clipboard", "audio"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    pub v: u32,
    pub aud: String,
    pub sub: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    pub scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ses: Option<u64>,
}

impl Claims {
    pub fn allows(&self, scope: &str) -> bool {
        self.scope.iter().any(|s| s == scope)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TicketError {
    Malformed,
    BadSignature,
    WrongAudience,
    Expired,
    NotYetValid,
    TooLong,
    Replayed,
    BadScope,
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TicketError::Malformed => "malformed ticket",
            TicketError::BadSignature => "bad signature",
            TicketError::WrongAudience => "ticket is for another server",
            TicketError::Expired => "ticket expired",
            TicketError::NotYetValid => "ticket not valid yet",
            TicketError::TooLong => "ticket lifetime too long",
            TicketError::Replayed => "ticket already used",
            TicketError::BadScope => "bad scope",
        };
        f.write_str(s)
    }
}

impl std::error::Error for TicketError {}

pub fn mint(key: &SigningKey, claims: &Claims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let input = format!("{PREFIX}.{payload}");
    let sig = key.sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}

pub fn random_id() -> String {
    let mut b = [0u8; 18];
    getrandom::fill(&mut b).expect("OS randomness");
    URL_SAFE_NO_PAD.encode(b)
}

pub struct Verifier {
    keys: Vec<VerifyingKey>,
    audience: String,
    used: Mutex<HashMap<String, u64>>,
}

impl Verifier {
    /// Accepts tickets signed by any of `keys` (one per host app, so each can be revoked alone).
    pub fn new(keys: Vec<VerifyingKey>, audience: String) -> Verifier {
        Verifier {
            keys,
            audience,
            used: Mutex::new(HashMap::new()),
        }
    }

    /// Check a ticket at unix time `now` and spend it.
    pub fn verify(&self, ticket: &str, now: u64) -> Result<Claims, TicketError> {
        if ticket.len() > 4096 {
            return Err(TicketError::Malformed);
        }
        let mut parts = ticket.split('.');
        let (Some(prefix), Some(payload), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TicketError::Malformed);
        };
        if prefix != PREFIX {
            return Err(TicketError::Malformed);
        }
        let sig = URL_SAFE_NO_PAD
            .decode(sig)
            .map_err(|_| TicketError::Malformed)?;
        let sig = Signature::from_slice(&sig).map_err(|_| TicketError::Malformed)?;
        let input = &ticket[..prefix.len() + 1 + payload.len()];
        if !self
            .keys
            .iter()
            .any(|k| k.verify_strict(input.as_bytes(), &sig).is_ok())
        {
            return Err(TicketError::BadSignature);
        }
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| TicketError::Malformed)?;
        let claims: Claims = serde_json::from_slice(&json).map_err(|_| TicketError::Malformed)?;
        if claims.v != 1 {
            return Err(TicketError::Malformed);
        }
        if claims.aud != self.audience {
            return Err(TicketError::WrongAudience);
        }
        if claims.iat > now + SKEW {
            return Err(TicketError::NotYetValid);
        }
        if claims.exp < now {
            return Err(TicketError::Expired);
        }
        if claims.exp < claims.iat
            || claims.exp - claims.iat > MAX_TTL
            || claims.exp > now + MAX_TTL + SKEW
        {
            return Err(TicketError::TooLong);
        }
        if claims.jti.len() < 16 || claims.jti.len() > 128 {
            return Err(TicketError::Malformed);
        }
        if !claims.allows("view") || claims.scope.iter().any(|s| !SCOPES.contains(&s.as_str())) {
            return Err(TicketError::BadScope);
        }
        let mut used = self.used.lock().unwrap();
        used.retain(|_, exp| *exp + SKEW >= now);
        if used.contains_key(&claims.jti) {
            return Err(TicketError::Replayed);
        }
        used.insert(claims.jti.clone(), claims.exp);
        Ok(claims)
    }
}

// --- Key files ---------------------------------------------------------------------------------
//
// The private key is written as PKCS#8 PEM so Node's `crypto.createPrivateKey()` reads it as is.
// The public key is the raw 32 bytes in base64url (or an SPKI PEM, also accepted).

const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn pem(label: &str, der: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        STANDARD.encode(der)
    )
}

fn pem_body(text: &str) -> Option<Vec<u8>> {
    let body: String = text.lines().filter(|l| !l.starts_with("-----")).collect();
    STANDARD.decode(body.trim()).ok()
}

pub fn generate() -> SigningKey {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("OS randomness");
    SigningKey::from_bytes(&seed)
}

pub fn private_pem(key: &SigningKey) -> String {
    let mut der = PKCS8_PREFIX.to_vec();
    der.extend_from_slice(&key.to_bytes());
    pem("PRIVATE KEY", &der)
}

pub fn public_pem(key: &VerifyingKey) -> String {
    let mut der = SPKI_PREFIX.to_vec();
    der.extend_from_slice(key.as_bytes());
    pem("PUBLIC KEY", &der)
}

pub fn public_b64(key: &VerifyingKey) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

pub fn parse_private(text: &str) -> Result<SigningKey> {
    let der = pem_body(text).ok_or_else(|| anyhow!("not a PEM file"))?;
    if der.len() != 48 || der[..16] != PKCS8_PREFIX {
        bail!("not an Ed25519 PKCS#8 private key");
    }
    Ok(SigningKey::from_bytes(der[16..].try_into().unwrap()))
}

pub fn parse_public(text: &str) -> Result<VerifyingKey> {
    let text = text.trim();
    let raw: Vec<u8> = if text.starts_with("-----BEGIN") {
        let der = pem_body(text).ok_or_else(|| anyhow!("bad PEM"))?;
        if der.len() != 44 || der[..12] != SPKI_PREFIX {
            bail!("not an Ed25519 public key");
        }
        der[12..].to_vec()
    } else {
        URL_SAFE_NO_PAD
            .decode(text.trim_end_matches('='))
            .context("public key is not base64url")?
    };
    let bytes: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("public key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow!("bad public key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(now: u64) -> Claims {
        Claims {
            v: 1,
            aud: "my-desktop".into(),
            sub: "alice".into(),
            iat: now,
            exp: now + 60,
            jti: random_id(),
            scope: vec!["view".into(), "input".into()],
            mon: None,
            ses: None,
        }
    }

    fn setup() -> (SigningKey, Verifier) {
        let k = generate();
        let v = Verifier::new(vec![k.verifying_key()], "my-desktop".into());
        (k, v)
    }

    #[test]
    fn roundtrip_and_single_use() {
        let (k, v) = setup();
        let c = claims(1000);
        let t = mint(&k, &c);
        assert_eq!(v.verify(&t, 1010).unwrap(), c);
        assert_eq!(v.verify(&t, 1011), Err(TicketError::Replayed));
    }

    #[test]
    fn rejects_bad_tickets() {
        let (k, v) = setup();
        let other = generate();
        assert_eq!(
            v.verify(&mint(&other, &claims(1000)), 1000),
            Err(TicketError::BadSignature)
        );
        assert_eq!(
            v.verify(&mint(&k, &claims(1000)), 1061),
            Err(TicketError::Expired)
        );
        assert_eq!(
            v.verify(&mint(&k, &claims(1000)), 900),
            Err(TicketError::NotYetValid)
        );
        let mut c = claims(1000);
        c.aud = "elsewhere".into();
        assert_eq!(
            v.verify(&mint(&k, &c), 1000),
            Err(TicketError::WrongAudience)
        );
        let mut c = claims(1000);
        c.exp = c.iat + 3600;
        assert_eq!(v.verify(&mint(&k, &c), 1000), Err(TicketError::TooLong));
        let mut c = claims(1000);
        c.scope = vec!["input".into()];
        assert_eq!(v.verify(&mint(&k, &c), 1000), Err(TicketError::BadScope));
        let mut c = claims(1000);
        c.scope.push("root".into());
        assert_eq!(v.verify(&mint(&k, &c), 1000), Err(TicketError::BadScope));
        assert_eq!(v.verify("sk1.e30", 1000), Err(TicketError::Malformed));
        assert_eq!(v.verify("garbage", 1000), Err(TicketError::Malformed));
    }

    #[test]
    fn any_trusted_key_verifies() {
        let (a, b) = (generate(), generate());
        let v = Verifier::new(
            vec![a.verifying_key(), b.verifying_key()],
            "my-desktop".into(),
        );
        assert!(v.verify(&mint(&a, &claims(1000)), 1000).is_ok());
        assert!(v.verify(&mint(&b, &claims(1000)), 1000).is_ok());
        assert_eq!(
            v.verify(&mint(&generate(), &claims(1000)), 1000),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn tampered_payload_fails() {
        let (k, v) = setup();
        let t = mint(&k, &claims(1000));
        let mut parts: Vec<String> = t.split('.').map(String::from).collect();
        let mut c = claims(1000);
        c.scope.push("clipboard".into());
        parts[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap());
        assert_eq!(
            v.verify(&parts.join("."), 1000),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn key_files_roundtrip() {
        let k = generate();
        let back = parse_private(&private_pem(&k)).unwrap();
        assert_eq!(back.to_bytes(), k.to_bytes());
        let vk = k.verifying_key();
        assert_eq!(parse_public(&public_pem(&vk)).unwrap(), vk);
        assert_eq!(parse_public(&public_b64(&vk)).unwrap(), vk);
    }

    /// A ticket minted by `clients/node/mint-ticket.mjs` with a fixed test key (seed 0x01..0x20),
    /// so the Node reference implementation and this verifier stay compatible.
    #[test]
    fn node_minted_ticket_verifies() {
        let seed: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let k = SigningKey::from_bytes(&seed);
        let v = Verifier::new(vec![k.verifying_key()], "my-desktop".into());
        let claims = v
            .verify(NODE_TICKET, 1_762_140_010)
            .expect("node ticket verifies");
        assert_eq!(claims.sub, "alice");
        assert!(claims.allows("input"));
    }

    const NODE_TICKET: &str = include_str!("../clients/node/testdata/ticket.txt");
}
