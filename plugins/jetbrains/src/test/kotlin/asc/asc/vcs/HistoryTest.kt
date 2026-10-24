package asc.asc.vcs

import asc.v1.Repo.BlameHunk
import asc.v1.Repo.BlameResponse
import asc.v1.Repo.LineHistoryRequest
import asc.v1.Repo.LogEntry
import asc.v1.Repo.LogResponse
import asc.v1.Repo.ReviewState
import asc.v1.Repo.Signature
import asc.v1.Workspace.WsBlameRequest
import asc.v1.Workspace.WsLogRequest
import com.google.protobuf.ByteString
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class HistoryTest {
    private class Fake(val onStream: (String, ByteArray) -> Flow<ByteArray>) : AscCalls {
        val seen = mutableListOf<String>()

        override suspend fun unary(method: String, request: ByteArray, root: String): ByteArray = ByteArray(0)

        override suspend fun stream(method: String, request: ByteArray, root: String): Flow<ByteArray> {
            seen += method
            return onStream(method, request)
        }
    }

    private fun id(vararg b: Int) = ByteString.copyFrom(ByteArray(b.size) { b[it].toByte() })

    private fun commit(i: Int, name: String, time: Long, msg: String) = LogEntry.newBuilder().setId(id(i))
        .setAuthor(Signature.newBuilder().setName(ByteString.copyFromUtf8(name)).setEmail(ByteString.copyFromUtf8("$name@x")))
        .setCommitter(Signature.newBuilder().setTime(time))
        .setMessage(ByteString.copyFromUtf8(msg)).build()

    @Test fun blameMapsHunksToOneBasedLinesAndLeavesUncommittedNull() = runBlocking {
        var req: WsBlameRequest? = null
        val resp = BlameResponse.newBuilder()
            .addCommits(commit(1, "ann", 100, "first\n\nbody"))
            .addReviews(ReviewState.newBuilder().setNumber(7))
            .addHunks(BlameHunk.newBuilder().setOrigin(id(1)).setStart(1).setLen(2))
            .addHunks(BlameHunk.newBuilder().setStart(3).setLen(1)) // empty origin: uncommitted
            .build()
        val fake = Fake { _, r ->
            req = WsBlameRequest.parseFrom(r)
            flowOf(resp.toByteArray())
        }
        val lines = AscBlame.load(fake, "/r", "ws", "src\a.kt", "buf".toByteArray(), 4)
        assertEquals(4, lines.size)
        assertEquals("01", lines[0]!!.revision.id)
        assertEquals("01", lines[1]!!.revision.id)
        assertNull(lines[2])
        assertNull(lines[3])
        assertEquals("first", lines[0]!!.subject)
        assertEquals("#7", lines[0]!!.review)
        assertEquals("src/a.kt", req!!.path.toStringUtf8())
        assertEquals("", req!!.rev)
        assertEquals("buf", req!!.buffer.toStringUtf8())
        assertEquals(listOf("asc.v1.Workspace/Blame"), fake.seen)
    }

    @Test fun blameWithoutBufferSendsNone() = runBlocking {
        var req: WsBlameRequest? = null
        val fake = Fake { _, r ->
            req = WsBlameRequest.parseFrom(r)
            flowOf()
        }
        AscBlame.load(fake, "/r", "ws", "a", null, 0)
        assertTrue(req!!.buffer.isEmpty)
    }

    @Test fun blameIgnoresHunksPastTheBuffer() = runBlocking {
        val resp = BlameResponse.newBuilder().addCommits(commit(1, "a", 1, "m"))
            .addHunks(BlameHunk.newBuilder().setOrigin(id(1)).setStart(2).setLen(5)).build()
        val lines = AscBlame.load(Fake { _, _ -> flowOf(resp.toByteArray()) }, "/r", "w", "a", null, 3)
        assertNull(lines[0])
        assertEquals("01", lines[1]!!.revision.id)
        assertEquals("01", lines[2]!!.revision.id)
    }

    @Test fun lineCountMatchesEditors() {
        assertEquals(0, AscBlame.lineCount(""))
        assertEquals(2, AscBlame.lineCount("a\nb"))
        assertEquals(2, AscBlame.lineCount("a\nb\n"))
    }

    @Test fun tooltipCarriesRevisionAuthorChangeAndReview() {
        val info = AscLineInfo(AscRevisionNumber("abcdef0123456789"), "Ann", "a@x", 1, "fix", "c0ffee", "#7")
        val t = AscBlame.tooltip(info)!!
        assertTrue(t.contains("abcdef012345 Ann <a@x>"))
        assertTrue(t.contains("fix"))
        assertTrue(t.contains("change c0ffee"))
        assertTrue(t.contains("review #7"))
        assertNull(AscBlame.tooltip(null))
    }

    @Test fun fileHistoryAsksForPagesOf50WithFollow() = runBlocking {
        var req: WsLogRequest? = null
        val fake = Fake { _, r ->
            req = WsLogRequest.parseFrom(r)
            flowOf(LogResponse.newBuilder().addEntries(commit(2, "b", 5, "m")).setNextPageToken("p2").build().toByteArray())
        }
        val page = AscHistory.page(fake, "/r", "ws", "d\f.kt", "p1")
        assertEquals(50, req!!.query.limit)
        assertTrue(req!!.query.follow)
        assertEquals(listOf("d/f.kt"), req!!.query.pathsList)
        assertEquals("p1", req!!.query.pageToken)
        assertEquals("p2", page.nextToken)
        assertEquals(1, page.entries.size)
    }

    @Test fun lastPageHasNoToken() = runBlocking {
        val fake = Fake { _, _ -> flowOf(LogResponse.getDefaultInstance().toByteArray()) }
        assertEquals("", AscHistory.page(fake, "/r", "w", "a", "").nextToken)
    }

    @Test fun lineHistoryResumesTruncatedScans() = runBlocking {
        val reqs = mutableListOf<LineHistoryRequest>()
        val fake = Fake { _, r ->
            val q = LineHistoryRequest.parseFrom(r)
            reqs += q
            val first = q.resumeToken.isEmpty()
            flowOf(
                LogResponse.newBuilder().addEntries(commit(if (first) 1 else 2, "a", 1, "m"))
                    .setTruncated(first).setResumeToken(if (first) "tok" else "").build().toByteArray(),
            )
        }
        val page = AscHistory.lineHistory(fake, "/r", "a\b.kt", "", 3, 9)
        assertEquals(2, page.entries.size)
        assertFalse(page.truncated)
        assertEquals("tok", reqs[1].resumeToken)
        assertEquals(3, reqs[0].startLine)
        assertEquals(9, reqs[0].endLine)
        assertEquals("a/b.kt", reqs[0].path.toStringUtf8())
        assertEquals(listOf("asc.v1.Repo/LineHistory", "asc.v1.Repo/LineHistory"), fake.seen)
    }

    @Test fun lineHistoryReportsStillTruncatedAfterTheCallCap() = runBlocking {
        val fake = Fake { _, _ ->
            flowOf(LogResponse.newBuilder().setTruncated(true).setResumeToken("t").build().toByteArray())
        }
        val page = AscHistory.lineHistory(fake, "/r", "a", "", 1, 2, maxCalls = 3)
        assertTrue(page.truncated)
        assertEquals(3, fake.seen.size)
    }
}
