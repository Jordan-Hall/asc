package asc.asc.auth

/**
 * The host rules of sign-in (SPEC 24.6), the same as the VS Code extension's `auth.ts`: what a person types, the
 * host of a root's remote, and the one host a fallback token may be sent to. Every function fails closed.
 */
object HostRules {
    const val MAX_TOKEN = 16 * 1024
    private const val MAX_HOST = 253 + 6
    private val LABEL = Regex("^[a-z0-9]([a-z0-9-]*[a-z0-9])?$")
    private val PORT = Regex("^[1-9]\d{0,4}$")

    /**
     * A bare `host` or `host:port` from typed text: lowercase, https only, no user information, path, query,
     * fragment or whitespace, and a name rather than an address. Null for anything else.
     */
    fun normalize(input: String): String? {
        var text = input.trim().lowercase()
        if (text.isEmpty() || text.length > MAX_HOST) return null
        if (text.any { it.code <= 0x20 || it.code == 0x7f || it in "\@?#" }) return null
        if (text.contains("://")) {
            if (!text.startsWith("https://")) return null
            text = text.removePrefix("https://").removeSuffix("/")
        }
        if (text.contains('/')) return null
        var port = ""
        val colon = text.lastIndexOf(':')
        if (colon >= 0) {
            port = text.substring(colon + 1)
            text = text.substring(0, colon)
            if (!PORT.matches(port) || port.toInt() > 65535) return null
        }
        val labels = text.split('.')
        if (labels.any { it.isEmpty() || it.length > 63 || !LABEL.matches(it) }) return null
        if (labels.last().all { it.isDigit() }) return null
        return if (port.isEmpty()) text else "$text:$port"
    }

    /** The token host of a root's remote URL; null for ssh, scp-like, local and http remotes and user information. */
    fun ofRemote(remoteUrl: String): String? {
        if (remoteUrl.any { it.code <= 0x20 || it.code == 0x7f || it == '\' }) return null
        if (!remoteUrl.lowercase().startsWith("https://")) return null
        val rest = remoteUrl.substring("https://".length)
        val end = rest.indexOfFirst { it in "/?#" }.let { if (it < 0) rest.length else it }
        val authority = rest.substring(0, end)
        if (authority.isEmpty() || authority.contains('@')) return null
        return normalize(authority.removeSuffix(":443"))
    }

    /** What the token check needs of a `Hello` root. */
    data class Root(val wsId: String, val remoteUrl: String)

    /**
     * The one host every root's remote is on, or null. A fallback token reaches `asc` only for this host, so a
     * project with roots on two hosts, or any root without an https remote, gets none.
     */
    fun commonTokenHost(roots: List<Root>): String? {
        var host: String? = null
        for (r in roots) {
            val h = (if (r.wsId.isEmpty()) null else ofRemote(r.remoteUrl)) ?: return null
            if (host != null && host != h) return null
            host = h
        }
        return host
    }

    /** The hosts to offer in the sign-in prompt: each root's https host, once, in root order. */
    fun candidates(roots: List<Root>): List<String> = roots.mapNotNull { ofRemote(it.remoteUrl) }.distinct()

    /** A pasted token: trimmed, non-empty, bounded, one line. */
    fun cleanToken(raw: String): String? {
        val t = raw.trim()
        if (t.isEmpty() || t.length > MAX_TOKEN || t.any { it.code < 0x20 || it.code == 0x7f }) return null
        return t
    }
}
