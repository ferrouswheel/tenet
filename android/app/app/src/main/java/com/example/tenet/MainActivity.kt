package com.example.tenet

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.navigation.compose.rememberNavController
import com.example.tenet.theme.TenetTheme
import com.example.tenet.ui.TenetNavHost
import dagger.hilt.android.AndroidEntryPoint

/**
 * Single-activity entry point.  Navigation is handled entirely by [TenetNavHost]
 * via Navigation Compose.
 *
 * ### Deep-link handling
 * The activity responds to `tenet://` URIs delivered via [Intent.ACTION_VIEW] (from
 * system notification taps).  The URI host and path are mapped to internal navigation
 * routes and passed to [TenetNavHost] as [initialRoute]:
 *
 * | Deep-link URI                    | Route            |
 * |----------------------------------|-----------------|
 * | `tenet://conversations`          | conversations   |
 * | `tenet://conversation/<peerId>`  | conversation/…  |
 * | `tenet://friends`                | friends         |
 * | `tenet://groups`                 | groups          |
 * | `tenet://post/<messageId>`       | post/…          |
 *
 * ### Share-to intent handling
 * When another app shares `text/plain` content to Tenet via [Intent.ACTION_SEND],
 * the text is captured in [sharedText] and forwarded to [TenetNavHost], which
 * navigates to the Compose screen with the text pre-populated.
 *
 * [android:launchMode] is `singleTop` so that a notification tap while the app is
 * already in the foreground calls [onNewIntent] rather than creating a new instance.
 */
@AndroidEntryPoint
class MainActivity : ComponentActivity() {

    /** Route to navigate to on startup (from a deep-link intent), or null. */
    private var initialRoute by mutableStateOf<String?>(null)

    /** Text pre-seeded in the Compose screen (from a share intent), or null. */
    private var sharedText by mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        parseIntent(intent)
        setContent {
            TenetTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    val navController = rememberNavController()
                    TenetNavHost(
                        navController = navController,
                        initialRoute = initialRoute,
                        sharedText = sharedText,
                        onSharedTextConsumed = { sharedText = null },
                    )
                }
            }
        }
    }

    /**
     * Called when the activity is already running (singleTop) and a new intent
     * arrives — e.g. the user taps a second notification while the app is open.
     */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        parseIntent(intent)
    }

    // -----------------------------------------------------------------------
    // Intent parsing
    // -----------------------------------------------------------------------

    private fun parseIntent(intent: Intent?) {
        when (intent?.action) {
            Intent.ACTION_VIEW -> {
                intent.data?.let { uri ->
                    initialRoute = deepLinkUriToRoute(uri)
                }
            }
            Intent.ACTION_SEND -> {
                if (intent.type == "text/plain") {
                    sharedText = intent.getStringExtra(Intent.EXTRA_TEXT)
                }
                // image/* shares: content URI available via EXTRA_STREAM.
                // Full image-share support is deferred (requires reading bytes
                // and calling uploadAttachment, then passing the hash to Compose).
            }
        }
    }

    companion object {
        /**
         * Converts a `tenet://` deep-link [Uri] to the matching Compose navigation
         * route string.  Returns null if the URI is unrecognised.
         *
         * Exposed as an internal function so it can be tested without an Activity.
         */
        internal fun deepLinkUriToRoute(uri: Uri): String? {
            if (uri.scheme != "tenet") return null
            val host = uri.host ?: return null
            val segments = uri.pathSegments     // empty list for authority-only URIs

            return when {
                // tenet://conversations
                host == "conversations" && segments.isEmpty() -> "conversations"

                // tenet://conversation/<peerId>
                host == "conversation" && segments.size == 1 -> "conversation/${segments[0]}"

                // tenet://friends
                host == "friends" && segments.isEmpty() -> "friends"

                // tenet://groups
                host == "groups" && segments.isEmpty() -> "groups"

                // tenet://post/<messageId>
                host == "post" && segments.size == 1 -> "post/${segments[0]}"

                // tenet://timeline
                host == "timeline" && segments.isEmpty() -> "timeline"

                else -> null
            }
        }
    }
}
