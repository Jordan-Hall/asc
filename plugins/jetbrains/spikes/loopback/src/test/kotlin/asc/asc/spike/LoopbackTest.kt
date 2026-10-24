package asc.asc.spike

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class LoopbackTest {
    private var t = 1_000L
    private fun store() = StateStore { t }
    private fun cb(s: StateStore, state: String?, code: String? = "abc"): Callback {
        val q = listOfNotNull(state?.let { "state=$it" }, code?.let { "code=$it" }).joinToString("&")
        return CallbackHandler.handle(s, CALLBACK_PATH, q, true)
    }

    @Test fun pkceRfc7636Vector() {
        assertEquals("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", Pkce.challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"))
    }

    @Test fun pkceVerifierShapeAndUniqueness() {
        val a = Pkce.verifier(); val b = Pkce.verifier()
        assertEquals(43, a.length); assertNotEquals(a, b)
        assertTrue(Pkce.matches(a, Pkce.challenge(a)))
        assertFalse(Pkce.matches(b, Pkce.challenge(a)))
    }

    @Test fun stateIs128BitsHexAndUnique() {
        val s = store()
        val a = s.start(63342).state; val b = s.start(63342).state
        assertTrue(Regex("[0-9a-f]{32}").matches(a)); assertNotEquals(a, b)
    }

    @Test fun validCallbackAcceptedOnce() {
        val s = store(); val f = s.start(63343)
        val r = cb(s, f.state, "code1")
        assertTrue(r is Callback.Accepted)
        r as Callback.Accepted
        assertEquals("code1", r.code)
        assertEquals(f.verifier, r.flow.verifier)
        assertEquals(Callback.Ignored, cb(s, f.state, "code1")) // replay
    }

    @Test fun unknownOrMissingStateIgnoredAndPendingFlowKept() {
        val s = store(); val f = s.start(63342)
        assertEquals(Callback.Ignored, cb(s, "00000000000000000000000000000000"))
        assertEquals(Callback.Ignored, cb(s, null))
        assertEquals(1, s.pending())
        assertTrue(cb(s, f.state) is Callback.Accepted)
    }

    @Test fun expiredStateIgnored() {
        val s = store(); val f = s.start(63342)
        t += STATE_TTL_MS + 1
        assertEquals(Callback.Ignored, cb(s, f.state))
        assertEquals(0, s.pending())
    }

    @Test fun stateValidAtExactlyFiveMinutes() {
        val s = store(); val f = s.start(63342)
        t += STATE_TTL_MS
        assertTrue(cb(s, f.state) is Callback.Accepted)
    }

    @Test fun errorOrMissingCodeYieldsNoCodeAndEndsFlow() {
        val s = store(); val f = s.start(63342)
        assertEquals(Callback.Ignored, CallbackHandler.handle(s, CALLBACK_PATH, "state=${f.state}&error=access_denied", true))
        assertEquals(0, s.pending())
        val g = s.start(63342)
        assertEquals(Callback.Ignored, cb(s, g.state, null))
    }

    @Test fun duplicateStateParamFirstWins() {
        val s = store(); val f = s.start(63342)
        assertEquals(Callback.Ignored, CallbackHandler.handle(s, CALLBACK_PATH, "state=bad&state=${f.state}&code=x", true))
    }

    @Test fun nonLoopbackPeerAndWrongPathIgnored() {
        val s = store(); val f = s.start(63342)
        assertEquals(Callback.Ignored, CallbackHandler.handle(s, CALLBACK_PATH, "state=${f.state}&code=x", false))
        assertEquals(Callback.Ignored, CallbackHandler.handle(s, "/other", "state=${f.state}&code=x", true))
        assertEquals(1, s.pending())
    }

    @Test fun redirectFollowsMovedPort() {
        val s = store()
        for (p in listOf(63342, 63343, 63400, 51234)) {
            val f = s.start(p)
            assertEquals("http://127.0.0.1:$p$CALLBACK_PATH", f.redirect)
            assertTrue(Redirect.p06Accepts(f.redirect))
        }
    }

    @Test fun redirectRejectsBadPort() {
        for (p in listOf(0, -1, 65536)) {
            try { Redirect.build(p); fail("accepted $p") } catch (e: IllegalArgumentException) { /* expected */ }
        }
    }

    @Test fun p06ModelRejectsNonLoopbackShapes() {
        val bad = listOf(
            "http://localhost:63342$CALLBACK_PATH", "http://127.0.0.1$CALLBACK_PATH", "https://127.0.0.1:63342$CALLBACK_PATH",
            "http://127.0.0.1:63342/other", "http://127.0.0.1:63342$CALLBACK_PATH?x=1", "http://127.0.0.1:63342$CALLBACK_PATH#f",
            "http://evil.com@127.0.0.1:63342$CALLBACK_PATH", "http://127.0.0.1.evil.com:63342$CALLBACK_PATH",
            "http://0.0.0.0:63342$CALLBACK_PATH", "http://127.0.0.1:99999$CALLBACK_PATH", "vscode://asc.asc/auth", "not a uri",
        )
        for (r in bad) assertFalse(r, Redirect.p06Accepts(r))
        assertTrue(Redirect.p06Accepts("http://[::1]:5000$CALLBACK_PATH"))
    }

    @Test fun authorizeUrlCarriesS256StateAndRedirect() {
        val f = store().start(63342)
        val u = f.authorizeUrl("https://forge.example/login/oauth/authorize", scopes = listOf("read:user", "read:repository"))
        assertTrue(u.contains("code_challenge_method=S256"))
        assertTrue(u.contains("state=${f.state}"))
        assertTrue(u.contains("code_challenge=${f.challenge}"))
        assertFalse(u.contains(f.verifier))
        assertTrue(u.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A63342"))
        try {
            f.authorizeUrl("http://forge.example/authorize", scopes = listOf()); fail("http accepted")
        } catch (e: IllegalArgumentException) { /* expected */ }
    }

    @Test fun listenerUsesLiveBuiltInPortEachTime() {
        var live: Int? = 63342
        LoopbackListener.open { live }.use { assertEquals(63342, it.port); assertFalse(it.ownsSocket) }
        live = 63351 // the built-in server moved
        LoopbackListener.open { live }.use { assertEquals(63351, it.port) }
    }

    @Test fun listenerFallsBackToOwnLoopbackSocket() {
        val sources = listOf<() -> Int?>({ null }, { throw IllegalStateException("no server") }, { 0 })
        for (src in sources) {
            LoopbackListener.open(src).use {
                assertTrue(it.ownsSocket); assertTrue(it.port in 1..65535)
                assertTrue(Redirect.p06Accepts(Redirect.build(it.port)))
            }
        }
    }
}
