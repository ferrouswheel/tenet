package com.example.tenet.data

import android.content.Context
import android.util.Base64
import dagger.hilt.android.qualifiers.ApplicationContext
import javax.inject.Inject
import javax.inject.Singleton

/**
 * Thin wrapper around SharedPreferences for persisting app-level settings.
 */
@Singleton
class TenetPreferences @Inject constructor(
    @ApplicationContext context: Context,
) {
    private val prefs = context.getSharedPreferences("tenet_prefs", Context.MODE_PRIVATE)

    var relayUrl: String?
        get() = prefs.getString(KEY_RELAY_URL, null)
        set(value) = prefs.edit().putString(KEY_RELAY_URL, value).apply()

    /**
     * The Keystore-wrapped DEK produced by [KeystoreManager].
     * Stored as a Base64 string; null if the DEK has not been created yet.
     */
    var wrappedDek: ByteArray?
        get() = prefs.getString(KEY_WRAPPED_DEK, null)
            ?.let { Base64.decode(it, Base64.NO_WRAP) }
        set(value) {
            val encoded = value?.let { Base64.encodeToString(it, Base64.NO_WRAP) }
            prefs.edit().putString(KEY_WRAPPED_DEK, encoded).apply()
        }

    companion object {
        private const val KEY_RELAY_URL = "relay_url"
        private const val KEY_WRAPPED_DEK = "wrapped_dek"
    }
}
