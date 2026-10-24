package asc.asc.bridge

import asc.v1.Bridge.HelloRequest
import asc.v1.Bridge.HelloResponse
import com.intellij.openapi.Disposable
import com.intellij.openapi.components.Service
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow

/**
 * One `asc api --stdio` per IDE process. It starts lazily, restarts with backoff when it dies (callers see
 * `bridgeGone` errors and resubscribe `Workspace.Watch` from their last token), and respawns when the token
 * changes. Child stderr is discarded, never logged: it may name paths and hosts.
 */
@Service(Service.Level.APP)
class AscBridgeService : Disposable {
    @Volatile var ascPath: String = "asc"
    @Volatile private var token: String? = null
    private var process: Process? = null
    private var client: BridgeClient? = null
    private var failures = 0
    private val lock = Any()

    /** Set the PasswordSafe fallback token; the bridge is respawned when it changes. */
    fun setToken(value: String?) {
        synchronized(lock) {
            if (value == token) return
            token = value
            stopLocked()
        }
    }

    private fun clientLocked(): BridgeClient {
        client?.takeIf { !it.isClosed && process?.isAlive == true }?.let { return it }
        stopLocked()
        val pb = ProcessBuilder(BridgeEnv.command(ascPath))
        val env = pb.environment()
        val built = BridgeEnv.build(System.getenv(), token)
        env.clear()
        env.putAll(built)
        pb.redirectError(ProcessBuilder.Redirect.DISCARD)
        val p = try {
            pb.start()
        } catch (_: java.io.IOException) {
            failures++
            throw BridgeException(Code.UNAVAILABLE, "cannot run the bridge", bridgeGone = true)
        }
        process = p
        return BridgeClient(p.inputStream, p.outputStream).also {
            it.start()
            client = it
        }
    }

    private fun stopLocked() {
        client?.close()
        process?.destroy()
        client = null
        process = null
    }

    private suspend fun live(): BridgeClient {
        val wait = synchronized(lock) { if (failures == 0) 0L else minOf(30_000L, 250L shl minOf(failures - 1, 7)) }
        if (wait > 0) delay(wait)
        return synchronized(lock) { clientLocked() }
    }

    suspend fun unary(method: String, request: ByteArray, root: String = ""): ByteArray {
        val c = live()
        return try {
            c.unary(method, request, root).also { synchronized(lock) { failures = 0 } }
        } catch (e: BridgeException) {
            if (e.bridgeGone) {
                synchronized(lock) {
                    failures++
                    if (client === c) stopLocked()
                }
            }
            throw e
        }
    }

    suspend fun stream(method: String, request: ByteArray, root: String = ""): Flow<ByteArray> =
        live().stream(method, request, root)

    suspend fun hello(roots: List<String>): HelloResponse =
        HelloResponse.parseFrom(
            unary("asc.v1.Bridge/Hello", HelloRequest.newBuilder().addAllRoots(roots).build().toByteArray()),
        )

    override fun dispose() {
        synchronized(lock) { stopLocked() }
    }
}
