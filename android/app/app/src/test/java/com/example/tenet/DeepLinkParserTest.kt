package com.example.tenet

import android.net.Uri
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/**
 * Unit tests for [MainActivity.deepLinkUriToRoute].
 *
 * [Uri] is an Android class; we use Robolectric so these run on the JVM
 * without a device.  (Add `testImplementation "org.robolectric:robolectric:4.12"`
 * to build.gradle.kts if Robolectric is not already on the test classpath.)
 *
 * These tests validate the deep-link → route mapping used by [MainActivity] to
 * navigate to the correct screen when a system notification is tapped.
 */
@RunWith(RobolectricTestRunner::class)
class DeepLinkParserTest {

    private fun parse(uri: String): String? =
        MainActivity.deepLinkUriToRoute(Uri.parse(uri))

    @Test
    fun `tenet conversations maps to conversations route`() {
        assertEquals("conversations", parse("tenet://conversations"))
    }

    @Test
    fun `tenet conversation with peerId maps to conversation detail`() {
        assertEquals("conversation/peer123", parse("tenet://conversation/peer123"))
    }

    @Test
    fun `tenet friends maps to friends route`() {
        assertEquals("friends", parse("tenet://friends"))
    }

    @Test
    fun `tenet groups maps to groups route`() {
        assertEquals("groups", parse("tenet://groups"))
    }

    @Test
    fun `tenet post with messageId maps to post detail`() {
        assertEquals("post/abc-def", parse("tenet://post/abc-def"))
    }

    @Test
    fun `tenet timeline maps to timeline route`() {
        assertEquals("timeline", parse("tenet://timeline"))
    }

    @Test
    fun `unknown scheme returns null`() {
        assertNull(parse("https://example.com/conversations"))
    }

    @Test
    fun `unknown host returns null`() {
        assertNull(parse("tenet://unknown-screen"))
    }

    @Test
    fun `conversation with no segments returns null`() {
        // tenet://conversation (no peerId) is not a valid route
        assertNull(parse("tenet://conversation"))
    }

    @Test
    fun `post with no segments returns null`() {
        assertNull(parse("tenet://post"))
    }
}
