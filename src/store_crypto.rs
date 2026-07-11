//! ADR-027 total-store envelope encryption.

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use zeroize::Zeroizing;

const ENVELOPE_PREFIX: &str = "sv1";
const KEYCHAIN_SERVICE: &str = "session-vault";
const KEYCHAIN_ACCOUNT: &str = "total-store-master-v1";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("total-store key is missing from the OS keychain")]
    MissingKey,
    #[error("invalid total-store key")]
    InvalidKey,
    #[error("OS keychain: {0}")]
    Keychain(String),
    #[error("total-store encryption failed")]
    Encrypt,
    #[error("total-store authentication/decryption failed")]
    Decrypt,
}

/// A 256-bit total-store key. Key bytes are zeroized when the value is dropped.
pub struct StoreKey(Zeroizing<[u8; 32]>);

impl StoreKey {
    pub fn generate() -> Self {
        use aes_gcm::aead::rand_core::RngCore;
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(Zeroizing::new(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    fn encode(&self) -> String {
        STANDARD_NO_PAD.encode(self.0.as_ref())
    }

    fn decode(value: &str) -> Result<Self, CryptoError> {
        let decoded = STANDARD_NO_PAD
            .decode(value)
            .map_err(|_| CryptoError::InvalidKey)?;
        let bytes: [u8; 32] = decoded.try_into().map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self::from_bytes(bytes))
    }
}

pub(crate) struct StoreCipher {
    cipher: Aes256Gcm,
}

impl StoreCipher {
    pub(crate) fn new(key: StoreKey) -> Self {
        let cipher = Aes256Gcm::new_from_slice(key.0.as_ref()).expect("AES-256 key length");
        Self { cipher }
    }

    pub(crate) fn encrypt(&self, plaintext: &[u8], aad: &[u8]) -> Result<String, CryptoError> {
        use aes_gcm::aead::rand_core::RngCore;
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::Encrypt)?;
        Ok(format!(
            "{ENVELOPE_PREFIX}:{}:{}",
            STANDARD_NO_PAD.encode(nonce_bytes),
            STANDARD_NO_PAD.encode(ciphertext)
        ))
    }

    pub(crate) fn decrypt(&self, envelope: &str, aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut parts = envelope.split(':');
        if parts.next() != Some(ENVELOPE_PREFIX) {
            return Err(CryptoError::Decrypt);
        }
        let nonce = parts
            .next()
            .ok_or(CryptoError::Decrypt)
            .and_then(|v| STANDARD_NO_PAD.decode(v).map_err(|_| CryptoError::Decrypt))?;
        let ciphertext = parts
            .next()
            .ok_or(CryptoError::Decrypt)
            .and_then(|v| STANDARD_NO_PAD.decode(v).map_err(|_| CryptoError::Decrypt))?;
        if parts.next().is_some() || nonce.len() != 12 {
            return Err(CryptoError::Decrypt);
        }
        self.cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext.as_ref(),
                    aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt)
    }
}

pub(crate) fn is_envelope(value: &str) -> bool {
    value.starts_with("sv1:")
}

pub(crate) fn load_os_key() -> Result<Option<StoreKey>, CryptoError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| CryptoError::Keychain(e.to_string()))?;
    match entry.get_password() {
        Ok(value) => StoreKey::decode(&value).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(CryptoError::Keychain(e.to_string())),
    }
}

pub(crate) fn create_os_key() -> Result<StoreKey, CryptoError> {
    let key = StoreKey::generate();
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| CryptoError::Keychain(e.to_string()))?;
    entry
        .set_password(&key.encode())
        .map_err(|e| CryptoError::Keychain(e.to_string()))?;
    Ok(key)
}
