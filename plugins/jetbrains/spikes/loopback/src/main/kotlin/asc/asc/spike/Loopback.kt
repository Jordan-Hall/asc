package asc.asc.spike

import java.net.InetAddress
import java.net.ServerSocket
import java.net.URI
import java.net.URLDecoder
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64

/** Spike P55 (SPEC 24.6): JetBrains OAuth loopback with `state`, PKCE and a variable-port redirect. */

const val CALLBACK_PATH = "/api/asc/oauth/callback"
const val STATE_TTL_MS = 5 * 60 * 1000L

private val rng = SecureRandom()
private val b64 = Base64.getUrlEncoder().withoutPadding()

object Pkce {
    /** 32 random bytes -> 43 unreserved characters (RFC 7636 4.1). */
    fun verifier(): String = ByteArray(32).also(rng::nextBytes).let(b64::encodeToString)

    /** S256 only; `plain` is never offered. */
    fun challenge(verifier: String): String =
        b64.encodeToString(MessageDigest.getInstance("SHA-256").digest(verifier.toByteArray(Charsets.US_ASCII)))

    fun matches(verifier: String, challenge: String): Boolean =
        MessageDigest.isEqual(challenge(verifier).toByteArray(), challenge.toByteArray())
}

/** The redirect is built from the live port; there is no fixed port. Always `127.0.0.1`, never `localhost` (RFC 8252 7.3). */
object Redirect {
    fun build(port: Int): String {
        require(port in 1..65535) { "bad port $port" }
        return "http://127.0.0.1:$port$CALLBACK_PATH"
    }

    /**
     * Model of what P06 must accept for `asc-ide` loopback redirects: http, literal 127.0.0.1 or [::1], any port,
     * exact path, no userinfo, query or fragment. Everything else is rejected.
     */
    fun p06Accepts(redirect: String): Boolean {
        val u = try { URI(redirect) } catch (e: Exception) { return false }
        if (u.scheme != "http" || u.userInfo != null || u.query != null || u.fragment != null) return false
        if (u.host != "127.0.0.1" && u.host != "[::1]") return false
        if (u.port !in 1..65535) return false
        return u.rawPath == CALLBACK_PATH
    }
}

class Flow(val state: String, val verifier: String, val redirect: String, val expiresAtMs: Long) {
    val challenge: String get() = Pkce.challenge(verifier)

    /** The authorize URL; the caller supplies the forge authorize endpoint (https only). */
    fun authorizeUrl(authorizeEndpoint: String, clientId: String = "asc-ide", scopes: List<String>): String {
        require(authorizeEndpoint.startsWith("https://")) { "authorize endpoint must be https" }
        fun e(s: String) = java.net.URLEncoder.encode(s, Charsets.UTF_8)
        return authorizeEndpoint + "?response_type=code&client_id=" + e(clientId) + "&redirect_uri=" + e(redirect) +
            "&scope=" + e(scopes.joinToString(" ")) + "&state=" + e(state) +
            "&code_challenge=" + e(challenge) + "&code_challenge_method=S256"
    }
}

/** Single-use `state` values (128 random bits, 5 minute validity). */
class StateStore(private val now: () -> Long = System::currentTimeMillis) {
    private val live = HashMap<String, Flow>()

    @Synchronized
    fun start(port: Int): Flow {
        val state = ByteArray(16).also(rng::nextBytes).joinToString("") { "%02x".format(it) }
        val f = Flow(state, Pkce.verifier(), Redirect.build(port), now() + STATE_TTL_MS)
        live[state] = f
        return f
    }

    /** Removes the entry whether or not it is expired, so a state is never usable twice. */
    @Synchronized
    fun consume(state: String?): Flow? {
        if (state == null) return null
        val f = live.remove(state) ?: return null
        return if (now() <= f.expiresAtMs) f else null
    }

    @Synchronized
    fun pending(): Int = live.size
}

sealed class Callback {
    /** Valid callback: exchange `code` with `flow.verifier` and `flow.redirect`. */
    data class Accepted(val code: String, val flow: Flow) : Callback()

    /** Ignored: the response must not reveal why, and an unrelated pending flow stays untouched. */
    object Ignored : Callback()
}

object CallbackHandler {
    /**
     * `path` and `query` come from the request line; `remoteLoopback` is whether the peer address is loopback.
     * An unknown, expired, replayed or missing `state` is ignored. An `error` response or a missing `code` consumes
     * the state (the flow is over) but yields no code. A callback on another path or from a non-loopback peer is ignored.
     */
    fun handle(store: StateStore, path: String, query: String?, remoteLoopback: Boolean): Callback {
        if (!remoteLoopback || path != CALLBACK_PATH) return Callback.Ignored
        val p = parse(query)
        val flow = store.consume(p["state"]) ?: return Callback.Ignored
        if (p.containsKey("error")) return Callback.Ignored
        val code = p["code"]?.takeIf { it.isNotEmpty() && it.length <= 2048 } ?: return Callback.Ignored
        return Callback.Accepted(code, flow)
    }

    fun parse(q: String?): Map<String, String> {
        val m = HashMap<String, String>()
        if (q.isNullOrEmpty() || q.length > 8192) return m
        for (kv in q.split('&')) {
            val i = kv.indexOf('=')
            val k = URLDecoder.decode(if (i < 0) kv else kv.substring(0, i), Charsets.UTF_8)
            val v = if (i < 0) "" else URLDecoder.decode(kv.substring(i + 1), Charsets.UTF_8)
            if (!m.containsKey(k)) m[k] = v // first value wins; duplicates cannot override
        }
        return m
    }
}

/** Where the redirect port comes from. `builtIn` is `BuiltInServerManager.getInstance().port` in the plugin. */
class LoopbackListener private constructor(val port: Int, private val socket: ServerSocket?) : AutoCloseable {
    val ownsSocket: Boolean get() = socket != null
    override fun close() { socket?.close() }

    companion object {
        /**
         * Prefers the live built-in server port (read at call time, because it moves and differs under Remote Dev).
         * With none, opens an own `ServerSocket` on 127.0.0.1 with an OS-chosen port; never a wildcard bind.
         */
        fun open(builtIn: () -> Int?): LoopbackListener {
            val p = try { builtIn() } catch (e: Exception) { null }
            if (p != null && p in 1..65535) return LoopbackListener(p, null)
            val s = ServerSocket(0, 1, InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1)))
            return LoopbackListener(s.localPort, s)
        }
    }
}
