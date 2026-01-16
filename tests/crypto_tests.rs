use hpke::kem::X25519HkdfSha256;
use hpke::Kem as _;
use hpke::Serializable;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

use tenet::crypto::{decrypt_payload, encrypt_payload, unwrap_content_key, wrap_content_key};

#[test]
fn aead_encrypts_and_decrypts_payload() {
    let content_key = [7u8; 32];
    let plaintext = b"tenet test payload";
    let aad = b"tenet-header";

    let (nonce, ciphertext) =
        encrypt_payload(&content_key, plaintext, aad, None).expect("encrypt payload");
    let decrypted =
        decrypt_payload(&content_key, &nonce, &ciphertext, aad).expect("decrypt payload");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn hpke_wraps_and_unwraps_content_key() {
    let mut rng = ChaCha20Rng::from_seed([42u8; 32]);
    let (recipient_private, recipient_public) = X25519HkdfSha256::gen_keypair(&mut rng);
    let content_key = [3u8; 32];

    let wrapped = wrap_content_key(
        &recipient_public.to_bytes(),
        &content_key,
        b"tenet-hpke-test",
        Some([1u8; 32]),
    )
    .expect("wrap content key");

    let unwrapped = unwrap_content_key(&recipient_private.to_bytes(), &wrapped, b"tenet-hpke-test")
        .expect("unwrap content key");

    assert_eq!(unwrapped, content_key);
}
