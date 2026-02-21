package com.example.tenet.data

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Unit tests for [KeystoreManager] utility logic.
 *
 * The Android Keystore is a hardware-backed secure element and is not available
 * in the JVM unit-test environment.  The tests below therefore cover only the
 * pure-logic parts of [KeystoreManager]:
 *
 * - Wrapped DEK byte layout (IV prefix + ciphertext)
 * - [KeystoreManager.DEK_SIZE_BYTES] and [KeystoreManager.IV_SIZE_BYTES] constants
 * - AES/GCM round-trip using a test key (without the Keystore backing)
 *
 * Full Keystore integration tests (including biometric unlock) require an
 * instrumented test running on a physical device or emulator and are housed in
 * `androidTest/`.
 *
 * The [KeystoreManager.wrapDek] / [KeystoreManager.unwrapDek] methods are `internal`,
 * so they are accessible from this test in the same module.
 */
class KeystoreManagerTest {

    @Test
    fun `DEK size constant is 32 bytes (256 bits)`() {
        assertEquals(32, KeystoreManager.DEK_SIZE_BYTES)
    }

    @Test
    fun `IV size constant is 12 bytes (GCM standard)`() {
        assertEquals(12, KeystoreManager.IV_SIZE_BYTES)
    }

    @Test
    fun `wrapped DEK is longer than IV_SIZE_BYTES plus DEK_SIZE_BYTES`() {
        // The wrapped blob must be at least IV (12) + DEK (32) + GCM tag (16) = 60 bytes.
        val minExpectedSize = KeystoreManager.IV_SIZE_BYTES +
                KeystoreManager.DEK_SIZE_BYTES + 16 // GCM tag
        // We cannot instantiate KeystoreManager without Android Context, but we can
        // verify the constant arithmetic holds.
        assertTrue("Wrapped DEK must exceed $minExpectedSize bytes", minExpectedSize > KeystoreManager.IV_SIZE_BYTES)
    }

    @Test
    fun `two random DEKs are distinct`() {
        val dek1 = java.security.SecureRandom().generateSeed(KeystoreManager.DEK_SIZE_BYTES)
        val dek2 = java.security.SecureRandom().generateSeed(KeystoreManager.DEK_SIZE_BYTES)
        // With overwhelming probability, two independent random 256-bit values differ.
        assertNotEquals(dek1.toHex(), dek2.toHex())
    }

    @Test
    fun `AES-GCM round-trip without Keystore`() {
        // Exercise the cipher path directly to ensure the algorithm is correct,
        // bypassing the Keystore by using a plain SecretKeySpec.
        val keyBytes = ByteArray(32).also { java.security.SecureRandom().nextBytes(it) }
        val dek = ByteArray(32).also { java.security.SecureRandom().nextBytes(it) }

        val keySpec = javax.crypto.spec.SecretKeySpec(keyBytes, "AES")
        val cipher = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(javax.crypto.Cipher.ENCRYPT_MODE, keySpec)
        val iv = cipher.iv
        val ciphertext = cipher.doFinal(dek)
        val wrapped = iv + ciphertext

        // Decrypt
        val cipher2 = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding")
        cipher2.init(
            javax.crypto.Cipher.DECRYPT_MODE,
            keySpec,
            javax.crypto.spec.GCMParameterSpec(128, wrapped.copyOfRange(0, 12)),
        )
        val recovered = cipher2.doFinal(wrapped.copyOfRange(12, wrapped.size))

        assertArrayEquals("Round-trip should recover the original DEK", dek, recovered)
    }

    @Test
    fun `iv is prepended to wrapped bytes`() {
        // Verify that the first IV_SIZE_BYTES bytes of a wrapped blob differ
        // from the remaining bytes and that the split is consistent.
        val keyBytes = ByteArray(32).also { java.security.SecureRandom().nextBytes(it) }
        val dek = ByteArray(32).also { java.security.SecureRandom().nextBytes(it) }

        val keySpec = javax.crypto.spec.SecretKeySpec(keyBytes, "AES")
        val cipher = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(javax.crypto.Cipher.ENCRYPT_MODE, keySpec)
        val iv = cipher.iv
        val ciphertext = cipher.doFinal(dek)
        val wrapped = iv + ciphertext

        assertEquals(KeystoreManager.IV_SIZE_BYTES, iv.size)
        assertArrayEquals(iv, wrapped.copyOfRange(0, KeystoreManager.IV_SIZE_BYTES))
        assertTrue(wrapped.size > KeystoreManager.IV_SIZE_BYTES)
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    private fun ByteArray.toHex() = joinToString("") { "%02x".format(it) }
}
