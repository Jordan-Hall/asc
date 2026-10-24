package asc.asc.ui

import java.net.InetAddress
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration
import java.util.Base64

/**
 * Host-side avatar fetch (SPEC 24.4.5, 24.5.2). The page never loads an avatar itself. https only, no cookies
 * and no token, public addresses only, every redirect re-checked, at most 3 redirects, png/jpeg/webp/gif
 * (never SVG), 256 KB, 5 s, LRU of 200 entries and 2 MiB; the result is a `data:` URI.
 * Known limit: the name is resolved and checked, then the client resolves it again (a rebinding window); the
 * response is still only an image of at most 256 KB and nothing is sent but a GET without credentials.
 */
class AscAvatar(
    private val resolve: (String) -> List<InetAddress> = { InetAddress.getAllByName(it).toList() },
    private val fetch: (URI, Duration) -> AvatarResponse = { uri, timeout -> httpFetch(uri, timeout) },
    private val now: () -> Long = System::nanoTime,
) {
    class AvatarResponse(val status: Int, val headers: Map<String, String>, val body: java.io.InputStream)

    companion object {
        const val MAX_BYTES = 256 * 1024
        const val MAX_REDIRECTS = 3
        const val TIMEOUT_MS = 5000L
        const val CACHE_ENTRIES = 200
        const val CACHE_BYTES = 2 * 1024 * 1024
        private val TYPES = setOf("image/png", "image/jpeg", "image/webp", "image/gif")
        private val DOTTED = Regex("^\\d{1,3}(\\.\\d{1,3}){3}$")

        private val client: HttpClient by lazy {
            HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NEVER).connectTimeout(Duration.ofMillis(TIMEOUT_MS)).build()
        }

        private fun httpFetch(uri: URI, timeout: Duration): AvatarResponse {
            val req = HttpRequest.newBuilder(uri).GET().timeout(timeout)
                .header("accept", "image/png,image/jpeg,image/webp,image/gif").build()
            val res = client.send(req, HttpResponse.BodyHandlers.ofInputStream())
            return AvatarResponse(res.statusCode(), res.headers().map().mapValues { it.value.firstOrNull() ?: "" }.mapKeys { it.key.lowercase() }, res.body())
        }

        /** True only for globally routable addresses. */
        fun isPublic(a: InetAddress): Boolean {
            if (a.isAnyLocalAddress || a.isLoopbackAddress || a.isLinkLocalAddress || a.isSiteLocalAddress || a.isMulticastAddress) return false
            val b = a.address.map { it.toInt() and 0xff }
            if (b.size == 4) {
                val (x, y, z) = Triple(b[0], b[1], b[2])
                if (x == 0 || x >= 224) return false
                if (x == 100 && y in 64..127) return false
                if (x == 192 && y == 0 && (z == 0 || z == 2)) return false
                if (x == 198 && (y == 18 || y == 19)) return false
                if (x == 198 && y == 51 && z == 100) return false
                if (x == 203 && y == 0 && z == 113) return false
                return true
            }
            if (b.size == 16) {
                val first = (b[0] shl 8) or b[1]
                if ((first and 0xfe00) == 0xfc00) return false // fc00::/7
                if (first == 0x2001 && b[2] == 0x0d && b[3] == 0xb8) return false // 2001:db8::/32
                if (first == 0x0064 && b[2] == 0xff && b[3] == 0x9b) return false // 64:ff9b::/32
                if (first == 0x0100 && b.subList(2, 8).all { it == 0 }) return false // 100::/64
                if (b.subList(0, 12).all { it == 0 }) return false // IPv4-compatible and ::
                return true
            }
            return false
        }
    }

    private val cache = object : LinkedHashMap<String, String>(16, 0.75f, true) {}
    private var bytes = 0

    val size: Int get() = synchronized(cache) { cache.size }
    val cachedBytes: Int get() = synchronized(cache) { bytes }

    /** A `data:` URI, or null when the URL is refused or the fetch fails. Never throws. */
    fun get(raw: String): String? {
        synchronized(cache) { cache[raw]?.let { return it } }
        val uri = try {
            load(raw)
        } catch (_: Exception) {
            null
        }
        if (uri != null) put(raw, uri)
        return uri
    }

    private fun put(key: String, uri: String) {
        if (uri.length > CACHE_BYTES) return
        synchronized(cache) {
            cache.remove(key)?.let { bytes -= it.length }
            cache[key] = uri
            bytes += uri.length
            val it = cache.entries.iterator()
            while ((cache.size > CACHE_ENTRIES || bytes > CACHE_BYTES) && it.hasNext()) {
                val e = it.next()
                bytes -= e.value.length
                it.remove()
            }
        }
    }

    private fun allowed(u: URI): Boolean {
        if (u.scheme != "https" || u.userInfo != null) return false
        val host = (u.host ?: return false).removePrefix("[").removeSuffix("]")
        val addrs = if (host.contains(':') || DOTTED.matches(host)) listOf(InetAddress.getByName(host)) else resolve(host)
        return addrs.isNotEmpty() && addrs.all { isPublic(it) }
    }

    private fun load(raw: String): String? {
        val deadline = now() + TIMEOUT_MS * 1_000_000
        var uri = URI(raw)
        for (hop in 0..MAX_REDIRECTS) {
            if (!allowed(uri)) return null
            val left = (deadline - now()) / 1_000_000
            if (left <= 0) return null
            val res = fetch(uri, Duration.ofMillis(left))
            res.body.use { body ->
                if (res.status in 300..399) {
                    val loc = res.headers["location"] ?: return null
                    uri = uri.resolve(loc)
                    return@use
                }
                if (res.status !in 200..299) return null
                val type = (res.headers["content-type"] ?: "").substringBefore(';').trim().lowercase()
                if (type !in TYPES) return null
                if ((res.headers["content-length"]?.toLongOrNull() ?: 0L) > MAX_BYTES) return null
                val data = readCapped(body, MAX_BYTES) ?: return null
                return "data:$type;base64,${Base64.getEncoder().encodeToString(data)}"
            }
        }
        return null
    }

    private fun readCapped(input: java.io.InputStream, cap: Int): ByteArray? {
        val out = java.io.ByteArrayOutputStream()
        val buf = ByteArray(8192)
        while (true) {
            val n = input.read(buf)
            if (n < 0) break
            if (out.size() + n > cap) return null
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }
}
