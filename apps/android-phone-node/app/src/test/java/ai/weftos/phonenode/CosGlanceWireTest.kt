package ai.weftos.phonenode

import java.time.Instant
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class CosGlanceWireTest {
    private val now = 1_800_000_000_000L

    @Test
    fun cardEventAsTheCosEmitsIt() {
        // The shape `weftos-cos` serializes for `Event::Card` (the event's `kind` is "card";
        // the card's own kind is the `card` field).
        val data = """{"kind":"card","card":"needs_you","id":"1a2b3c4d","title":"Ship rc.16?",""" +
            """"detail":"A: ship / B: soak a day","options":["Ship","Soak"],"ttl_minutes":240,""" +
            """"reason":"release blocks on this"}"""
        val u = CosGlanceWire.cardEvent(data, now) as CardUpdate.Show
        assertEquals(
            CosCard(
                kind = CardKind.NeedsYou,
                id = "1a2b3c4d",
                title = "Ship rc.16?",
                detail = "A: ship / B: soak a day",
                options = listOf("Ship", "Soak"),
                reason = "release blocks on this",
                expiresAtMs = now + 240 * 60_000L,
            ),
            u.card,
        )
        assertTrue(u.card.isLive(now + 1))
        assertTrue(!u.card.isLive(now + 240 * 60_000L))
    }

    @Test
    fun clearEventAndOtherKinds() {
        assertEquals(
            CardUpdate.Clear("1a2b3c4d", "answered"),
            CosGlanceWire.cardEvent("""{"kind":"card","card":"clear","id":"1a2b3c4d","reason":"answered"}""", now),
        )
        assertEquals(CardUpdate.Clear("", ""), CosGlanceWire.cardEvent("""{"kind":"card","card":"clear"}""", now))
        assertNull(CosGlanceWire.cardEvent("""{"kind":"state","state":"thinking"}""", now))
        assertNull(CosGlanceWire.cardEvent("""{"kind":"card","card":"someday","id":"x","title":"t"}""", now))
        assertNull(CosGlanceWire.cardEvent("not json", now))
        // No id: it could never be answered, so it is not shown.
        assertNull(CosGlanceWire.cardEvent("""{"kind":"card","card":"now","title":"t"}""", now))
    }

    @Test
    fun optionsAreTrimmedAndCappedAtTwoAndExpiresIsAccepted() {
        val data = """{"kind":"card","card":"now","id":"a","title":"t","options":[" A ","","B","C"],""" +
            """"expires":"2027-01-15T08:00:00Z"}"""
        val c = (CosGlanceWire.cardEvent(data, now) as CardUpdate.Show).card
        assertEquals(listOf("A", "B"), c.options)
        assertEquals(Instant.parse("2027-01-15T08:00:00Z").toEpochMilli(), c.expiresAtMs)
        val noTtl = (CosGlanceWire.cardEvent("""{"kind":"card","card":"eod","id":"b","title":"t"}""", now) as CardUpdate.Show).card
        assertEquals(0L, noTtl.expiresAtMs)
        assertTrue(noTtl.isLive(Long.MAX_VALUE))
    }

    @Test
    fun glanceWithCardFieldsAndArt() {
        val body = """{"card":{"id":"9f","kind":"waiting_on","title":"Bob: PR review","detail":"since 10:00",""" +
            """"alert":"","options":[],"reason":"blocked","expires_at":"2026-10-25T18:00:00Z"},""" +
            """"fields":{"state":"idle","task":"triage","next":"standup","alert":"","model":"local/glm","ts":"09:05"},""" +
            """"art":{"caption":"a heron","at":"2026-10-25T09:00:00Z","png_url":"/v1/cos/glance/art.png"}}"""
        val g = CosGlanceWire.glance(body)!!
        assertEquals(CardKind.WaitingOn, g.card!!.kind)
        assertEquals("9f", g.card!!.id)
        assertEquals(Instant.parse("2026-10-25T18:00:00Z").toEpochMilli(), g.card!!.expiresAtMs)
        assertEquals(GlanceFields("idle", "triage", "standup", "", "local/glm", "09:05"), g.fields)
        assertEquals(GlanceArt("a heron", "2026-10-25T09:00:00Z", "/v1/cos/glance/art.png"), g.art)
    }

    @Test
    fun glanceWithNullsAndNullFields() {
        val g = CosGlanceWire.glance("""{"card":null,"fields":{"state":"idle","task":null},"art":null}""")!!
        assertNull(g.card)
        assertNull(g.art)
        assertEquals(GlanceFields(state = "idle"), g.fields)
        // The default trait body before the agent fills it in.
        assertEquals(GlanceFields(), CosGlanceWire.glance("""{"card":null,"fields":null,"art":null}""")!!.fields)
        assertNull(CosGlanceWire.glance("<html>"))
    }

    @Test
    fun artWithoutUrlFallsBackToTheRoute() {
        val g = CosGlanceWire.glance("""{"card":null,"fields":{},"art":{"caption":"c","at":null}}""")!!
        assertEquals(GlanceArt("c", "", "/v1/cos/glance/art.png"), g.art)
    }

    @Test
    fun clearRemovesOnlyTheCardItNames() {
        val c = CosCard(CardKind.NeedsYou, "3fa81c0d", "Ship rc.16?")
        assertNull(CosGlanceWire.reduce(c, CardUpdate.Clear("3fa81c0d", "answered")))
        assertEquals(c, CosGlanceWire.reduce(c, CardUpdate.Clear("00000000", "answered")))
        assertEquals(c, CosGlanceWire.reduce(c, CardUpdate.Clear("", "")))
        assertNull(CosGlanceWire.reduce(null, CardUpdate.Clear("3fa81c0d", "")))
        val d = c.copy(id = "11111111")
        assertEquals(d, CosGlanceWire.reduce(c, CardUpdate.Show(d)))
    }

    @Test
    fun freeTextReplyLimits() {
        assertNull(CosGlanceWire.replyProblem("soak it"))
        assertNull(CosGlanceWire.replyProblem("x".repeat(200)))
        // Counted as the CoS counts: code points of the trimmed text.
        assertNull(CosGlanceWire.replyProblem("  " + "\uD83D\uDE00".repeat(200) + "  "))
        assertNotNull(CosGlanceWire.replyProblem("x".repeat(201)))
        assertNotNull(CosGlanceWire.replyProblem("   "))
        assertEquals("ok", JSONObject(CosGlanceWire.answerBody("3fa81c0d", "  ok \n")).getString("value"))
    }

    @Test
    fun answerBodyShape() {
        val j = JSONObject(CosGlanceWire.answerBody("1a2b3c4d", "Soak \"a day\""))
        assertEquals(setOf("kind", "id", "value"), j.keySet())
        assertEquals("answer", j.getString("kind"))
        assertEquals("1a2b3c4d", j.getString("id"))
        assertEquals("Soak \"a day\"", j.getString("value"))
    }
}
