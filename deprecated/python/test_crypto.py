from unittest import TestCase

from tenet.crypto import (
    decrypt_payload,
    encrypt_payload,
    generate_content_key,
    unwrap_content_key,
    wrap_content_key,
)


class TestCrypto(TestCase):

    def test_generate_content_key(self):
        content_key = generate_content_key()
        self.assertEqual(len(content_key), 32)

    def test_encrypt_decrypt_roundtrip_deterministic(self):
        content_key = bytes.fromhex(
            "000102030405060708090a0b0c0d0e0f"
            "101112131415161718191a1b1c1d1e1f"
        )
        nonce = bytes.fromhex("000102030405060708090a0b")
        plaintext = b"payload:hello world"
        aad = b"context"

        nonce_out, ciphertext = encrypt_payload(
            content_key, plaintext, aad=aad, nonce=nonce)

        self.assertEqual(nonce_out, nonce)
        self.assertEqual(
            ciphertext.hex(),
            "f99a716c4676c17adfe6539ff73d790cbb1cd6a2b6081da95ff91c59a1083320"
            "ae00d5",
        )

        decrypted = decrypt_payload(content_key, nonce_out, ciphertext, aad=aad)
        self.assertEqual(decrypted, plaintext)

    def test_wrap_unwrap_roundtrip_deterministic(self):
        content_key = bytes.fromhex(
            "000102030405060708090a0b0c0d0e0f"
            "101112131415161718191a1b1c1d1e1f"
        )
        recipient_private = bytes.fromhex(
            "1f1e1d1c1b1a19181716151413121110"
            "0f0e0d0c0b0a09080706050403020100"
        )
        recipient_public = bytes.fromhex(
            "87968c1c1642bd0600f6ad869b88f92c"
            "9623d0dfc44f01deffe21c9add3dca5f"
        )
        ephemeral_private = bytes.fromhex(
            "09080706050403020100010203040506"
            "0708090a0b0c0d0e0f10111213141516"
        )

        wrapped = wrap_content_key(
            recipient_public,
            content_key,
            info=b"tenet-hpke",
            ephemeral_private_key_bytes=ephemeral_private,
        )

        self.assertEqual(
            wrapped.enc.hex(),
            "0cef1af710ae1ba914909f68a99cd7db"
            "baa9f2234e245d0964d9d1bfde59a771",
        )
        self.assertEqual(
            wrapped.ciphertext.hex(),
            "6a6e97b5c466df7fc45980cb852013ba"
            "b4a7ed8a0df7d4a08e2b6def285fa7be"
            "47b62c013510f666386db181cde3c0d7",
        )

        unwrapped = unwrap_content_key(
            recipient_private,
            wrapped,
            info=b"tenet-hpke",
        )

        self.assertEqual(unwrapped, content_key)
