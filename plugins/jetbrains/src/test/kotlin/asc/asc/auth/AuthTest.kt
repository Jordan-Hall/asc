package asc.asc.auth

import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class AuthTest {
    private class FakeBridge(var stored: String? = null, var helper: Boolean = true) : AuthBridge {
        override suspend fun token(host: String): String? = stored
        override suspend fun storeToken(host: String, token: String) {
            if (!helper) throw NoCredentialHelper()
            stored = token
        }
    }

    private class FakeVault : TokenVault {
        val map = HashMap<String, String>()
        override fun get(host: String) = map[host]
        override fun set(host: String, token: String) { map[host] = token }
        override fun delete(host: String) { map.remove(host) }
    }

    private class FakeListener(override val port: Int = 63343) : CallbackListener {
        var handler: ((String?) -> Unit)? = null
        var closed = false
        override fun onCallback(handler: (String?) -> Unit) { this.handler = handler }
        override fun close() { closed = true }
    }

    @Test fun stateIsSingleUseAndUnguessable() {
        val r = StateRegistry()
        val p = r.begin()
        assertEquals(22, p.state.length) // 128 bits, base64url without padding
        assertNotNull(r.consume(p.state))
        assertNull(r.consume(p.state))
        assertNull(r.consume("guess"))
        assertNull(r.consume(null))
    }

    @Test fun stateExpiresAfterFiveMinutes() {
        var t = 1_000L
        val r = StateRegistry(now = { t })
        val p = r.begin()
        t += AuthConstants.STATE_TTL_MS + 1
        assertNull(r.consume(p.state))
        val q = r.begin()
        t += AuthConstants.STATE_TTL_MS
        assertNotNull(r.consume(q.state))
    }

    @Test fun challengeIsS256OfVerifier() {
        // RFC 7636 appendix B vector
        val p = PendingAuth("s", "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", 0)
        assertEquals("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", p.challenge)
    }

    @Test fun authorizeUrlCarriesPkceStateAndLiveLoopbackPort() {
        val p = StateRegistry().begin()
        val url = OAuthUrls.authorize("forge.example.com", OAuthUrls.redirect(63350), p)
        assertTrue(url.startsWith("https://forge.example.com/login/oauth/authorize?"))
        val q = parseQuery(url.substringAfter('?'))
        assertEquals("asc-ide", q["client_id"])
        assertEquals("S256", q["code_challenge_method"])
        assertEquals(p.challenge, q["code_challenge"])
        assertEquals(p.state, q["state"])
        assertEquals("http://127.0.0.1:63350/asc-asc/auth", q["redirect_uri"])
        assertEquals("read:user read:repository write:repository read:issue write:issue", q["scope"])
        assertFalse(url.contains(p.verifier))
    }

    @Test fun hostIsValidated() {
        for (bad in listOf("a/b", "a@b", "a b", "", "a.com/../x", "a.com?x=1", "https://a.com")) {
            try {
                assertForgeHost(bad)
                fail("accepted $bad")
            } catch (e: AuthException) {
                assertEquals(AuthException.Kind.INVALID_HOST, e.kind)
            }
        }
        assertForgeHost("forge.example.com:3000")
    }

    private fun flow(
        bridge: FakeBridge,
        vault: FakeVault,
        l: FakeListener,
        post: HttpPost,
        opened: MutableList<String>,
        changed: MutableList<String?> = mutableListOf(),
        timeout: Long = 5_000,
    ) = AscAuth(
        bridge, vault, post,
        openBrowser = { u ->
            opened += u
            val state = parseQuery(u.substringAfter('?'))["state"]!!
            // The browser returns: a wrong-state request first (must be ignored), then the real one.
            l.handler!!("code=evil&state=nope")
            l.handler!!("code=abc&state=$state")
            l.handler!!("code=replay&state=$state")
        },
        listener = { l },
        onFallbackChanged = { changed += it },
        timeoutMs = timeout,
    )

    @Test fun oauthExchangesCodeWithVerifierAndStoresViaBridge() = runBlocking {
        val bridge = FakeBridge()
        val vault = FakeVault().also { it.map["forge.example.com"] = "stale" }
        val l = FakeListener()
        val opened = mutableListOf<String>()
        var seenForm = ""
        val auth = flow(bridge, vault, l, { url, form ->
            assertEquals("https://forge.example.com/login/oauth/access_token", url)
            seenForm = form
            200 to """{"access_token":"tok123","token_type":"bearer"}"""
        }, opened)
        val s = auth.signInOAuth("forge.example.com")
        assertEquals("tok123", s.token)
        assertEquals("tok123", bridge.stored)
        assertNull(vault.get("forge.example.com")) // helper is the single source
        val f = parseQuery(seenForm)
        assertEquals("abc", f["code"])
        assertEquals("authorization_code", f["grant_type"])
        val authState = parseQuery(opened[0].substringAfter('?'))
        assertNotNull(f["code_verifier"])
        assertEquals(b64url(java.security.MessageDigest.getInstance("SHA-256").digest(f["code_verifier"]!!.toByteArray())), authState["code_challenge"])
        assertTrue(l.closed)
    }

    @Test fun noCredentialHelperFallsBackToPasswordSafeAndRespawnsBridge() = runBlocking {
        val bridge = FakeBridge(helper = false)
        val vault = FakeVault()
        val changed = mutableListOf<String?>()
        val auth = flow(bridge, vault, FakeListener(), { _, _ -> 200 to """{"access_token":"t9"}""" }, mutableListOf(), changed)
        auth.signInOAuth("forge.example.com")
        assertEquals("t9", vault.get("forge.example.com"))
        assertEquals(listOf<String?>("t9"), changed)
        assertEquals("t9", auth.current("forge.example.com"))
        auth.signOut("forge.example.com")
        assertNull(vault.get("forge.example.com"))
        assertEquals(listOf<String?>("t9", null), changed)
    }

    @Test fun timeoutWhenNoCallback() {
        val auth = AscAuth(
            FakeBridge(), FakeVault(), { _, _ -> 200 to "{}" },
            openBrowser = {}, listener = { FakeListener() }, timeoutMs = 50,
        )
        try {
            runBlocking { auth.signInOAuth("forge.example.com") }
            fail("expected timeout")
        } catch (e: AuthException) {
            assertEquals(AuthException.Kind.TIMEOUT, e.kind)
        }
    }

    @Test fun deniedAndBadExchangeAreErrors() {
        val l = FakeListener()
        val denied = AscAuth(
            FakeBridge(), FakeVault(), { _, _ -> 200 to "{}" },
            openBrowser = { u -> l.handler!!("error=access_denied&state=" + parseQuery(u.substringAfter('?'))["state"]) },
            listener = { l }, timeoutMs = 1000,
        )
        try {
            runBlocking { denied.signInOAuth("forge.example.com") }
            fail()
        } catch (e: AuthException) {
            assertEquals(AuthException.Kind.DENIED, e.kind)
        }
        val bad = flow(FakeBridge(), FakeVault(), FakeListener(), { _, _ -> 500 to "" }, mutableListOf())
        try {
            runBlocking { bad.signInOAuth("forge.example.com") }
            fail()
        } catch (e: AuthException) {
            assertEquals(AuthException.Kind.EXCHANGE_FAILED, e.kind)
        }
    }

    @Test fun patIsStoredTrimmedAndNeverEmpty() = runBlocking {
        val bridge = FakeBridge()
        val auth = AscAuth(bridge, FakeVault(), { _, _ -> 200 to "" }, openBrowser = {})
        assertEquals("pat1", auth.signInPat("forge.example.com", "  pat1\n").token)
        assertEquals("pat1", bridge.stored)
        try {
            auth.signInPat("forge.example.com", "  ")
            fail()
        } catch (e: AuthException) {
            assertEquals(AuthException.Kind.CANCELLED, e.kind)
        }
    }

    @Test fun loopbackListenerBindsLoopbackOnlyAndDeliversQuery() {
        LoopbackListener().use { l ->
            assertTrue(l.port > 0)
            var got: String? = null
            l.onCallback { got = it }
            val c = URL("http://127.0.0.1:${l.port}${AuthConstants.CALLBACK_PATH}?code=c&state=s").openConnection() as HttpURLConnection
            assertEquals(200, c.responseCode)
            assertEquals("code=c&state=s", got)
        }
    }

    @Test fun configureAgentsRunsFixedArgvInRoot() {
        var seen: Pair<List<String>, String>? = null
        val msg = ConfigureAgents.run("asc", "/work/repo", { cmd, dir -> seen = cmd to dir.path; 0 })
        assertEquals(listOf("asc", "mcp", "install", "--scope", "project"), seen!!.first)
        assertTrue(msg.startsWith("Agents configured"))
        assertTrue(ConfigureAgents.run("asc", "/w", { _, _ -> 1 }).contains("failed"))
        assertTrue(ConfigureAgents.run("asc", "/w", { _, _ -> error("x") }).contains("failed"))
        assertTrue(ConfigureAgents.run(null, "/w").contains("not found"))
        assertTrue(ConfigureAgents.run("asc", null).contains("Open an asc workspace"))
    }
}
