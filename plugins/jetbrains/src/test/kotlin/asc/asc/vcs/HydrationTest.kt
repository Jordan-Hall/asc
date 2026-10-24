package asc.asc.vcs

import java.nio.file.Files
import java.nio.file.Path
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class HydrationTest {
    @get:Rule val tmp = TemporaryFolder()

    private fun ascRoot(): Path {
        val root = tmp.newFolder().toPath()
        val gitdir = tmp.newFolder().toPath()
        Files.writeString(gitdir.resolve("asc-workspace"), "ws-1")
        Files.writeString(root.resolve(".git"), "gitdir: ${gitdir.toString().replace('\\', '/')}\n")
        return root
    }

    @Test fun excludesGeneratedFoldersOnly() {
        val urls = IndexGuard.excludeUrls(listOf(ascRoot()))
        assertEquals(6, urls.size)
        for (d in listOf("node_modules", "dist", ".turbo", "target", ".next", ".asc")) {
            assertTrue(d, urls.any { it.endsWith("/$d") })
        }
        assertTrue(urls.none { it.endsWith("/src") })
    }

    @Test fun exclusionNeverTouchesDisk() {
        val root = ascRoot()
        IndexGuard.excludeUrls(listOf(root))
        val names = Files.list(root).use { s -> s.map { it.fileName.toString() }.toList() }
        assertEquals(listOf(".git"), names)
    }

    @Test fun inertOutsideAscRoots() {
        assertTrue(IndexGuard.ascRoots(tmp.newFolder().toString(), emptyList()).isEmpty())
        assertEquals(1, IndexGuard.ascRoots(ascRoot().toString(), emptyList()).size)
    }

    @Test fun alertOverBudgetWithTopThreeFolders() {
        val a = IndexGuard.evaluate(
            listOf("rg" to 50L, "idea" to 10L),
            listOf("a/x" to 5L, "a/y" to 5L, "b/z" to 7L, "c/w" to 1L, "d/v" to 2L),
            budget = 20, lastNotifiedMs = null, nowMs = 1000,
        )
        assertNotNull(a)
        assertEquals("rg", a!!.process)
        assertEquals(listOf("a", "b", "d"), a.topFolders)
        assertTrue(IndexGuard.message(a).startsWith("Search is hydrating files"))
    }

    @Test fun noAlertUnderBudget() =
        assertNull(IndexGuard.evaluate(listOf("rg" to 20L), emptyList(), 20, null, 0))

    @Test fun oneNotificationPer24Hours() {
        val p = listOf("rg" to 99L)
        assertNull(IndexGuard.evaluate(p, emptyList(), 20, 0L, IndexGuard.NOTIFY_INTERVAL_MS - 1))
        assertNotNull(IndexGuard.evaluate(p, emptyList(), 20, 0L, IndexGuard.NOTIFY_INTERVAL_MS))
    }

    @Test fun ascignoreHidesGeneratedAndKeepsSources() {
        val rules = IgnoreRules(listOf("node_modules/", "dist/"))
        assertTrue(rules.isIgnored("node_modules/a/b.js"))
        assertTrue(rules.isIgnored("pkg/dist/x.js"))
        assertTrue(!rules.isIgnored("src/main.ts"))
    }
}
