package asc.asc.auth

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeoutOrNull

/** Bridge credential calls (SPEC 24.6, P01c). `token` returns null on NOT_FOUND. */
interface AuthBridge {
    suspend fun token(host: String): String?

    /** Throws [NoCredentialHelper] when no helper is configured. */
    suspend fun storeToken(host: String, token: String)
}

class NoCredentialHelper : Exception("no credential helper is configured")

/** The fallback store (the JetBrains `PasswordSafe`), keyed `asc.asc.token.<host>`. */
interface TokenVault {
    fun get(host: String): String?
    fun set(host: String, token: String)
    fun delete(host: String)
}

fun interface HttpPost {
    /** POST a form body; returns the status and body. */
    fun post(url: String, form: String): Pair<Int, String>
}

data class SignIn(val host: String, val token: String)

class AscAuth(
    private val bridge: AuthBridge,
    private val vault: TokenVault,
    private val http: HttpPost,
    private val openBrowser: (String) -> Unit,
    private val registry: StateRegistry = StateRegistry(),
    private val listener: () -> CallbackListener = { LoopbackListener() },
    /** Called with the fallback token and the one host it is for (both null on sign-out), so the bridge is respawned with `ASC_TOKEN` and `ASC_TOKEN_HOST`. */
    private val onFallbackChanged: (String?, String?) -> Unit = { _, _ -> },
    private val timeoutMs: Long = AuthConstants.STATE_TTL_MS,
) {
    /** `Bridge.Token`, then the vault. */
    suspend fun current(host: String): String? {
        assertForgeHost(host)
        return bridge.token(host) ?: vault.get(host)
    }

    suspend fun signInOAuth(host: String): SignIn {
        assertForgeHost(host)
        val p = registry.begin()
        val got = CompletableDeferred<Map<String, String>>()
        val l = listener()
        try {
            l.onCallback { raw ->
                val q = parseQuery(raw)
                // Ignored unless the state is known, unexpired and unused; a stray request cannot end the flow.
                if (registry.consume(q["state"]) != null) got.complete(q)
            }
            val redirect = OAuthUrls.redirect(l.port)
            openBrowser(OAuthUrls.authorize(host, redirect, p))
            val q = withTimeoutOrNull(timeoutMs) { got.await() }
                ?: throw AuthException(AuthException.Kind.TIMEOUT, "sign-in timed out")
            q["error"]?.let { throw AuthException(AuthException.Kind.DENIED, "authorization denied: $it") }
            val code = q["code"]?.takeIf { it.isNotEmpty() }
                ?: throw AuthException(AuthException.Kind.EXCHANGE_FAILED, "no authorization code")
            val token = exchange(host, code, p.verifier, redirect)
            return save(host, token)
        } finally {
            registry.discard(p.state)
            l.close()
        }
    }

    suspend fun signInPat(host: String, pat: String): SignIn {
        assertForgeHost(host)
        val t = pat.trim()
        if (t.isEmpty()) throw AuthException(AuthException.Kind.CANCELLED, "no token entered")
        return save(host, t)
    }

    fun signOut(host: String) {
        assertForgeHost(host)
        vault.delete(host)
        onFallbackChanged(null, null)
    }

    private fun exchange(host: String, code: String, verifier: String, redirect: String): String {
        val (status, body) = http.post(OAuthUrls.tokenEndpoint(host), OAuthUrls.exchangeBody(code, verifier, redirect))
        if (status !in 200..299) throw AuthException(AuthException.Kind.EXCHANGE_FAILED, "token endpoint returned $status")
        val m = Regex("\"access_token\"\\s*:\\s*\"([^\"\\\\]+)\"").find(body)
            ?: throw AuthException(AuthException.Kind.EXCHANGE_FAILED, "token endpoint returned no access_token")
        return m.groupValues[1]
    }

    private suspend fun save(host: String, token: String): SignIn {
        try {
            bridge.storeToken(host, token)
            vault.delete(host) // a helper holds it now; the helper is the single source
        } catch (_: NoCredentialHelper) {
            vault.set(host, token)
            onFallbackChanged(token, host)
        }
        return SignIn(host, token)
    }
}
