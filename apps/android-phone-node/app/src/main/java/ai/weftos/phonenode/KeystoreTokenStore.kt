package ai.weftos.phonenode

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** AES-256-GCM under a non-exportable Android Keystore key; the 12-byte IV leads the blob. */
class KeystoreSecretBox(private val alias: String = "weft-node-token") : SecretBox {
    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(alias, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(
                alias,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return gen.generateKey()
    }

    override fun seal(plain: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        return c.iv + c.doFinal(plain)
    }

    override fun open(sealed: ByteArray): ByteArray? {
        if (sealed.size <= IV_LEN) return null
        return try {
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, sealed, 0, IV_LEN))
            c.doFinal(sealed, IV_LEN, sealed.size - IV_LEN)
        } catch (_: Exception) {
            null // key invalidated or blob damaged: treat as no token
        }
    }

    private companion object {
        const val IV_LEN = 12
    }
}

class PrefsBlobStore(context: Context, private val name: String = "token") : BlobStore {
    private val prefs = context.getSharedPreferences("weft-node-secret", Context.MODE_PRIVATE)

    override fun read(): ByteArray? =
        prefs.getString(name, null)?.let { Base64.decode(it, Base64.NO_WRAP) }

    override fun write(blob: ByteArray?) {
        prefs.edit().apply {
            if (blob == null) remove(name) else putString(name, Base64.encodeToString(blob, Base64.NO_WRAP))
        }.apply()
    }
}

fun keystoreTokenStore(context: Context): TokenStore =
    SealedTokenStore(KeystoreSecretBox(), PrefsBlobStore(context))

/** The CoS endpoint token (`weftos-cos endpoint add`), sealed under its own Keystore key. */
fun cosTokenStore(context: Context): TokenStore =
    SealedTokenStore(KeystoreSecretBox("weft-cos-token"), PrefsBlobStore(context, "cos_token"))
