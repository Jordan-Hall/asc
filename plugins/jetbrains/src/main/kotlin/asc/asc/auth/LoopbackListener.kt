package asc.asc.auth

import com.sun.net.httpserver.HttpServer
import java.net.InetAddress
import java.net.InetSocketAddress

/** A source of redirect callbacks on `127.0.0.1`. */
interface CallbackListener : AutoCloseable {
    val port: Int

    /** Called once per request on the callback path with the raw query string. */
    fun onCallback(handler: (query: String?) -> Unit)
}

/**
 * The plugin's own listener, bound to the IPv4 loopback address only (never a wildcard) on an ephemeral port.
 * It answers every request with a fixed page and does nothing else with the request.
 * It is the only callback path. The IDE built-in server (`BuiltInServerManager.getInstance().port` through an
 * `HttpRequestHandler`) needs a live IDE to verify, as spikes/loopback/RESULTS.md records, so it is not used.
 */
class LoopbackListener : CallbackListener {
    private val server: HttpServer =
        HttpServer.create(InetSocketAddress(InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1)), 0), 0)
    @Volatile private var handler: ((String?) -> Unit)? = null
    override val port: Int get() = server.address.port

    init {
        server.createContext(AuthConstants.CALLBACK_PATH) { ex ->
            val body = "<!doctype html><title>asc</title><p>Signed in. You can close this tab.</p>".toByteArray()
            val query = ex.requestURI.rawQuery
            ex.responseHeaders.add("Content-Type", "text/html; charset=utf-8")
            ex.responseHeaders.add("Cache-Control", "no-store")
            ex.responseHeaders.add("Referrer-Policy", "no-referrer")
            ex.sendResponseHeaders(200, body.size.toLong())
            ex.responseBody.use { it.write(body) }
            handler?.invoke(query)
        }
        server.start()
    }

    override fun onCallback(handler: (String?) -> Unit) {
        this.handler = handler
    }

    override fun close() = server.stop(0)
}
