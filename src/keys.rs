//! L2 identities: each host and receiver has a long-term Ed25519 key. Its fingerprint, shown to people to compare, is
//! the first 16 hex digits of SHA-256 of the public key, as `XXXX-XXXX-XXXX-XXXX`.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

/// A device's long-term identity.
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// A new random identity.
    pub fn generate() -> Identity {
        Identity { key: SigningKey::generate(&mut OsRng) }
    }

    /// From the 32 secret bytes saved earlier ([`Identity::secret`]).
    pub fn from_secret(secret: &[u8; 32]) -> Identity {
        Identity { key: SigningKey::from_bytes(secret) }
    }

    /// The 32 secret bytes, for saving. Keep them private.
    pub fn secret(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    pub fn public(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public())
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }
}

/// Whether `signature` is `public`'s signature of `message`.
pub fn verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else { return false };
    key.verify(message, &Signature::from_bytes(signature)).is_ok()
}

/// `XXXX-XXXX-XXXX-XXXX`: the first 16 hex digits (upper case) of SHA-256 of the public key.
pub fn fingerprint(public: &[u8; 32]) -> String {
    let hex: String = Sha256::digest(public).iter().take(8).map(|b| format!("{b:02X}")).collect();
    format!("{}-{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify_only_with_the_right_key_and_message() {
        let a = Identity::generate();
        let b = Identity::generate();
        let sig = a.sign(b"hello");
        assert!(verify(&a.public(), b"hello", &sig));
        assert!(!verify(&a.public(), b"hullo", &sig));
        assert!(!verify(&b.public(), b"hello", &sig));
    }

    #[test]
    fn an_identity_survives_saving() {
        let a = Identity::generate();
        let b = Identity::from_secret(&a.secret());
        assert_eq!(a.public(), b.public());
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprints_look_the_same_everywhere() {
        // SHA-256 of 32 zero bytes starts 66687AADF862BD77.
        assert_eq!(fingerprint(&[0u8; 32]), "6668-7AAD-F862-BD77");
    }
}
