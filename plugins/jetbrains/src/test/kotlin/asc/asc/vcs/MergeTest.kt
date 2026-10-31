package asc.asc.vcs

import asc.v1.RepoOuterClass.ConflictKind
import asc.v1.RepoOuterClass.ReadFileResponse
import asc.v1.WorkspaceOuterClass.WsConflict
import asc.v1.WorkspaceOuterClass.WsConflictsResponse
import asc.v1.WorkspaceOuterClass.WsReadFileRequest
import asc.v1.WorkspaceOuterClass.WsStageRequest
import com.google.protobuf.ByteString
import com.intellij.openapi.vcs.VcsException
import asc.asc.bridge.BridgeException
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class MergeTest {
    private class Fake(
        val conflicts: List<WsConflict>,
        val stages: Map<Int, String>,
    ) : AscCalls {
        val seen = mutableListOf<String>()
        var staged: WsStageRequest? = null

        override suspend fun unary(method: String, request: ByteArray, root: String): ByteArray {
            seen += method
            return when (method) {
                "asc.v1.Workspace/Conflicts" ->
                    WsConflictsResponse.newBuilder().addAllConflicts(conflicts).build().toByteArray()
                "asc.v1.Workspace/Stage" -> {
                    staged = WsStageRequest.parseFrom(request)
                    ByteArray(0)
                }
                else -> ByteArray(0)
            }
        }

        override suspend fun stream(method: String, request: ByteArray, root: String): Flow<ByteArray> {
            seen += method
            val n = WsReadFileRequest.parseFrom(request).stage.stage
            val s = stages[n] ?: throw BridgeException(5, "no stage $n")
            return flowOf(ReadFileResponse.newBuilder().setData(ByteString.copyFromUtf8(s)).build().toByteArray())
        }
    }

    private fun conflict(path: String, kind: ConflictKind = ConflictKind.CONFLICT_KIND_CONTENT) =
        WsConflict.newBuilder().setPath(ByteString.copyFromUtf8(path)).setKind(kind).build()

    @Test fun loadsBaseOursAndTheirsFromStages() = runBlocking {
        val fake = Fake(listOf(conflict("a/b.txt")), mapOf(1 to "base", 2 to "ours", 3 to "theirs"))
        val s = AscMerge.load(fake, "/r", "ws", "a\\b.txt")
        assertArrayEquals("base".toByteArray(), s.base)
        assertArrayEquals("ours".toByteArray(), s.ours)
        assertArrayEquals("theirs".toByteArray(), s.theirs)
        assertFalse(s.binary)
        val md = AscMerge.toMergeData(s)
        assertArrayEquals("ours".toByteArray(), md.CURRENT)
        assertArrayEquals("theirs".toByteArray(), md.LAST)
        assertArrayEquals("base".toByteArray(), md.ORIGINAL)
    }

    @Test fun addAddHasAnEmptyBase() = runBlocking {
        val fake = Fake(listOf(conflict("n.txt", ConflictKind.CONFLICT_KIND_ADD_ADD)), mapOf(2 to "x", 3 to "y"))
        val s = AscMerge.load(fake, "/r", "ws", "n.txt")
        assertEquals(0, s.base.size)
    }

    @Test fun binaryKindIsReported() = runBlocking {
        val fake = Fake(listOf(conflict("i.png", ConflictKind.CONFLICT_KIND_BINARY)), mapOf(1 to "a", 2 to "b", 3 to "c"))
        assertTrue(AscMerge.load(fake, "/r", "ws", "i.png").binary)
    }

    @Test fun nonConflictedPathFails() {
        val fake = Fake(listOf(conflict("a")), emptyMap())
        try {
            runBlocking { AscMerge.load(fake, "/r", "ws", "other") }
            fail("expected a VcsException")
        } catch (e: VcsException) {
            assertTrue(e.message!!.contains("not in conflict"))
        }
    }

    @Test fun resolvedStagesTheFile() = runBlocking {
        val fake = Fake(emptyList(), emptyMap())
        AscMerge.resolved(fake, "/r", "ws", "src\\m.txt")
        assertEquals(listOf("asc.v1.Workspace/Stage"), fake.seen)
        assertEquals("src/m.txt", fake.staged!!.getAdd(0).toStringUtf8())
        assertEquals("ws", fake.staged!!.wsId)
    }
}
