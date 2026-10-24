package asc.asc.vcs

import asc.v1.WorkspaceOuterClass.WatchEvent
import com.google.protobuf.ByteString
import java.nio.file.Paths
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ChangesTest {
    private fun ev(kind: WatchEvent.Kind, vararg paths: String, token: String = "") =
        WatchEvent.newBuilder().setKind(kind).setToken(token)
            .addAllPaths(paths.map { ByteString.copyFromUtf8(it) }).build()

    @Test fun classifiesPorcelainStatus() {
        assertEquals(ChangeKind.UNTRACKED, StatusMapping.classify("??"))
        assertEquals(ChangeKind.MODIFIED, StatusMapping.classify(".M"))
        assertEquals(ChangeKind.MODIFIED, StatusMapping.classify("MM"))
        assertEquals(ChangeKind.ADDED, StatusMapping.classify("A."))
        assertEquals(ChangeKind.ADDED, StatusMapping.classify("AM"))
        assertEquals(ChangeKind.DELETED, StatusMapping.classify(".D"))
        assertEquals(ChangeKind.DELETED, StatusMapping.classify("D."))
        assertEquals(ChangeKind.RENAMED, StatusMapping.classify("R."))
        assertEquals(ChangeKind.CONFLICT, StatusMapping.classify("UU"))
        assertEquals(ChangeKind.CONFLICT, StatusMapping.classify("AA"))
        assertNull(StatusMapping.classify("AD"))
        assertNull(StatusMapping.classify(".."))
        assertNull(StatusMapping.classify("!!"))
    }

    @Test fun resolveKeepsPathsUnderTheRoot() {
        val root = Paths.get("/work/repo")
        assertEquals(Paths.get("/work/repo/a/b.txt"), StatusMapping.resolve(root, ByteString.copyFromUtf8("a/b.txt")))
        assertNull(StatusMapping.resolve(root, ByteString.copyFromUtf8("../x")))
        assertNull(StatusMapping.resolve(root, ByteString.copyFromUtf8("a/../../x")))
        assertNull(StatusMapping.resolve(root, ByteString.copyFromUtf8("/etc/passwd")))
        assertNull(StatusMapping.resolve(root, ByteString.copyFromUtf8("")))
        assertNull(StatusMapping.resolve(root, ByteString.copyFromUtf8(".")))
    }

    @Test fun scopePathsAreRootRelative() {
        val root = Paths.get("/work/repo")
        assertEquals(emptyList<String>(), StatusMapping.scopePaths(root, listOf(root), emptyList()))
        assertEquals(
            listOf("src", "a.txt"),
            StatusMapping.scopePaths(root, listOf(Paths.get("/work/repo/src")), listOf(Paths.get("/work/repo/a.txt"))),
        )
        assertNull(StatusMapping.scopePaths(root, emptyList(), listOf(Paths.get("/other/a.txt"))))
    }

    @Test fun eventsMapToDirtyPlans() {
        assertEquals(setOf("a", "b"), WatchPlan.plan(ev(WatchEvent.Kind.KIND_PATHS, "a", "b")).paths)
        assertTrue(WatchPlan.plan(ev(WatchEvent.Kind.KIND_HEAD)).everything)
        assertTrue(WatchPlan.plan(ev(WatchEvent.Kind.KIND_STAGE)).everything)
        assertTrue(WatchPlan.plan(ev(WatchEvent.Kind.KIND_RESYNC)).everything)
        assertTrue(WatchPlan.plan(ev(WatchEvent.Kind.KIND_OP_STARTED)).isEmpty)
        assertTrue(WatchPlan.plan(ev(WatchEvent.Kind.KIND_CLOUD)).isEmpty)
    }

    @Test fun tooManyPathsBecomeEverything() {
        val a = DirtyPlan(false, (1..600).map { "a$it" }.toSet())
        val b = DirtyPlan(false, (1..600).map { "b$it" }.toSet())
        assertTrue(a.merge(b).everything)
        assertFalse(a.merge(DirtyPlan(false, setOf("x"))).everything)
    }

    @Test fun burstIsCoalescedIntoOnePlan() = runBlocking {
        val events = flow {
            emit(ev(WatchEvent.Kind.KIND_PATHS, "a"))
            emit(ev(WatchEvent.Kind.KIND_OP_STARTED))
            emit(ev(WatchEvent.Kind.KIND_PATHS, "b"))
        }
        val out = WatchPlan.coalesce(events, 20).toList()
        assertEquals(1, out.size)
        assertEquals(setOf("a", "b"), out[0].paths)
    }

    @Test fun headMoveWinsOverPaths() = runBlocking {
        val events = flow {
            emit(ev(WatchEvent.Kind.KIND_PATHS, "a"))
            emit(ev(WatchEvent.Kind.KIND_HEAD))
        }
        val out = WatchPlan.coalesce(events, 20).toList()
        assertEquals(1, out.size)
        assertTrue(out[0].everything)
    }
}
