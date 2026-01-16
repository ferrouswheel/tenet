use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hpke::aead::ChaCha20Poly1305 as HpkeChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use rand::{rngs::OsRng, CryptoRng, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::Path;

pub const CONTENT_KEY_SIZE: usize = 32;
pub const NONCE_SIZE: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedKey {
    pub enc: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug)]
pub enum CryptoError {
    InvalidLength(&'static str),
    Hpke(hpke::HpkeError),
    Aead(chacha20poly1305::aead::Error),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::InvalidLength(message) => write!(f, "invalid length: {message}"),
            CryptoError::Hpke(error) => write!(f, "hpke error: {error}"),
            CryptoError::Aead(error) => write!(f, "aead error: {error}"),
        }
    }
}

impl std::error::Error for CryptoError {}

impl From<hpke::HpkeError> for CryptoError {
    fn from(error: hpke::HpkeError) -> Self {
        CryptoError::Hpke(error)
    }
}

impl From<chacha20poly1305::aead::Error> for CryptoError {
    fn from(error: chacha20poly1305::aead::Error) -> Self {
        CryptoError::Aead(error)
    }
}

#[derive(Debug)]
pub enum KeyStoreError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Hex(hex::FromHexError),
}

impl fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyStoreError::Io(error) => write!(f, "io error: {error}"),
            KeyStoreError::Serde(error) => write!(f, "serde error: {error}"),
            KeyStoreError::Hex(error) => write!(f, "hex error: {error}"),
        }
    }
}

impl std::error::Error for KeyStoreError {}

impl From<std::io::Error> for KeyStoreError {
    fn from(error: std::io::Error) -> Self {
        KeyStoreError::Io(error)
    }
}

impl From<serde_json::Error> for KeyStoreError {
    fn from(error: serde_json::Error) -> Self {
        KeyStoreError::Serde(error)
    }
}

impl From<hex::FromHexError> for KeyStoreError {
    fn from(error: hex::FromHexError) -> Self {
        KeyStoreError::Hex(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredKeypair {
    pub id: String,
    pub public_key_hex: String,
    pub private_key_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRotation {
    pub previous_id: String,
    pub new_id: String,
}

pub fn generate_content_key() -> [u8; CONTENT_KEY_SIZE] {
    let mut key = [0u8; CONTENT_KEY_SIZE];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

pub fn derive_user_id_from_public_key(public_key_bytes: &[u8]) -> String {
    let digest = Sha256::digest(public_key_bytes);
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn generate_keypair() -> StoredKeypair {
    let mut rng = OsRng;
    generate_keypair_with_rng(&mut rng)
}

pub fn generate_keypair_with_rng(rng: &mut (impl RngCore + CryptoRng)) -> StoredKeypair {
    let (private_key, public_key) = X25519HkdfSha256::gen_keypair(rng);
    let public_key_bytes = public_key.to_bytes();
    let private_key_bytes = private_key.to_bytes();
    let id = derive_user_id_from_public_key(&public_key_bytes);

    StoredKeypair {
        id,
        public_key_hex: hex::encode(public_key_bytes),
        private_key_hex: hex::encode(private_key_bytes),
    }
}

pub fn load_keypair(path: &Path) -> Result<StoredKeypair, KeyStoreError> {
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

pub fn store_keypair(path: &Path, keypair: &StoredKeypair) -> Result<(), KeyStoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(keypair)?;
    fs::write(path, json)?;
    Ok(())
}

/// Rotate the keypair stored at the given path.
///
/// Because user IDs are derived from public keys, rotation changes the user ID.
/// Callers should share the new public key and ID with peers to maintain continuity.
pub fn rotate_keypair(path: &Path) -> Result<KeyRotation, KeyStoreError> {
    let existing = load_keypair(path)?;
    let new_keypair = generate_keypair();
    store_keypair(path, &new_keypair)?;
    Ok(KeyRotation {
        previous_id: existing.id,
        new_id: new_keypair.id,
    })
}

pub fn encrypt_payload(
    content_key: &[u8],
    plaintext: &[u8],
    aad: &[u8],
    nonce: Option<&[u8]>,
) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    if content_key.len() != CONTENT_KEY_SIZE {
        return Err(CryptoError::InvalidLength("content key must be 32 bytes"));
    }

    let nonce_bytes = match nonce {
        Some(value) => {
            if value.len() != NONCE_SIZE {
                return Err(CryptoError::InvalidLength("nonce must be 12 bytes"));
            }
            value.to_vec()
        }
        None => {
            let mut generated = [0u8; NONCE_SIZE];
            rand::rngs::OsRng.fill_bytes(&mut generated);
            generated.to_vec()
        }
    };

    let key = Key::from_slice(content_key);
    let aead = ChaCha20Poly1305::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = aead.encrypt(
        nonce,
        chacha20poly1305::aead::Payload {
            msg: plaintext,
            aad,
        },
    )?;
    Ok((nonce_bytes, ciphertext))
}

pub fn decrypt_payload(
    content_key: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if content_key.len() != CONTENT_KEY_SIZE {
        return Err(CryptoError::InvalidLength("content key must be 32 bytes"));
    }
    if nonce.len() != NONCE_SIZE {
        return Err(CryptoError::InvalidLength("nonce must be 12 bytes"));
    }

    let key = Key::from_slice(content_key);
    let aead = ChaCha20Poly1305::new(key);
    let nonce = Nonce::from_slice(nonce);
    let plaintext = aead.decrypt(
        nonce,
        chacha20poly1305::aead::Payload {
            msg: ciphertext,
            aad,
        },
    )?;
    Ok(plaintext)
}

pub fn wrap_content_key(
    recipient_public_key_bytes: &[u8],
    content_key: &[u8],
    info: &[u8],
    sender_seed: Option<[u8; 32]>,
) -> Result<WrappedKey, CryptoError> {
    if content_key.len() != CONTENT_KEY_SIZE {
        return Err(CryptoError::InvalidLength("content key must be 32 bytes"));
    }

    let recipient_public_key =
        <X25519HkdfSha256 as hpke::Kem>::PublicKey::from_bytes(recipient_public_key_bytes)?;

    let (encapped_key, mut sender_ctx) = if let Some(seed) = sender_seed {
        let mut rng = ChaCha20Rng::from_seed(seed);
        hpke::setup_sender::<HpkeChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
            &OpModeS::Base,
            &recipient_public_key,
            info,
            &mut rng,
        )?
    } else {
        let mut rng = rand::rngs::OsRng;
        hpke::setup_sender::<HpkeChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
            &OpModeS::Base,
            &recipient_public_key,
            info,
            &mut rng,
        )?
    };

    let ciphertext = sender_ctx.seal(content_key, info)?;
    Ok(WrappedKey {
        enc: encapped_key.to_bytes().to_vec(),
        ciphertext,
    })
}

pub fn unwrap_content_key(
    recipient_private_key_bytes: &[u8],
    wrapped_key: &WrappedKey,
    info: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let recipient_private_key =
        <X25519HkdfSha256 as hpke::Kem>::PrivateKey::from_bytes(recipient_private_key_bytes)?;
    let encapped_key = <X25519HkdfSha256 as hpke::Kem>::EncappedKey::from_bytes(&wrapped_key.enc)?;

    let mut receiver_ctx = hpke::setup_receiver::<
        HpkeChaCha20Poly1305,
        HkdfSha256,
        X25519HkdfSha256,
    >(&OpModeR::Base, &recipient_private_key, &encapped_key, info)?;

    let plaintext = receiver_ctx.open(&wrapped_key.ciphertext, info)?;
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_content_key() {
        let key = generate_content_key();
        assert_eq!(key.len(), CONTENT_KEY_SIZE);
    }

    #[test]
    fn encrypts_and_decrypts_payload_with_fixture() {
        let content_key =
            hex::decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .unwrap();
        let nonce = hex::decode("000102030405060708090a0b").unwrap();
        let plaintext = b"payload:hello world";
        let aad = b"context";

        let (nonce_out, ciphertext) =
            encrypt_payload(&content_key, plaintext, aad, Some(&nonce)).unwrap();

        assert_eq!(nonce_out, nonce);
        assert_eq!(
            hex::encode(&ciphertext),
            "f99a716c4676c17adfe6539ff73d790cbb1cd6a2b6081da95ff91c59a1083320ae00d5"
        );

        let decrypted = decrypt_payload(&content_key, &nonce_out, &ciphertext, aad).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wraps_and_unwraps_content_key_with_fixture() {
        let mut recipient_rng = ChaCha20Rng::from_seed([7u8; 32]);
        let (recipient_private_key, recipient_public_key) =
            X25519HkdfSha256::gen_keypair(&mut recipient_rng);

        let content_key =
            hex::decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .unwrap();

        let info = b"tenet-hpke";
        let sender_seed = [9u8; 32];
        let wrapped = wrap_content_key(
            &recipient_public_key.to_bytes(),
            &content_key,
            info,
            Some(sender_seed),
        )
        .unwrap();

        assert_eq!(
            hex::encode(&wrapped.enc),
            "0e2e6b71a023c1d67c9df32f0459ff6e93d1805d20951728d3e5cd540eac945a"
        );
        assert_eq!(
            hex::encode(&wrapped.ciphertext),
            "e7c9e1e1948c5dd8c110e313a16afd1b5e2e9a53d99aecd6cad17e48bac00ca34eeee3a38557ec53763c500179030753"
        );

        let unwrapped =
            unwrap_content_key(&recipient_private_key.to_bytes(), &wrapped, info).unwrap();

        assert_eq!(unwrapped, content_key);
    }

    #[test]
    fn derives_stable_user_id_from_public_key() {
        let key_bytes = [1u8, 2, 3];
        let id = derive_user_id_from_public_key(&key_bytes);
        assert_eq!(id, "A5BYxvLAy0ksUzsKTRTvd8wPeKvMztUofYShogEc-4E");
        assert_eq!(id, derive_user_id_from_public_key(&key_bytes));
    }

    #[test]
    fn stores_and_loads_keypairs_round_trip() {
        let keypair = generate_keypair();
        let temp_dir =
            std::env::temp_dir().join(format!("tenet-keypair-test-{}", rand::random::<u64>()));
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("identity.json");

        store_keypair(&path, &keypair).unwrap();
        let loaded = load_keypair(&path).unwrap();

        assert_eq!(loaded, keypair);
    }
}
