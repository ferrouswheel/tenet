use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hpke::aead::ChaCha20Poly1305 as HpkeChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, OpModeR, OpModeS, Serializable};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

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

pub fn generate_content_key() -> [u8; CONTENT_KEY_SIZE] {
    let mut key = [0u8; CONTENT_KEY_SIZE];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
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
    let ciphertext = aead.encrypt(nonce, chacha20poly1305::aead::Payload { msg: plaintext, aad })?;
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
    let plaintext = aead.decrypt(nonce, chacha20poly1305::aead::Payload { msg: ciphertext, aad })?;
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
    let encapped_key =
        <X25519HkdfSha256 as hpke::Kem>::EncappedKey::from_bytes(&wrapped_key.enc)?;

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
    use hpke::Kem as _;

    #[test]
    fn generates_content_key() {
        let key = generate_content_key();
        assert_eq!(key.len(), CONTENT_KEY_SIZE);
    }

    #[test]
    fn encrypts_and_decrypts_payload_with_fixture() {
        let content_key = hex::decode(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
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

        let content_key = hex::decode(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
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

        let unwrapped = unwrap_content_key(
            &recipient_private_key.to_bytes(),
            &wrapped,
            info,
        )
        .unwrap();

        assert_eq!(unwrapped, content_key);
    }
}
