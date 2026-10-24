package asc.asc.bridge

import asc.v1.Bridge.BridgeFrame
import asc.v1.Bridge.End
import com.google.protobuf.ByteString
import java.io.ByteArrayInputStream
import java.io.PipedInputStream
import java.io.PipedOutputStream
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class BridgeTest {
    private fun reply(id: Int, b: String) =
        BridgeFrame.newBuilder().setId(id).setReply(ByteString.copyFromUtf8(b)).build()

    private fun end(id: Int, code: Int = 0, msg: String = "") =
        BridgeFrame.newBuilder().setId(id).setEnd(End.newBuilder().setCode(code).setMessage(msg)).build()

    @Test fun frameRoundTripIsLittleEndianLengthPrefixed() {
        val f = reply(7, "hi")
        val bytes = encodeFrame(f)
        assertEquals(f.serializedSize, bytes[0].toInt() and 0xff)
        assertEquals(0, bytes[1].toInt())
        assertEquals(0, bytes[3].toInt())
        assertEquals(f, readFrame(ByteArrayInputStream(bytes)))
    }

    @Test fun cleanEndOfStreamIsNull() = assertNull(readFrame(ByteArrayInputStream(ByteArray(0))))

    @Test fun oversizeLengthIsRefused() {
        val bad = byteArrayOf(0, 0, 0, 0x7f)
        try {
            readFrame(ByteArrayInputStream(bad))
            fail()
        } catch (_: FrameException) {
        }
    }

    @Test fun truncatedFrameFails() {
        val bytes = encodeFrame(reply(1, "abcdef")).copyOf(7)
        try {
            readFrame(ByteArrayInputStream(bytes))
            fail()
        } catch (_: java.io.EOFException) {
        }
    }

    /** A fake bridge: reads calls from the client and answers per [script]. */
    private fun harness(script: (BridgeFrame, (BridgeFrame) -> Unit) -> Unit): BridgeClient {
        val toBridge = PipedOutputStream()
        val bridgeIn = PipedInputStream(toBridge, 1 shl 16)
        val fromBridge = PipedOutputStream()
        val clientIn = PipedInputStream(fromBridge, 1 shl 16)
        Thread {
            try {
                while (true) {
                    val f = readFrame(bridgeIn) ?: break
                    script(f) { writeFrame(fromBridge, it) }
                }
            } finally {
                fromBridge.close()
            }
        }.apply { isDaemon = true }.start()
        return BridgeClient(clientIn, toBridge).also { it.start() }
    }

    @Test fun unaryReturnsReply() = runBlocking {
        val c = harness { f, out ->
            if (f.hasCall()) {
                out(reply(f.id, f.call.method))
                out(end(f.id))
            }
        }
        assertArrayEquals("m/x".toByteArray(), c.unary("m/x", ByteArray(0)))
        c.close()
    }

    @Test fun unaryErrorCarriesCode() = runBlocking {
        val c = harness { f, out -> if (f.hasCall()) out(end(f.id, 5, "nope")) }
        try {
            c.unary("m", ByteArray(0))
            fail()
        } catch (e: BridgeException) {
            assertEquals(5, e.code)
            assertEquals("nope", e.message)
            assertFalse(e.bridgeGone)
        }
        c.close()
    }

    @Test fun streamYieldsAllRepliesThenEnds() = runBlocking {
        val c = harness { f, out ->
            if (f.hasCall()) {
                out(reply(f.id, "a"))
                out(reply(f.id, "b"))
                out(end(f.id))
            }
        }
        val got = c.stream("m", ByteArray(0)).toList().map { String(it) }
        assertEquals(listOf("a", "b"), got)
        c.close()
    }

    @Test fun bridgeExitFailsPendingCallsAsGone() = runBlocking {
        val c = harness { f, _ -> if (f.hasCall()) throw RuntimeException("die") }
        try {
            c.unary("m", ByteArray(0))
            fail()
        } catch (e: BridgeException) {
            assertTrue(e.bridgeGone)
            assertEquals(Code.UNAVAILABLE, e.code)
        }
    }

    @Test fun environmentNeverPromptsAndTokenStaysOutOfArgv() {
        val env = BridgeEnv.build(mapOf("GIT_ASKPASS" to "x", "PATH" to "p", "ASC_TOKEN" to "old"), "new")
        assertFalse(env.containsKey("GIT_ASKPASS"))
        assertEquals("0", env["GIT_TERMINAL_PROMPT"])
        assertEquals("never", env["GCM_INTERACTIVE"])
        assertEquals("new", env["ASC_TOKEN"])
        assertEquals("p", env["PATH"])
        assertEquals(listOf("asc", "api", "--stdio"), BridgeEnv.command("asc"))
        assertEquals("old", BridgeEnv.build(mapOf("ASC_TOKEN" to "old"), null)["ASC_TOKEN"])
    }
}
