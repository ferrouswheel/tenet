package com.example.tenet.ui.qr

import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Unit tests for [generateQrBitmap].
 *
 * These tests run on the JVM.  [android.graphics.Bitmap] is available on the
 * JVM when Robolectric is on the classpath, but [generateQrBitmap] itself only
 * calls [android.graphics.Bitmap.createBitmap] and [Bitmap.setPixel] — both of
 * which Robolectric stubs correctly.
 *
 * Note: full rendering tests (QrCodeScreen composable) require an instrumented
 * test environment and are omitted here.
 */
class QrCodeTest {

    @Test
    fun `non-blank content produces a non-null bitmap`() {
        val bitmap = generateQrBitmap("hello-world", size = 128)
        assertNotNull("Expected a bitmap for valid content", bitmap)
    }

    @Test
    fun `blank content returns null`() {
        assertNull(generateQrBitmap("", size = 256))
        assertNull(generateQrBitmap("   ", size = 256))
    }

    @Test
    fun `peer id string produces correct bitmap dimensions`() {
        val size = 256
        val peerId = "a".repeat(64) // typical hex peer ID length
        val bitmap = generateQrBitmap(peerId, size = size)
        assertNotNull(bitmap)
        assertEquals(size, bitmap!!.width)
        assertEquals(size, bitmap.height)
    }

    // JUnit 4 convenience alias so we can use assertEquals without a static import clash
    private fun assertEquals(expected: Int, actual: Int) {
        org.junit.Assert.assertEquals(expected.toLong(), actual.toLong())
    }
}
