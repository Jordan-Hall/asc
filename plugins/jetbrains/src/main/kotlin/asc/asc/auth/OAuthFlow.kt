package asc.asc.auth

import java.net.URLEncoder
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64

/** SPEC 24.6: the pre-registered public client, its scopes and the state lifetime. */
object AuthConstants {
    const val CLIENT_ID = "asc-ide"
    const val STATE_TTL_MS: Long = 5 * 60 * 1000
    const val CALLBACK_PATH = "/asc-asc/auth"
    val SCOPES = listOf("read:user", "read:repository", "write:repository", "read:issue", "write:issue")

    fun passwordSafeService(host: String): String = "asc.asc.token.$host"
}

class AuthException(val kind: Kind, message: String) : Exception(message) {
    enum class Kind { TIMEOUT, DENIED, EXCHANGE_FAILED, INVALID_HOST, CANCELLED }
}

private val HOST_RE = Regex("^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?(:\\d{1,5})?$")

/** Bare host[:port]; anything that could change the URL shape is refused. */
fun assertForgeHost(host: String) {
    if (!HOST_RE.matches(host)) throw AuthException(AuthException.Kind.INVALID_HOST, "invalid forge host")
}

private val b64 = Base64.getUrlEncoder().withoutPadding()

fun b64url(bytes: ByteArray): String = b64.encodeToString(bytes)

private fun enc(s: String): String = URLEncoder.encode(s, StandardCharsets.UTF_8)

fun formEncode(params: Map<String, String>): String =
    params.entries.joinToString("&") { "${enc(it.key)}=${enc(it.value)}" }

/** One sign-in attempt: a PKCE verifier and a single-use state (128 random bits, valid 5 minutes). */
class PendingAuth(val state: String, val verifier: String, val expiresAt: Long) {
    val challenge: String =
        b64url(MessageDigest.getInstance("SHA-256").digest(verifier.toByteArray(StandardCharsets.US_ASCII)))
}

/**
 * The pending-state registry. A callback is accepted once, only when `state` is known and unexpired; the entry
 * is removed on first sight, so a replay, a guess or a late arrival is ignored (returns null).
 */
class StateRegistry(
    private val now: () -> Long = System::currentTimeMillis,
    private val random: SecureRandom = SecureRandom(),
) {
    private val pending = HashMap<String, PendingAuth>()

    @Synchronized
    fun begin(): PendingAuth {
        val state = b64url(ByteArray(16).also(random::nextBytes))
        val verifier = b64url(ByteArray(32).also(random::nextBytes))
        val p = PendingAuth(state, verifier, now() + AuthConstants.STATE_TTL_MS)
        pending[state] = p
        return p
    }

    @Synchronized
    fun consume(state: String?): PendingAuth? {
        if (state == null) return null
        val p = pending.remove(state) ?: return null
        return if (now() > p.expiresAt) null else p
    }

    @Synchronized
    fun discard(state: String) {
        pending.remove(state)
    }
}

object OAuthUrls {
    /** The redirect is built from the live loopback port (RFC 8252 7.3): `127.0.0.1`, never `localhost`. */
    fun redirect(port: Int): String = "http://127.0.0.1:$port${AuthConstants.CALLBACK_PATH}"

    fun authorize(host: String, redirect: String, p: PendingAuth): String {
        assertForgeHost(host)
        val q = formEncode(
            linkedMapOf(
                "response_type" to "code",
                "client_id" to AuthConstants.CLIENT_ID,
                "redirect_uri" to redirect,
                "scope" to AuthConstants.SCOPES.joinToString(" "),
                "state" to p.state,
                "code_challenge" to p.challenge,
                "code_challenge_method" to "S256",
            ),
        )
        return "https://$host/login/oauth/authorize?$q"
    }

    fun tokenEndpoint(host: String): String {
        assertForgeHost(host)
        return "https://$host/login/oauth/access_token"
    }

    fun exchangeBody(code: String, verifier: String, redirect: String): String = formEncode(
        linkedMapOf(
            "grant_type" to "authorization_code",
            "client_id" to AuthConstants.CLIENT_ID,
            "code" to code,
            "code_verifier" to verifier,
            "redirect_uri" to redirect,
        ),
    )
}

/** Parse a query string into its first values. */
fun parseQuery(raw: String?): Map<String, String> {
    if (raw.isNullOrEmpty()) return emptyMap()
    val out = LinkedHashMap<String, String>()
    for (part in raw.split('&')) {
        if (part.isEmpty()) continue
        val i = part.indexOf('=')
        val k = java.net.URLDecoder.decode(if (i < 0) part else part.substring(0, i), StandardCharsets.UTF_8)
        val v = if (i < 0) "" else java.net.URLDecoder.decode(part.substring(i + 1), StandardCharsets.UTF_8)
        out.putIfAbsent(k, v)
    }
    return out
}
