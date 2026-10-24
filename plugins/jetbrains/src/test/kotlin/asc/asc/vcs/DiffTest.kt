package asc.asc.vcs

import asc.v1.Repo.LogEntry
import asc.v1.Repo.LogResponse
import asc.v1.Repo.ReadFileResponse
import asc.v1.Repo.Signature
import asc.v1.Workspace.WsLogRequest
import asc.v1.Workspace.WsReadFileRequest
import asc.v1.Workspace.WsStatusResponse
import com.google.protobuf.ByteString
import asc.asc.bridge.BridgeException
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class DiffTest {
    private class Fake(
        val onStream: (String, ByteArray) -> Flow<ByteArray> = { _, _ -> flowOf() },
        val onUnary: (String, ByteArray) -> ByteArray = { _, _ -> ByteArray(0) },
    ) : AscCalls {
        val seen = mutableListOf<String>()

        override suspend fun unary(method: String, request: ByteArray, root: String): ByteArray {
            seen += method
            return onUnary(method, request)
        }

        override suspend fun stream(method: String, request: ByteArray, root: String): Flow<ByteArray> {
            seen += method
            return onStream(method, request)
        }
    }

    private fun chunk(s: String) =
        ReadFileResponse.newBuilder().setData(ByteString.copyFromUtf8(s)).build().toByteArray()

    @Test fun readJoinsChunksAndAsksForTheRevPath() = runBlocking {
        var req: WsReadFileRequest? = null
        val fake = Fake(onStream = { _, r ->
            req = WsReadFileRequest.parseFrom(r)
            flowOf(chunk("ab"), chunk("cd"))
        })
        val got = AscContent.read(fake, "/r", "ws-1", "abc123", "src/a.txt")
        assertArrayEquals("abcd".toByteArray(), got)
        assertEquals("ws-1", req!!.wsId)
        assertEquals("abc123", req!!.revPath.rev)
        assertEquals("src/a.txt", req!!.revPath.path.toStringUtf8())
        assertEquals(listOf("asc.v1.Workspace/ReadFile"), fake.seen)
    }

    @Test fun readNormalisesBackslashes() = runBlocking {
        var req: WsReadFileRequest? = null
        val fake = Fake(onStream = { _, r ->
            req = WsReadFileRequest.parseFrom(r)
            flowOf(chunk(""))
        })
        AscContent.read(fake, "/r", "w", "", "a\\b.txt")
        assertEquals("a/b.txt", req!!.revPath.path.toStringUtf8())
    }

    @Test fun missingFileReadsAsNull() = runBlocking {
        val fake = Fake(onStream = { _, _ -> throw BridgeException(5, "no such path") })
        assertNull(AscContent.read(fake, "/r", "w", "abc", "new.txt"))
    }

    @Test fun otherErrorsPropagate() {
        val fake = Fake(onStream = { _, _ -> throw BridgeException(14, "gone", bridgeGone = true) })
        try {
            runBlocking { AscContent.read(fake, "/r", "w", "abc", "a") }
            fail("expected a BridgeException")
        } catch (e: BridgeException) {
            assertEquals(14, e.code)
        }
    }

    @Test fun headIsHexOfStatusHead() = runBlocking {
        val resp = WsStatusResponse.newBuilder().setHead(ByteString.copyFrom(byteArrayOf(0x0a, 0xff.toByte()))).build()
        val fake = Fake(onUnary = { _, _ -> resp.toByteArray() })
        assertEquals(AscRevisionNumber("0aff"), AscContent.head(fake, "/r", "w"))
    }

    @Test fun unbornHeadIsNull() = runBlocking {
        val fake = Fake(onUnary = { _, _ -> WsStatusResponse.getDefaultInstance().toByteArray() })
        assertNull(AscContent.head(fake, "/r", "w"))
    }

    @Test fun lastChangeAsksForOneFollowedEntryAndKeepsTime() = runBlocking {
        var req: WsLogRequest? = null
        val entry = LogEntry.newBuilder().setId(ByteString.copyFrom(byteArrayOf(1, 2)))
            .setCommitter(Signature.newBuilder().setTime(1700000000L)).build()
        val fake = Fake(onStream = { _, r ->
            req = WsLogRequest.parseFrom(r)
            flowOf(LogResponse.newBuilder().addEntries(entry).build().toByteArray())
        })
        val rev = AscContent.lastChange(fake, "/r", "w", "d/f.kt")!!
        assertEquals("0102", rev.id)
        assertEquals(1700000000L, rev.time)
        assertEquals(listOf("d/f.kt"), req!!.query.pathsList)
        assertTrue(req!!.query.follow)
        assertEquals(1, req!!.query.limit)
    }

    @Test fun lastChangeNullWithoutHistory() = runBlocking {
        val fake = Fake(onStream = { _, _ -> flowOf(LogResponse.getDefaultInstance().toByteArray()) })
        assertNull(AscContent.lastChange(fake, "/r", "w", "x"))
    }

    @Test fun revisionOrderUsesTimeThenId() {
        assertTrue(AscRevisionNumber("zz", 1).compareTo(AscRevisionNumber("aa", 2)) < 0)
        assertTrue(AscRevisionNumber("a").compareTo(AscRevisionNumber("b")) < 0)
        assertEquals(AscRevisionNumber("a", 1), AscRevisionNumber("a", 9))
    }

    @Test fun relativePaths() {
        assertEquals("a/b.txt", AscPaths.relative("C:\\w\\r", "C:/w/r/a/b.txt"))
        assertEquals("", AscPaths.relative("/w/r/", "/w/r"))
        assertNull(AscPaths.relative("/w/r", "/w/rx/a"))
        assertNull(AscPaths.relative("/w/r", "/other/a"))
    }
}
