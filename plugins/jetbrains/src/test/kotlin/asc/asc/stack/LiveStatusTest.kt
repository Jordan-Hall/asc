package asc.asc.stack

import org.junit.Test
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue

class LiveStatusTest {
    private class Rec : LiveVerbs {
        val calls = mutableListOf<Pair<String, List<String>>>()
        override fun run(verb: String, args: List<String>): Int {
            calls.add(verb to args)
            return 0
        }
    }

    @Test
    fun restackItemShowsNAndHidesAtZero() {
        assertEquals("trunk moved 3 commits - Restack", LiveStatus.restackText(3))
        assertEquals("trunk moved 1 commit - Restack", LiveStatus.restackText(1))
        assertNull(LiveStatus.restackText(0))
        assertNull(LiveStatus.restackText(-4))
        assertEquals("trunk moved 999 commits - Restack", LiveStatus.restackText(Int.MAX_VALUE))
    }

    @Test
    fun toggleCallsTheMatchingVerb() {
        val r = Rec()
        assertTrue(LiveStatus.toggle(LiveState(paused = false, conflict = false), r))
        assertTrue(LiveStatus.toggle(LiveState(paused = true, conflict = false), r))
        assertEquals(listOf("live" to listOf("pause"), "live" to listOf("resume")), r.calls)
    }

    @Test
    fun restackRunsOnlyRestack() {
        val r = Rec()
        assertTrue(LiveStatus.restack(r))
        assertEquals(listOf("restack" to emptyList<String>()), r.calls)
    }

    @Test
    fun noticeAppearsOnConflictPauseAndClearsOnResume() {
        val paused = LiveStatus.parse("""{"paused":true,"pauseReason":"conflict","detail":"<script>"}""")
        val text = LiveStatus.notice(paused)
        assertTrue(text != null && !text.contains("script"))
        assertEquals("resume", LiveStatus.toggleVerb(paused))
        val resumed = LiveStatus.parse("""{"paused":false,"pauseReason":null}""")
        assertNull(LiveStatus.notice(resumed))
        assertNull(LiveStatus.notice(LiveStatus.parse("""{"paused":true,"pauseReason":"user"}""")))
    }

    @Test
    fun junkStatusIsRejected() {
        for (bad in listOf(null, "", "{", "[]", """{"paused":"yes"}""")) {
            assertNull(LiveStatus.parse(bad))
        }
        assertFalse(LiveStatus.notice(null) != null)
    }
}
