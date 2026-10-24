package asc.asc.ui

import com.google.gson.JsonElement
import com.google.gson.JsonNull
import com.google.gson.JsonParser
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

// The Forgejo adapter of the host API seam: the relay (AscRelay.kt) checks a `{provider, method, path}` request
// against the allowlist and hands it to the adapter registered for its provider id. Nothing outside this file
// names Forgejo; its REST base path and token handling live here.

/** The host API adapters this build ships, by provider id. */
fun hostApiAdapters(
    forges: () -> Map<String, String>,
    token: suspend (host: String) -> String?,
): Map<String, suspend (HostApiCall) -> HostApiResult> =
    mapOf<String, suspend (HostApiCall) -> HostApiResult>("forgejo" to AscForgejoClient(forges, token)::call)

/**
 * Runs a Forgejo request that the relay already matched against the allowlist (SPEC 24.4.5.1, 24.6). The token
 * comes from `Bridge.Token` through [token], is attached here as a bearer header and is never logged, never
 * put in a URL and never returned to the page. https only, no redirects (a redirect could carry the token to
 * another origin), the host must be the forge host of an opened root, and the reply is capped.
 */
class AscForgejoClient(
    /** `owner/repo` to forge URL of the opened roots (credentials already removed by the bridge). */
    private val forges: () -> Map<String, String>,
    private val token: suspend (host: String) -> String?,
    private val send: (HttpRequest) -> HttpResponse<java.io.InputStream> = { client.send(it, HttpResponse.BodyHandlers.ofInputStream()) },
) {
    companion object {
        const val MAX_BODY = 8 * 1024 * 1024
        private val client: HttpClient by lazy {
            HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NEVER).connectTimeout(Duration.ofSeconds(10)).build()
        }

        /** The request URL, or null when the forge URL is not an https origin without credentials. */
        fun url(forge: String, path: String): URI? {
            val base = try {
                URI(forge.trimEnd('/'))
            } catch (_: Exception) {
                return null
            }
            if (base.scheme != "https" || base.host == null || base.userInfo != null || base.rawQuery != null || base.rawFragment != null) {
                return null
            }
            val full = try {
                URI(base.toString() + "/api/v1" + path)
            } catch (_: Exception) {
                return null
            }
            return if (full.host == base.host && full.port == base.port && full.scheme == "https") full else null
        }
    }

    suspend fun call(c: HostApiCall): HostApiResult {
        val map = forges()
        val forge = (c.repo?.let { map[it] } ?: map.values.firstOrNull()) ?: throw IllegalStateException("no forge")
        val uri = url(forge, c.path) ?: throw IllegalStateException("forge is not https")
        val bearer = token(uri.host) ?: return HostApiResult(401, JsonNull.INSTANCE)
        return withContext(Dispatchers.IO) {
            val b = HttpRequest.newBuilder(uri).timeout(Duration.ofSeconds(30))
                .header("accept", "application/json").header("authorization", "Bearer $bearer")
            val body = c.body
            val req = if (body == null) {
                b.method(c.method, HttpRequest.BodyPublishers.noBody())
            } else {
                b.header("content-type", "application/json").method(c.method, HttpRequest.BodyPublishers.ofString(body.toString()))
            }.build()
            val res = send(req)
            res.body().use { input ->
                val buf = java.io.ByteArrayOutputStream()
                val chunk = ByteArray(8192)
                while (true) {
                    val n = input.read(chunk)
                    if (n < 0) break
                    if (buf.size() + n > MAX_BODY) throw IllegalStateException("reply too large")
                    buf.write(chunk, 0, n)
                }
                val text = String(buf.toByteArray(), Charsets.UTF_8)
                val el: JsonElement = if (text.isBlank()) {
                    JsonNull.INSTANCE
                } else {
                    try {
                        JsonParser.parseString(text)
                    } catch (_: Exception) {
                        com.google.gson.JsonPrimitive(text)
                    }
                }
                HostApiResult(res.statusCode(), el)
            }
        }
    }
}
