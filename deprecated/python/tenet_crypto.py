import os
from dataclasses import dataclass

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import x25519
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

CONTENT_KEY_SIZE = 32
NONCE_SIZE = 12
ENCAPSULATED_KEY_SIZE = 32
WRAP_KEY_SIZE = 32
WRAP_NONCE_SIZE = 12
DEFAULT_INFO = b"tenet-hpke"


@dataclass(frozen=True)
class WrappedKey:
    enc: bytes
    ciphertext: bytes


def generate_content_key():
    return os.urandom(CONTENT_KEY_SIZE)


def encrypt_payload(content_key, plaintext, aad=b"", nonce=None):
    if len(content_key) != CONTENT_KEY_SIZE:
        raise ValueError("content key must be 32 bytes")
    if nonce is None:
        nonce = os.urandom(NONCE_SIZE)
    if len(nonce) != NONCE_SIZE:
        raise ValueError("nonce must be 12 bytes")
    aead = ChaCha20Poly1305(content_key)
    ciphertext = aead.encrypt(nonce, plaintext, aad)
    return nonce, ciphertext


def decrypt_payload(content_key, nonce, ciphertext, aad=b""):
    if len(content_key) != CONTENT_KEY_SIZE:
        raise ValueError("content key must be 32 bytes")
    if len(nonce) != NONCE_SIZE:
        raise ValueError("nonce must be 12 bytes")
    aead = ChaCha20Poly1305(content_key)
    return aead.decrypt(nonce, ciphertext, aad)


def wrap_content_key(recipient_public_key_bytes, content_key, info=DEFAULT_INFO,
                     ephemeral_private_key_bytes=None):
    if len(content_key) != CONTENT_KEY_SIZE:
        raise ValueError("content key must be 32 bytes")
    recipient_public_key = x25519.X25519PublicKey.from_public_bytes(
        recipient_public_key_bytes)
    if ephemeral_private_key_bytes is None:
        ephemeral_private_key = x25519.X25519PrivateKey.generate()
    else:
        ephemeral_private_key = x25519.X25519PrivateKey.from_private_bytes(
            ephemeral_private_key_bytes)
    encapsulated_key = ephemeral_private_key.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    shared_secret = ephemeral_private_key.exchange(recipient_public_key)
    wrap_key, wrap_nonce = _derive_wrap_key(shared_secret, info)
    aead = ChaCha20Poly1305(wrap_key)
    ciphertext = aead.encrypt(wrap_nonce, content_key, info)
    return WrappedKey(enc=encapsulated_key, ciphertext=ciphertext)


def unwrap_content_key(recipient_private_key_bytes, wrapped_key, info=DEFAULT_INFO):
    recipient_private_key = x25519.X25519PrivateKey.from_private_bytes(
        recipient_private_key_bytes)
    encapsulated_key = x25519.X25519PublicKey.from_public_bytes(wrapped_key.enc)
    shared_secret = recipient_private_key.exchange(encapsulated_key)
    wrap_key, wrap_nonce = _derive_wrap_key(shared_secret, info)
    aead = ChaCha20Poly1305(wrap_key)
    return aead.decrypt(wrap_nonce, wrapped_key.ciphertext, info)


def _derive_wrap_key(shared_secret, info):
    hkdf = HKDF(
        algorithm=hashes.SHA256(),
        length=WRAP_KEY_SIZE + WRAP_NONCE_SIZE,
        salt=None,
        info=info,
    )
    okm = hkdf.derive(shared_secret)
    return okm[:WRAP_KEY_SIZE], okm[WRAP_KEY_SIZE:]
