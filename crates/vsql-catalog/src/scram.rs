//! SCRAM-SHA-256 (RFC 5802, RFC 7677) for role passwords.
//!
//! PostgreSQL's default password mechanism since v10 is `scram-sha-256`, and
//! this module implements both halves of it:
//!
//! * **storing** — a password is never kept in the clear. It is turned into a
//!   *verifier* (salt, iteration count, `StoredKey`, `ServerKey`), which is what
//!   `pg_authid.rolpassword` holds.
//! * **proving** — during authentication the server never sees the password
//!   either: the client sends a proof that the verifier can check, and gets back
//!   a server signature that proves the server also holds the verifier.
//!
//! # Why `StoredKey` and `ServerKey` and not just the salted password
//!
//! The verifier must be usable to *check* a client without being usable to
//! *impersonate* one. `StoredKey = SHA256(HMAC(SaltedPassword, "Client Key"))`
//! is a one-way image of the client key, and `ServerKey` is a different HMAC of
//! the same salted password, so leaking the verifier does not let an attacker
//! log in as the client.
//!
//! # Iteration count
//!
//! [`DEFAULT_ITERATIONS`] matches PostgreSQL's `SCRAM_SHA_256_DEFAULT_ITERATIONS`.
//! PBKDF2 is deliberately slow; that is the point, and the stored format records
//! the count so old verifiers keep working when the default changes.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Iterations used for a freshly created verifier.
pub const DEFAULT_ITERATIONS: u32 = 4096;

/// The mechanism name as it appears in the protocol.
pub const MECHANISM: &str = "SCRAM-SHA-256";

/// Prefix of PostgreSQL's stored password format.
const STORED_PREFIX: &str = "SCRAM-SHA-256$";

/// Salt length; PostgreSQL uses 16 random bytes.
const SALT_BYTES: usize = 16;

/// A stored SCRAM-SHA-256 verifier.
#[derive(Clone, PartialEq, Eq)]
pub struct ScramVerifier {
    /// PBKDF2 iteration count.
    pub iterations: u32,
    /// Per-role salt.
    pub salt: Vec<u8>,
    /// `SHA256(HMAC(SaltedPassword, "Client Key"))`.
    pub stored_key: [u8; 32],
    /// `HMAC(SaltedPassword, "Server Key")`.
    pub server_key: [u8; 32],
}

impl std::fmt::Debug for ScramVerifier {
    /// Redacted: key material must never reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScramVerifier")
            .field("iterations", &self.iterations)
            .field("salt_bytes", &self.salt.len())
            .field("stored_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ScramVerifier {
    /// Derives a verifier from a plaintext password, with a fresh random salt.
    pub fn new(password: &str) -> Self {
        Self::with_salt(password.as_bytes(), &random_salt(), DEFAULT_ITERATIONS)
    }

    /// Derives a verifier from a password and an explicit salt.
    pub fn with_salt(password: &[u8], salt: &[u8], iterations: u32) -> Self {
        let salted_password = pbkdf2_hmac_sha256(password, salt, iterations);
        ScramVerifier {
            iterations,
            salt: salt.to_vec(),
            stored_key: Sha256::digest(hmac_sha256(&salted_password, b"Client Key")).into(),
            server_key: hmac_sha256(&salted_password, b"Server Key"),
        }
    }

    /// Parses PostgreSQL's `SCRAM-SHA-256$<iterations>:<salt>$<stored>:<server>`.
    pub fn parse(text: &str) -> Option<ScramVerifier> {
        let rest = text.strip_prefix(STORED_PREFIX)?;
        let (parameters, keys) = rest.split_once('$')?;
        let (iterations, salt) = parameters.split_once(':')?;
        let (stored_key, server_key) = keys.split_once(':')?;
        Some(ScramVerifier {
            iterations: iterations.parse().ok()?,
            salt: BASE64.decode(salt).ok()?,
            stored_key: to_array(&BASE64.decode(stored_key).ok()?)?,
            server_key: to_array(&BASE64.decode(server_key).ok()?)?,
        })
    }

    /// Renders PostgreSQL's stored password format.
    pub fn to_stored(&self) -> String {
        format!(
            "{STORED_PREFIX}{}:{}${}:{}",
            self.iterations,
            BASE64.encode(&self.salt),
            BASE64.encode(self.stored_key),
            BASE64.encode(self.server_key)
        )
    }

    /// `true` when `password` derives this verifier.
    pub fn verify_password(&self, password: &str) -> bool {
        let salted_password = pbkdf2_hmac_sha256(password.as_bytes(), &self.salt, self.iterations);
        let client_key = hmac_sha256(&salted_password, b"Client Key");
        let stored_key: [u8; 32] = Sha256::digest(client_key).into();
        stored_key.ct_eq(&self.stored_key).into()
    }

    /// Checks a client proof and returns the server signature to send back.
    ///
    /// `auth_message` is the concatenation from RFC 5802 §3:
    /// `client-first-bare + "," + server-first + "," + client-final-without-proof`.
    /// Returns `None` when the proof does not match, which is what makes a wrong
    /// password indistinguishable from a forgery attempt.
    pub fn verify_proof(&self, auth_message: &str, proof: &[u8]) -> Option<[u8; 32]> {
        let client_signature = hmac_sha256(&self.stored_key, auth_message.as_bytes());
        if proof.len() != client_signature.len() {
            return None;
        }
        // ClientKey = ClientProof XOR ClientSignature, and the client proves it
        // knows ClientKey by sending its hash.
        let client_key: Vec<u8> = proof
            .iter()
            .zip(client_signature.iter())
            .map(|(proof, signature)| proof ^ signature)
            .collect();
        let stored_key: [u8; 32] = Sha256::digest(&client_key).into();
        if !bool::from(stored_key.ct_eq(&self.stored_key)) {
            return None;
        }
        Some(hmac_sha256(&self.server_key, auth_message.as_bytes()))
    }

    /// The server signature for an already verified exchange.
    pub fn server_signature(&self, auth_message: &str) -> [u8; 32] {
        hmac_sha256(&self.server_key, auth_message.as_bytes())
    }
}

/// PBKDF2-HMAC-SHA-256 with a 32-byte output (RFC 8018 §5.2, one block).
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let first = {
        let mut mac = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
        mac.update(salt);
        mac.update(&1u32.to_be_bytes());
        let digest: [u8; 32] = mac.finalize().into_bytes().into();
        digest
    };
    let mut result = first;
    let mut previous = first;
    // `iterations` counts the first application too, and at least one must run.
    for _ in 1..iterations.max(1) {
        previous = hmac_sha256(password, &previous);
        for (out, byte) in result.iter_mut().zip(previous.iter()) {
            *out ^= *byte;
        }
    }
    result
}

/// SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// HMAC-SHA-256.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Reads the value of a `k=v` attribute from a SCRAM message.
///
/// The attributes are comma separated and every one of them is a single
/// character, so a plain scan is enough - and it must stay a scan, because
/// values may themselves contain `=` (base64 padding).
pub fn attribute<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    message
        .split(',')
        .find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
}

/// `count` random bytes.
pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; count];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

/// A server nonce: 18 random bytes, which base64-encode to 24 characters with
/// no padding - the same shape PostgreSQL sends.
pub fn random_nonce() -> String {
    base64_encode(&random_bytes(18))
}

/// Encodes bytes as base64, as the protocol's messages require.
pub fn base64_encode(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// Decodes base64, rejecting anything malformed.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    BASE64.decode(text.trim()).ok()
}

fn random_salt() -> [u8; SALT_BYTES] {
    let mut salt = [0u8; SALT_BYTES];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

fn to_array(bytes: &[u8]) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7677 §3: the published SCRAM-SHA-256 exchange for `user`/`pencil`.
    const RFC_CLIENT_FIRST_BARE: &str = "n=user,r=rOprNGfwEbeRWgbNEkqO";
    const RFC_SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const RFC_CLIENT_FINAL_WITHOUT_PROOF: &str =
        "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    const RFC_PROOF: &str = "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
    const RFC_SERVER_SIGNATURE: &str = "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";

    fn rfc_verifier() -> ScramVerifier {
        // The vector publishes the salt, so the verifier is reconstructed from it
        // rather than from a random one.
        let salt = base64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        ScramVerifier::with_salt(b"pencil", &salt, 4096)
    }

    fn rfc_auth_message() -> String {
        format!("{RFC_CLIENT_FIRST_BARE},{RFC_SERVER_FIRST},{RFC_CLIENT_FINAL_WITHOUT_PROOF}")
    }

    #[test]
    fn matches_the_rfc_7677_vector() {
        let verifier = rfc_verifier();
        let auth_message = rfc_auth_message();
        let proof = base64_decode(RFC_PROOF).unwrap();

        let signature = verifier
            .verify_proof(&auth_message, &proof)
            .expect("the published proof must verify");
        assert_eq!(
            base64_encode(&signature),
            RFC_SERVER_SIGNATURE,
            "the server signature must match the vector"
        );
    }

    #[test]
    fn a_wrong_proof_is_rejected() {
        let verifier = rfc_verifier();
        let auth_message = rfc_auth_message();
        let mut proof = base64_decode(RFC_PROOF).unwrap();
        proof[0] ^= 0x01;
        assert!(verifier.verify_proof(&auth_message, &proof).is_none());
        // A tampered auth message must also fail.
        assert!(verifier
            .verify_proof(
                &format!("{auth_message}x"),
                &base64_decode(RFC_PROOF).unwrap()
            )
            .is_none());
        // ... and so must a truncated proof.
        assert!(verifier.verify_proof(&auth_message, &[0u8; 3]).is_none());
    }

    #[test]
    fn passwords_round_trip_through_the_stored_format() {
        let verifier = ScramVerifier::new("s3cret");
        let stored = verifier.to_stored();
        assert!(stored.starts_with("SCRAM-SHA-256$4096:"), "{stored}");

        let parsed = ScramVerifier::parse(&stored).expect("the format is readable");
        assert_eq!(parsed, verifier);
        assert!(parsed.verify_password("s3cret"));
        assert!(!parsed.verify_password("s3cre"));
        assert!(!parsed.verify_password(""));

        // Only the documented format is accepted.
        assert!(ScramVerifier::parse("md5deadbeef").is_none());
        assert!(ScramVerifier::parse("SCRAM-SHA-256$4096:zzz$a:b").is_none());
    }

    #[test]
    fn salts_are_random_so_equal_passwords_differ() {
        let first = ScramVerifier::new("same");
        let second = ScramVerifier::new("same");
        assert_ne!(first.salt, second.salt);
        assert_ne!(first.to_stored(), second.to_stored());
        // Both still verify their own password.
        assert!(first.verify_password("same") && second.verify_password("same"));
    }

    #[test]
    fn pbkdf2_matches_the_published_sha256_vector() {
        // P="password", S="salt", c=1, dkLen=32.
        let derived = pbkdf2_hmac_sha256(b"password", b"salt", 1);
        assert_eq!(
            derived.to_vec(),
            [
                0x12, 0x0f, 0xb6, 0xcf, 0xfc, 0xf8, 0xb3, 0x2c, 0x43, 0xe7, 0x22, 0x52, 0x56, 0xc4,
                0xf8, 0x37, 0xa8, 0x65, 0x48, 0xc9, 0x2c, 0xcc, 0x35, 0x48, 0x08, 0x05, 0x98, 0x7c,
                0xb7, 0x0b, 0xe1, 0x7b
            ]
        );
        // One iteration must equal a single HMAC, which pins the loop bounds.
        assert_eq!(derived, hmac_sha256(b"password", b"salt\x00\x00\x00\x01"));
        // ... and the iteration count must actually matter.
        assert_ne!(derived, pbkdf2_hmac_sha256(b"password", b"salt", 2));
    }
}
