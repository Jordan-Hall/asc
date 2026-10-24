package asc.asc.bridge

import asc.v1.Bridge.BridgeFrame
import asc.v1.Bridge.Call
import com.google.protobuf.ByteString
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.consumeAsFlow

/** gRPC status code numbers used by the bridge. */
object Code {
    const val OK = 0
    const val CANCELLED = 1
    const val UNAVAILABLE = 14
}

class BridgeException(
    val code: Int,
    message: String,
    val errorDetail: ByteArray = ByteArray(0),
    val bridgeGone: Boolean = false,
) : Exception(message)

/**
 * One framed conversation with an `asc api --stdio` process. It takes the child's stdout and stdin, so tests
 * drive it with pipes. Replies are raw message bytes; callers parse them with the generated protobuf-java
 * classes. Nothing here logs request or reply bytes.
 */
class BridgeClient(private val input: InputStream, private val output: OutputStream) : AutoCloseable {
    private sealed interface Pending
    private class Unary(val done: CompletableDeferred<ByteArray>) : Pending {
        @Volatile var reply: ByteArray? = null
    }
    private class Stream(val ch: Channel<ByteArray>) : Pending

    private val ids = AtomicInteger(0)
    private val pending = ConcurrentHashMap<Int, Pending>()
    private val writeLock = Any()
    @Volatile private var closed = false
    private val reader = Thread({ readLoop() }, "asc-bridge-reader").apply { isDaemon = true }

    fun start() = reader.start()

    private fun send(frame: BridgeFrame) {
        synchronized(writeLock) {
            try {
                writeFrame(output, frame)
            } catch (_: java.io.IOException) {
                throw BridgeException(Code.UNAVAILABLE, "the bridge is not running", bridgeGone = true)
            }
        }
    }

    private fun cancelFrame(id: Int) = BridgeFrame.newBuilder().setId(id).setCancel(true).build()

    private fun callFrame(id: Int, method: String, request: ByteArray, root: String, stream: Boolean): BridgeFrame =
        BridgeFrame.newBuilder().setId(id).setCall(
            Call.newBuilder().setMethod(method).setRequest(ByteString.copyFrom(request)).setRoot(root).setServerStream(stream),
        ).build()

    suspend fun unary(method: String, request: ByteArray, root: String = ""): ByteArray {
        if (closed) throw BridgeException(Code.CANCELLED, "bridge is closed")
        val id = ids.incrementAndGet()
        val u = Unary(CompletableDeferred())
        pending[id] = u
        try {
            send(callFrame(id, method, request, root, false))
            return u.done.await()
        } catch (e: kotlinx.coroutines.CancellationException) {
            pending.remove(id)
            runCatching { send(cancelFrame(id)) }
            throw e
        } catch (e: BridgeException) {
            pending.remove(id)
            throw e
        }
    }

    /** A server stream. The call starts now; closing the collector early sends `cancel`. */
    fun stream(method: String, request: ByteArray, root: String = ""): Flow<ByteArray> {
        if (closed) throw BridgeException(Code.CANCELLED, "bridge is closed")
        val id = ids.incrementAndGet()
        val ch = Channel<ByteArray>(Channel.UNLIMITED)
        pending[id] = Stream(ch)
        try {
            send(callFrame(id, method, request, root, true))
        } catch (e: BridgeException) {
            pending.remove(id)
            throw e
        }
        ch.invokeOnClose { cause ->
            if (cause is kotlinx.coroutines.CancellationException && pending.remove(id) != null) {
                runCatching { send(cancelFrame(id)) }
            }
        }
        return ch.consumeAsFlow()
    }

    private fun readLoop() {
        try {
            while (true) {
                val frame = readFrame(input) ?: break
                dispatch(frame)
            }
        } catch (_: Exception) {
            // a bad frame or a broken pipe both end the conversation
        } finally {
            failAll(BridgeException(Code.UNAVAILABLE, "the bridge exited", bridgeGone = true))
        }
    }

    private fun dispatch(frame: BridgeFrame) {
        val id = frame.id
        when (frame.fCase) {
            BridgeFrame.FCase.REPLY -> when (val p = pending[id]) {
                is Unary -> p.reply = frame.reply.toByteArray()
                is Stream -> p.ch.trySend(frame.reply.toByteArray())
                null -> {}
            }
            BridgeFrame.FCase.END -> {
                val p = pending.remove(id) ?: return
                val end = frame.end
                val err = if (end.code == Code.OK) null else BridgeException(end.code, end.message, end.errorDetail.toByteArray())
                when (p) {
                    is Unary -> if (err != null) p.done.completeExceptionally(err) else p.done.complete(p.reply ?: ByteArray(0))
                    is Stream -> p.ch.close(err)
                }
            }
            else -> {}
        }
    }

    private fun failAll(e: BridgeException) {
        closed = true
        val snapshot = pending.values.toList()
        pending.clear()
        for (p in snapshot) when (p) {
            is Unary -> p.done.completeExceptionally(e)
            is Stream -> p.ch.close(e)
        }
    }

    val isClosed: Boolean get() = closed

    override fun close() {
        runCatching { output.close() }
        runCatching { input.close() }
        failAll(BridgeException(Code.CANCELLED, "bridge is closed"))
    }
}
