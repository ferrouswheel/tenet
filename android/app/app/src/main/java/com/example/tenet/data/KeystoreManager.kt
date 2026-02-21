package com.example.tenet.data

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import dagger.hilt.android.qualifiers.ApplicationContext
import android.content.Context
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import javax.inject.Inject
import javax.inject.Singleton

/**
 * Manages a 256-bit data-encryption key (DEK) protected by the Android Keystore.
 *
 * **First launch**: a random 256-bit DEK is generated, wrapped (AES-256-GCM) with a
 * [SecretKey] stored in the Android Keystore, and the wrapped bytes are persisted in
 * [TenetPreferences].
 *
 * **Subsequent launches**: the Keystore key is retrieved and used to unwrap the DEK
 * from [TenetPreferences].
 *
 * The Keystore key never leaves the secure element on supported devices. In a
 * production build, add `.setUserAuthenticationRequired(true)` (plus a validity
 * duration) to the [KeyGenParameterSpec] to enforce biometric or screen-lock
 * authentication before the key can be used.
 *
 * The Rust [com.example.tenet.uniffi.TenetClient] is unmodified; this class provides
 * the Android-layer key-protection mechanism described in the architecture document.
 * The DEK is available for future integration where the Rust layer accepts externally
 * provided key material (e.g. a pre-decrypted identity path).
 *
 * Thread-safety: all operations acquire a lock on [this]; callers should dispatch to
 * a background thread (e.g. [kotlinx.coroutines.Dispatchers.IO]) to avoid blocking
 * the main thread.
 */
@Singleton
class KeystoreManager @Inject constructor(
    @ApplicationContext private val context: Context,
    private val prefs: TenetPreferences,
) {

    /**
     * Returns the plaintext 256-bit DEK, creating and persisting it on first call.
     *
     * The returned [ByteArray] is the raw DEK — callers should zero it after use
     * if it is written to any mutable buffer.
     *
     * @throws java.security.GeneralSecurityException on Keystore failures.
     */
    @Synchronized
    fun getOrCreateDek(): ByteArray {
        val wrapped = prefs.wrappedDek
        return if (wrapped != null) {
            unwrapDek(wrapped)
        } else {
            val dek = generateDek()
            prefs.wrappedDek = wrapDek(dek)
            dek
        }
    }

    /** True if a DEK has already been created and stored. */
    fun hasDek(): Boolean = prefs.wrappedDek != null

    /**
     * Discards the stored DEK and Keystore key.  After calling this, [getOrCreateDek]
     * will generate a new DEK — any data previously encrypted with the old DEK will
     * become permanently inaccessible.  Use only for identity reset / wipe scenarios.
     */
    @Synchronized
    fun clearDek() {
        prefs.wrappedDek = null
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        if (keyStore.containsAlias(KEY_ALIAS)) {
            keyStore.deleteEntry(KEY_ALIAS)
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    private fun getOrCreateKeystoreKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }

        val keyGenerator = KeyGenerator.getInstance(
            KeyProperties.KEY_ALGORITHM_AES,
            ANDROID_KEYSTORE,
        )
        keyGenerator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(KEY_SIZE_BITS)
                // Production: uncomment to require biometric / screen-lock auth.
                // .setUserAuthenticationRequired(true)
                // .setUserAuthenticationValidityDurationSeconds(30)
                .build(),
        )
        return keyGenerator.generateKey()
    }

    private fun generateDek(): ByteArray =
        ByteArray(DEK_SIZE_BYTES).also { SecureRandom().nextBytes(it) }

    /**
     * Encrypts [dek] with the Keystore key.
     * Layout of the returned bytes: `IV (12 bytes) || GCM ciphertext+tag`.
     */
    internal fun wrapDek(dek: ByteArray): ByteArray {
        val key = getOrCreateKeystoreKey()
        val cipher = Cipher.getInstance(AES_GCM_NO_PADDING)
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val iv = cipher.iv          // GCM standard: 12 bytes
        val ciphertext = cipher.doFinal(dek)
        return iv + ciphertext
    }

    /**
     * Decrypts [wrapped] (produced by [wrapDek]) with the Keystore key.
     * Expects layout: `IV (12 bytes) || GCM ciphertext+tag`.
     */
    internal fun unwrapDek(wrapped: ByteArray): ByteArray {
        require(wrapped.size > IV_SIZE_BYTES) {
            "Wrapped DEK is too short (${wrapped.size} bytes); expected > $IV_SIZE_BYTES"
        }
        val key = getOrCreateKeystoreKey()
        val iv = wrapped.copyOfRange(0, IV_SIZE_BYTES)
        val ciphertext = wrapped.copyOfRange(IV_SIZE_BYTES, wrapped.size)
        val cipher = Cipher.getInstance(AES_GCM_NO_PADDING)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, iv))
        return cipher.doFinal(ciphertext)
    }

    companion object {
        private const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val KEY_ALIAS = "tenet_master_key"
        private const val KEY_SIZE_BITS = 256
        const val DEK_SIZE_BYTES = 32        // 256-bit DEK
        const val IV_SIZE_BYTES = 12         // GCM standard IV length
        private const val GCM_TAG_BITS = 128
        private const val AES_GCM_NO_PADDING = "AES/GCM/NoPadding"
    }
}
