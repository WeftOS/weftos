package ai.weftos.phonenode

import android.content.Context
import java.net.URI

data class PhoneSettings(
    val endpoint: String = DEFAULT_ENDPOINT,
    val port: Int = DEFAULT_PORT,
    /** Base URL of the chief-of-staff API (`/v1/cos/...`); empty means the card link is off. */
    val cosBase: String = "",
) {
    companion object {
        const val DEFAULT_ENDPOINT = "https://weftos-weaver.vercel.app/api/speech/turn"
        /** Matches the glasses app's default parent port. */
        const val DEFAULT_PORT = 7440
    }
}

/** Input checks for the settings screen; pure so they are unit tested. */
object SettingsCheck {
    /** Null when `raw` is acceptable, otherwise the reason. HTTPS only: the token rides in a header. */
    fun endpointProblem(raw: String): String? {
        val uri = try {
            URI(raw.trim())
        } catch (_: Exception) {
            return "not a valid URL"
        }
        return when {
            uri.scheme != "https" -> "must start with https://"
            uri.host.isNullOrEmpty() -> "missing host"
            uri.userInfo != null -> "must not embed credentials"
            else -> null
        }
    }

    /**
     * Null when `raw` is a usable CoS base URL (or empty: off). HTTPS anywhere; plain HTTP only
     * to a Tailscale MagicDNS name (`*.ts.net`), where WireGuard already encrypts the hop and
     * the network security config permits cleartext. No path past the base, no credentials.
     */
    fun cosBaseProblem(raw: String): String? {
        val t = raw.trim().trimEnd('/')
        if (t.isEmpty()) return null
        val uri = try {
            URI(t)
        } catch (_: Exception) {
            return "not a valid URL"
        }
        val host = uri.host?.lowercase()
        return when {
            host.isNullOrEmpty() -> "missing host"
            uri.userInfo != null -> "must not embed credentials"
            uri.query != null || uri.fragment != null -> "must not have a query"
            uri.scheme == "https" -> null
            uri.scheme == "http" && host.endsWith(".ts.net") -> null
            uri.scheme == "http" -> "plain http only to a *.ts.net name; use https otherwise"
            else -> "must start with https://"
        }
    }

    fun portProblem(raw: String): String? {
        val p = raw.trim().toIntOrNull() ?: return "not a number"
        return if (p in 1024..65535) null else "use 1024 to 65535"
    }
}

/** Non-secret settings and the glasses key the owner already confirmed. The token is elsewhere. */
class SettingsStore(context: Context) {
    private val prefs = context.getSharedPreferences("weft-node-settings", Context.MODE_PRIVATE)

    fun load() = PhoneSettings(
        endpoint = prefs.getString("endpoint", null) ?: PhoneSettings.DEFAULT_ENDPOINT,
        port = prefs.getInt("port", PhoneSettings.DEFAULT_PORT),
        cosBase = prefs.getString("cos_base", null).orEmpty(),
    )

    fun save(s: PhoneSettings) {
        prefs.edit().putString("endpoint", s.endpoint.trim()).putInt("port", s.port)
            .putString("cos_base", s.cosBase.trim().trimEnd('/')).apply()
    }

    var trustedGlasses: String?
        get() = prefs.getString("trusted_glasses", null)
        set(v) {
            prefs.edit().apply { if (v == null) remove("trusted_glasses") else putString("trusted_glasses", v) }.apply()
        }
}
