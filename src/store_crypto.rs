//! ADR-027 total-store envelope encryption.

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use zeroize::Zeroizing;

const ENVELOPE_PREFIX: &str = "sv1";
const DATA_ENVELOPE_PREFIX: &str = "sv2";
const WRAPPED_KEY_PREFIX: &str = "svk1";
const KEYCHAIN_SERVICE: &str = "session-vault";
const KEYCHAIN_ACCOUNT: &str = "total-store-master-v1";
/// 设了就从这个文件取主密钥，不碰 OS 密钥链 —— 无桌面的 Linux 没有 Secret Service。
pub const KEY_FILE_ENV: &str = "SVAULT_KEY_FILE";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("total-store key is missing (OS keychain, or the file named by SVAULT_KEY_FILE)")]
    MissingKey,
    #[error("key file {0}")]
    KeyFile(String),
    /// 本机已经有另一把主密钥：装上新的会让旧钥匙加密的数据再也打不开。换钥还没做。
    #[error("this machine already has a different total-store key; importing would orphan data encrypted under it (re-keying is not supported yet)")]
    KeyConflict,
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

    pub fn from_encoded(value: &str) -> Result<Self, CryptoError> {
        Self::decode(value)
    }

    pub(crate) fn same_as(&self, other: &StoreKey) -> bool {
        *self.0 == *other.0
    }

    pub(crate) fn duplicate(&self) -> StoreKey {
        Self(Zeroizing::new(*self.0))
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
        self.decrypt_with_prefix(envelope, ENVELOPE_PREFIX, aad)
    }

    pub(crate) fn encrypt_data(
        &self,
        key_id: &str,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<String, CryptoError> {
        self.encrypt_with_prefix(&format!("{DATA_ENVELOPE_PREFIX}:{key_id}"), plaintext, aad)
    }

    pub(crate) fn decrypt_data(
        &self,
        envelope: &str,
        key_id: &str,
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let prefix = format!("{DATA_ENVELOPE_PREFIX}:{key_id}");
        self.decrypt_with_prefix(envelope, &prefix, aad)
    }

    pub(crate) fn wrap_key(&self, key: &StoreKey, aad: &[u8]) -> Result<String, CryptoError> {
        self.encrypt_with_prefix(WRAPPED_KEY_PREFIX, key.0.as_ref(), aad)
    }

    pub(crate) fn unwrap_key(&self, wrapped: &str, aad: &[u8]) -> Result<StoreKey, CryptoError> {
        let plaintext = self.decrypt_with_prefix(wrapped, WRAPPED_KEY_PREFIX, aad)?;
        let bytes: [u8; 32] = plaintext.try_into().map_err(|_| CryptoError::InvalidKey)?;
        Ok(StoreKey::from_bytes(bytes))
    }

    fn encrypt_with_prefix(
        &self,
        prefix: &str,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<String, CryptoError> {
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
            "{prefix}:{}:{}",
            STANDARD_NO_PAD.encode(nonce_bytes),
            STANDARD_NO_PAD.encode(ciphertext)
        ))
    }

    fn decrypt_with_prefix(
        &self,
        envelope: &str,
        expected_prefix: &str,
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let mut parts = envelope.split(':');
        for expected in expected_prefix.split(':') {
            if parts.next() != Some(expected) {
                return Err(CryptoError::Decrypt);
            }
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
    value.starts_with("sv1:") || value.starts_with("sv2:")
}

pub(crate) fn data_key_id(envelope: &str) -> Result<&str, CryptoError> {
    let mut parts = envelope.split(':');
    if parts.next() != Some(DATA_ENVELOPE_PREFIX) {
        return Err(CryptoError::Decrypt);
    }
    let key_id = parts.next().ok_or(CryptoError::Decrypt)?;
    if key_id.is_empty() {
        return Err(CryptoError::Decrypt);
    }
    Ok(key_id)
}

pub(crate) fn new_data_key_id() -> String {
    use aes_gcm::aead::rand_core::RngCore;
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    STANDARD_NO_PAD.encode(bytes)
}

/// 总库标识：16 字节随机数的十六进制 —— 中转服务器上直接拿它当文件名。
pub(crate) fn new_store_id() -> String {
    use aes_gcm::aead::rand_core::RngCore;
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 主密钥从哪来：OS 密钥链，或 [`KEY_FILE_ENV`] 指定的文件。
pub(crate) enum KeySource {
    OsKeychain,
    File(std::path::PathBuf),
}

impl KeySource {
    pub(crate) fn from_env() -> Self {
        match std::env::var_os(KEY_FILE_ENV) {
            Some(path) if !path.is_empty() => Self::File(path.into()),
            _ => Self::OsKeychain,
        }
    }

    pub(crate) fn load(&self) -> Result<Option<StoreKey>, CryptoError> {
        match self {
            Self::OsKeychain => load_os_key(),
            Self::File(path) => load_key_file(path),
        }
    }

    pub(crate) fn create(&self) -> Result<StoreKey, CryptoError> {
        let key = StoreKey::generate();
        self.install(&key)?;
        Ok(key)
    }

    /// 把给定的钥匙放进这个来源。文件来源只新建、不覆盖；密钥链来源由调用方先确认里面没有别的钥匙。
    pub(crate) fn install(&self, key: &StoreKey) -> Result<(), CryptoError> {
        match self {
            Self::OsKeychain => keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
                .and_then(|entry| entry.set_password(&key.encode()))
                .map_err(|e| CryptoError::Keychain(e.to_string())),
            Self::File(path) => crate::probe::create_private_file(path, key.encode())
                .map_err(|e| CryptoError::KeyFile(format!("{}: {e}", path.display()))),
        }
    }
}

/// 文件不在是 `Ok(None)`；对组或其他用户开放了权限就拒绝，不读。
pub(crate) fn load_key_file(path: &std::path::Path) -> Result<Option<StoreKey>, CryptoError> {
    use crate::probe::Probed;
    let refuse = |why: String| CryptoError::KeyFile(format!("{}: {why}", path.display()));
    match crate::probe::open_to_others(path) {
        Probed::Absent => return Ok(None),
        Probed::Found(true) => return Err(refuse("readable by group or others; chmod 600".into())),
        Probed::Found(false) => {}
        Probed::Unknown(e) => return Err(refuse(e.to_string())),
    }
    match crate::probe::read_text(path, None) {
        Probed::Found(text) => StoreKey::decode(text.trim()).map(Some),
        Probed::Absent => Ok(None),
        Probed::Unknown(e) => Err(refuse(e.to_string())),
    }
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
