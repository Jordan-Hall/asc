package asc.asc.vcs

import asc.v1.Bridge.RunEvent
import java.nio.file.Paths
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class CheckinTest {
    @Test fun createArgsCarryTheMessageAndNoBranch() {
        assertEquals(listOf("-m", "fix x", "--no-branch", "--end-of-options"), CheckinPlan.createArgs("fix x"))
        assertNull(CheckinPlan.createArgs("   "))
    }

    @Test fun amendKeepsTheOldMessageWhenBlank() {
        assertEquals(listOf("--end-of-options"), CheckinPlan.amendArgs(""))
        assertEquals(listOf("-m", "new", "--end-of-options"), CheckinPlan.amendArgs("new"))
    }

    @Test fun rollbackStopsOptionParsingBeforePaths() {
        assertEquals(
            listOf("--staged", "--worktree", "--end-of-options", "--", "-rf", "a/b.txt"),
            CheckinPlan.rollbackArgs(listOf("-rf", "a/b.txt")),
        )
        assertNull(CheckinPlan.rollbackArgs(emptyList()))
    }

    @Test fun pathsAreRootRelative() {
        val root = Paths.get("/work/repo")
        assertEquals("a/b.txt", CheckinPlan.relative(root, Paths.get("/work/repo/a/b.txt")))
        assertNull(CheckinPlan.relative(root, Paths.get("/work/repo")))
        assertNull(CheckinPlan.relative(root, Paths.get("/work/other/a.txt")))
        assertNull(CheckinPlan.relative(root, Paths.get("/work/repo/../x")))
        assertEquals(listOf("a"), CheckinPlan.relativePaths(root, listOf(Paths.get("/work/repo/a"), Paths.get("/work/repo/a"))))
    }

    @Test fun groupsByNearestRoot() {
        val r1 = Paths.get("/w/one").toAbsolutePath()
        val r2 = Paths.get("/w/one/nested").toAbsolutePath()
        val isRoot = { p: java.nio.file.Path -> p == r1 || p == r2 }
        val g = CheckinPlan.groupByRoot(
            listOf(Paths.get("/w/one/a.txt"), Paths.get("/w/one/nested/b.txt"), Paths.get("/elsewhere/c.txt")),
            isRoot,
        )
        assertEquals(setOf(r1, r2), g.keys)
        assertEquals(1, g.getValue(r2).size)
    }

    @Test fun runOutcomeFoldsExitAndStderr() {
        val ok = RunOutcome.fold(listOf(RunEvent.newBuilder().setStderrLine("hi").build(), RunEvent.newBuilder().setExit(0).build()))
        assertTrue(ok.ok)
        val bad = RunOutcome.fold(listOf(RunEvent.newBuilder().setStderrLine("boom").build(), RunEvent.newBuilder().setExit(1).build()))
        assertFalse(bad.ok)
        assertEquals("boom", bad.message())
        assertFalse(RunOutcome.fold(emptyList()).ok)
    }

    @Test fun ascignoreMatchesGitignoreSubset() {
        val r = IgnoreRules(listOf("# c", "*.log", "/build/", "docs/**/tmp", "!keep.log", "src/gen"))
        assertTrue(r.isIgnored("a/b/x.log"))
        assertFalse(r.isIgnored("keep.log"))
        assertTrue(r.isIgnored("build", isDirectory = true))
        assertFalse(r.isIgnored("build", isDirectory = false))
        assertTrue(r.isIgnored("build/out.bin"))
        assertFalse(r.isIgnored("sub/build/out.bin"))
        assertTrue(r.isIgnored("docs/a/b/tmp"))
        assertTrue(r.isIgnored("docs/tmp"))
        assertTrue(r.isIgnored("src/gen/x.kt"))
        assertFalse(r.isIgnored("other/src/gen"))
        assertFalse(r.isIgnored("main.kt"))
    }
}
