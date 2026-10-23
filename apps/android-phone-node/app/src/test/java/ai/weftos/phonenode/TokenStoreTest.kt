package ai.weftos.phonenode

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TokenStoreTest {
    /** Reversible stand-in for the Keystore box: XOR with a key byte, plus a marker. */
    private class XorBox(var keyByte: Int = 0x5a) : SecretBox {
        override fun seal(plain: ByteArray) = byteArrayOf(1) + ByteArray(plain.size) { (plain[it].toInt() xor keyByte).toByte() }
        override fun open(sealed: ByteArray): ByteArray? {
            if (sealed.isEmpty() || sealed[0] != 1.toByte()) return null
            return ByteArray(sealed.size - 1) { (sealed[it + 1].toInt() xor keyByte).toByte() }
        }
    }

    private class MemBlob : BlobStore {
        var blob: ByteArray? = null
        var writes = 0
        override fun read() = blob
        override fun write(blob: ByteArray?) {
            this.blob = blob
            writes++
        }
    }

    private fun store(box: SecretBox = XorBox(), blobs: BlobStore = MemBlob()) = SealedTokenStore(box, blobs)

    @Test
    fun roundTripsAndTrimsWhitespace() {
        val s = store()
        assertNull(s.get())
        s.set("  wt_live_abc123  \n")
        assertEquals("wt_live_abc123", s.get())
    }

    @Test
    fun theTokenIsNotInTheBlobAtRest() {
        val blobs = MemBlob()
        val s = store(blobs = blobs)
        s.set("wt_live_abc123")
        val at = blobs.blob!!
        assertFalse(String(at, Charsets.ISO_8859_1).contains("wt_live_abc123"))
        assertTrue(at.isNotEmpty())
    }

    @Test
    fun clearRemovesIt() {
        val blobs = MemBlob()
        val s = store(blobs = blobs)
        s.set("tok")
        s.clear()
        assertNull(s.get())
        assertNull(blobs.blob)
    }

    @Test
    fun aLostKeyOrCorruptBlobReadsAsNoToken() {
        val blobs = MemBlob()
        store(box = XorBox(), blobs = blobs).set("tok")
        blobs.blob!![0] = 9 // damage the marker
        assertNull(store(blobs = blobs).get())
        blobs.blob = null
        assertNull(store(blobs = blobs).get())
    }

    @Test
    fun invalidTokensAreRejectedWithoutEchoingThem() {
        val s = store()
        for (bad in listOf("", "   ", "has space", "tab\there", "x".repeat(TokenFormat.MAX_LEN + 1))) {
            val e = runCatching { s.set(bad) }.exceptionOrNull()
            assertTrue("accepted: ${bad.take(12)}", e is IllegalArgumentException)
            assertFalse(e!!.message.orEmpty().contains(bad.trim().ifEmpty { "\u0000" }))
        }
        assertNull(s.get())
    }

    @Test
    fun noDescriptionOrToStringRevealsTheToken() {
        val s = store()
        s.set("wt_live_abc123")
        assertEquals("set (14 chars)", TokenFormat.describe(s.get()))
        assertEquals("not set", TokenFormat.describe(null))
        assertFalse(s.toString().contains("wt_live"))
        assertFalse(TokenFormat.describe(s.get()).contains("abc"))
    }

    @Test
    fun normalizeAcceptsOnlyPlausibleTokens() {
        assertEquals("a.b-c_d", TokenFormat.normalize(" a.b-c_d "))
        assertNull(TokenFormat.normalize(null))
        assertNull(TokenFormat.normalize("a b"))
        assertNull(TokenFormat.normalize("a\u0007b"))
    }
}
