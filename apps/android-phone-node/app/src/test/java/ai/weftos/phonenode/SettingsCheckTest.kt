package ai.weftos.phonenode

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class SettingsCheckTest {
    @Test
    fun theDefaultEndpointIsValid() {
        assertNull(SettingsCheck.endpointProblem(PhoneSettings.DEFAULT_ENDPOINT))
        assertEquals(7440, PhoneSettings.DEFAULT_PORT)
    }

    @Test
    fun endpointMustBeHttpsWithAHostAndNoCredentials() {
        assertNotNull(SettingsCheck.endpointProblem("http://example.invalid/api/speech/turn"))
        assertNotNull(SettingsCheck.endpointProblem("https:///no-host"))
        assertNotNull(SettingsCheck.endpointProblem("https://user:pw@example.invalid/x"))
        assertNotNull(SettingsCheck.endpointProblem("not a url"))
        assertNull(SettingsCheck.endpointProblem(" https://example.invalid/api/speech/turn "))
    }

    @Test
    fun portIsAnUnprivilegedNumber() {
        assertNull(SettingsCheck.portProblem("7440"))
        assertNull(SettingsCheck.portProblem(" 65535 "))
        assertNotNull(SettingsCheck.portProblem("80"))
        assertNotNull(SettingsCheck.portProblem("70000"))
        assertNotNull(SettingsCheck.portProblem("abc"))
    }
}
