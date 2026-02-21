package com.example.tenet.data

import android.content.Context
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

    companion object {
        private const val KEY_RELAY_URL = "relay_url"
    }
}
