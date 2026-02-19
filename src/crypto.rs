use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
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
use std::time::{SystemTime, UNIX_EPOCH};

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
    Signature(ed25519_dalek::SignatureError),
    InvalidSignature,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::InvalidLength(message) => write!(f, "invalid length: {message}"),
            CryptoError::Hpke(error) => write!(f, "hpke error: {error}"),
            CryptoError::Aead(error) => write!(f, "aead error: {error}"),
            CryptoError::Signature(error) => write!(f, "signature error: {error}"),
            CryptoError::InvalidSignature => write!(f, "invalid signature"),
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

impl From<ed25519_dalek::SignatureError> for CryptoError {
    fn from(error: ed25519_dalek::SignatureError) -> Self {
        CryptoError::Signature(error)
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
    pub signing_public_key_hex: String,
    pub signing_private_key_hex: String,
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

    // Generate Ed25519 signing key; peer ID is derived from the signing key
    let signing_key = SigningKey::generate(rng);
    let verifying_key = signing_key.verifying_key();
    let id = derive_user_id_from_public_key(&verifying_key.to_bytes());

    StoredKeypair {
        id,
        public_key_hex: hex::encode(public_key_bytes),
        private_key_hex: hex::encode(private_key_bytes),
        signing_public_key_hex: hex::encode(verifying_key.to_bytes()),
        signing_private_key_hex: hex::encode(signing_key.to_bytes()),
    }
}

/// Build a signed auth token for relay inbox/WebSocket access.
///
/// Token format: `{ed25519_pubkey_hex}.{unix_timestamp_secs}.{signature_base64url}`
/// Signature covers: `b"tenet-relay-auth\n{peer_id}\n{timestamp_secs}"`
pub fn make_relay_auth_token(
    signing_private_key_hex: &str,
    peer_id: &str,
) -> Result<String, CryptoError> {
    let signing_key_bytes = hex::decode(signing_private_key_hex)
        .map_err(|_| CryptoError::InvalidLength("invalid hex"))?;
    if signing_key_bytes.len() != 32 {
        return Err(CryptoError::InvalidLength("signing key must be 32 bytes"));
    }
    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&signing_key_bytes);
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let verifying_key = signing_key.verifying_key();
    let pubkey_hex = hex::encode(verifying_key.to_bytes());

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CryptoError::InvalidLength("clock error"))?
        .as_secs();

    let message = format!("tenet-relay-auth\n{peer_id}\n{timestamp}");
    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!("{pubkey_hex}.{timestamp}.{sig_b64}"))
}

/// Verify a relay auth token for a given peer_id.
///
/// Returns `Err` if the signature is invalid, the pubkey doesn't match the
/// peer_id, or the timestamp is outside ±60 seconds.
pub fn verify_relay_auth_token(token: &str, peer_id: &str) -> Result<(), CryptoError> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() != 3 {
        return Err(CryptoError::InvalidSignature);
    }
    let pubkey_hex = parts[0];
    let timestamp_str = parts[1];
    let sig_b64 = parts[2];

    let timestamp: u64 = timestamp_str
        .parse()
        .map_err(|_| CryptoError::InvalidSignature)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CryptoError::InvalidLength("clock error"))?
        .as_secs();
    let diff = if now >= timestamp {
        now - timestamp
    } else {
        timestamp - now
    };
    if diff > 60 {
        return Err(CryptoError::InvalidSignature);
    }

    let pubkey_bytes = hex::decode(pubkey_hex).map_err(|_| CryptoError::InvalidSignature)?;
    if pubkey_bytes.len() != 32 {
        return Err(CryptoError::InvalidLength("pubkey must be 32 bytes"));
    }
    let expected_peer_id = derive_user_id_from_public_key(&pubkey_bytes);
    if expected_peer_id != peer_id {
        return Err(CryptoError::InvalidSignature);
    }

    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&pubkey_bytes);
    let verifying_key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| CryptoError::InvalidSignature)?;

    let message = format!("tenet-relay-auth\n{peer_id}\n{timestamp}");

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| CryptoError::InvalidSignature)?;
    if sig_bytes.len() != 64 {
        return Err(CryptoError::InvalidLength("signature must be 64 bytes"));
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_arr);

    verifying_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| CryptoError::InvalidSignature)
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

/// Sign a message with an Ed25519 private key.
pub fn sign_message(message: &[u8], signing_private_key_hex: &str) -> Result<String, CryptoError> {
    let signing_key_bytes = hex::decode(signing_private_key_hex)
        .map_err(|_| CryptoError::InvalidLength("invalid hex"))?;
    if signing_key_bytes.len() != 32 {
        return Err(CryptoError::InvalidLength("signing key must be 32 bytes"));
    }
    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&signing_key_bytes);
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let signature = signing_key.sign(message);
    Ok(URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

/// Verify a message signature with an Ed25519 public key.
pub fn verify_signature(
    message: &[u8],
    signature_base64: &str,
    signing_public_key_hex: &str,
) -> Result<(), CryptoError> {
    let verifying_key_bytes = hex::decode(signing_public_key_hex)
        .map_err(|_| CryptoError::InvalidLength("invalid hex"))?;
    if verifying_key_bytes.len() != 32 {
        return Err(CryptoError::InvalidLength("verifying key must be 32 bytes"));
    }
    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&verifying_key_bytes);
    let verifying_key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| CryptoError::InvalidSignature)?;

    let signature_bytes = URL_SAFE_NO_PAD
        .decode(signature_base64)
        .map_err(|_| CryptoError::InvalidSignature)?;
    if signature_bytes.len() != 64 {
        return Err(CryptoError::InvalidLength("signature must be 64 bytes"));
    }
    let mut sig_bytes = [0u8; 64];
    sig_bytes.copy_from_slice(&signature_bytes);
    let signature = Signature::from_bytes(&sig_bytes);

    verifying_key
        .verify(message, &signature)
        .map_err(|_| CryptoError::InvalidSignature)
}

/// Encrypt a payload for a group using a symmetric group key
///
/// Returns (ciphertext, nonce) tuple
pub fn encrypt_group_payload(
    plaintext: &[u8],
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        ChaCha20Poly1305,
    };

    let cipher = ChaCha20Poly1305::new(group_key.into());

    // Generate a random nonce
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let payload = Payload {
        msg: plaintext,
        aad,
    };

    let ciphertext = cipher
        .encrypt(&nonce_bytes.into(), payload)
        .map_err(CryptoError::Aead)?;

    Ok((ciphertext, nonce_bytes.to_vec()))
}

/// Decrypt a group payload using a symmetric group key
pub fn decrypt_group_payload(
    ciphertext: &[u8],
    nonce: &[u8],
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        ChaCha20Poly1305,
    };

    if nonce.len() != 12 {
        return Err(CryptoError::InvalidLength("nonce must be 12 bytes"));
    }

    let cipher = ChaCha20Poly1305::new(group_key.into());

    let mut nonce_bytes = [0u8; 12];
    nonce_bytes.copy_from_slice(nonce);

    let payload = Payload {
        msg: ciphertext,
        aad,
    };

    cipher
        .decrypt(&nonce_bytes.into(), payload)
        .map_err(CryptoError::Aead)
}

/// Validate a hex-encoded key has the expected byte length.
pub fn validate_hex_key(
    hex_str: &str,
    expected_len: usize,
    field_name: &str,
) -> Result<(), String> {
    let bytes = hex::decode(hex_str).map_err(|_| format!("{field_name} is not valid hex"))?;
    if bytes.len() != expected_len {
        return Err(format!(
            "{field_name} must be {expected_len} bytes, got {}",
            bytes.len()
        ));
    }
    Ok(())
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

    #[test]
    fn signs_and_verifies_messages() {
        let keypair = generate_keypair();
        let message = b"hello, world!";

        let signature = sign_message(message, &keypair.signing_private_key_hex).unwrap();
        verify_signature(message, &signature, &keypair.signing_public_key_hex).unwrap();
    }

    #[test]
    fn rejects_invalid_signature() {
        let keypair = generate_keypair();
        let message = b"hello, world!";
        let tampered_message = b"hello, world?";

        let signature = sign_message(message, &keypair.signing_private_key_hex).unwrap();
        let result = verify_signature(
            tampered_message,
            &signature,
            &keypair.signing_public_key_hex,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_wrong_public_key() {
        let keypair1 = generate_keypair();
        let keypair2 = generate_keypair();
        let message = b"hello, world!";

        let signature = sign_message(message, &keypair1.signing_private_key_hex).unwrap();
        let result = verify_signature(message, &signature, &keypair2.signing_public_key_hex);
        assert!(result.is_err());
    }
}
