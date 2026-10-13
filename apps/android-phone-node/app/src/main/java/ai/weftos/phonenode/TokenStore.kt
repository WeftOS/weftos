package ai.weftos.phonenode

/**
 * The node token (a bearer credential for the speech endpoint) is held only sealed at rest.
 * This file is the platform-free part: the abstraction and the storage logic. The Android
 * implementations (Keystore AES-GCM, SharedPreferences) live in `KeystoreTokenStore.kt`.
 * The token is never logged and never appears in `toString`, `describe` or an exception.
 */
interface TokenStore {
    fun get(): String?
    fun set(token: String)
    fun clear()
}

/** Seals bytes with a key the app cannot export (Android Keystore on the device). */
interface SecretBox {
    fun seal(plain: ByteArray): ByteArray

    /** Null when the blob is corrupt or its key is gone. */
    fun open(sealed: ByteArray): ByteArray?
}

/** One opaque blob on disk. */
interface BlobStore {
    fun read(): ByteArray?
    fun write(blob: ByteArray?)
}

object TokenFormat {
    const val MAX_LEN = 4096

    /** Trim a pasted or scanned value; null when it cannot be a token. */
    fun normalize(raw: String?): String? {
        val t = raw?.trim().orEmpty()
        if (t.isEmpty() || t.length > MAX_LEN) return null
        if (t.any { it.isWhitespace() || it.isISOControl() }) return null
        return t
    }

    /** What the UI may show about a token: that it exists, never what it is. */
    fun describe(token: String?): String = if (token == null) "not set" else "set (${token.length} chars)"
}

class SealedTokenStore(private val box: SecretBox, private val blobs: BlobStore) : TokenStore {
    override fun get(): String? {
        val sealed = blobs.read() ?: return null
        val plain = box.open(sealed) ?: return null
        return TokenFormat.normalize(String(plain, Charsets.UTF_8))
    }

    override fun set(token: String) {
        val t = TokenFormat.normalize(token)
            ?: throw IllegalArgumentException("not a valid token (empty, too long, or contains whitespace)")
        blobs.write(box.seal(t.toByteArray(Charsets.UTF_8)))
    }

    override fun clear() = blobs.write(null)

    override fun toString(): String = "SealedTokenStore(${TokenFormat.describe(get())})"
}
