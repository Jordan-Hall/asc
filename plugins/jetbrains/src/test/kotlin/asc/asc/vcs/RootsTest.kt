package asc.asc.vcs

import java.nio.file.Files
import java.nio.file.Path
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class RootsTest {
    @get:Rule val tmp = TemporaryFolder()

    private fun workspace(marker: Boolean, gitfile: String? = null): Path {
        val root = tmp.newFolder().toPath()
        val gitdir = tmp.newFolder().toPath()
        if (marker) Files.writeString(gitdir.resolve("asc-workspace"), "ws-1")
        Files.writeString(root.resolve(".git"), gitfile ?: "gitdir: ${gitdir.toString().replace('\\', '/')}\n")
        return root
    }

    @Test fun gitfileWithMarkerIsAscRoot() = assertTrue(AscRoots.isAscRoot(workspace(marker = true)))

    @Test fun gitfileWithoutMarkerIsNot() = assertFalse(AscRoots.isAscRoot(workspace(marker = false)))

    @Test fun plainFolderIsNot() = assertFalse(AscRoots.isAscRoot(tmp.newFolder().toPath()))

    @Test fun gitDirectoryIsNot() {
        val root = tmp.newFolder().toPath()
        Files.createDirectory(root.resolve(".git"))
        assertFalse(AscRoots.isAscRoot(root))
    }

    @Test fun malformedGitfileIsNot() {
        assertFalse(AscRoots.isAscRoot(workspace(marker = true, gitfile = "not a gitfile")))
        assertFalse(AscRoots.isAscRoot(workspace(marker = true, gitfile = "gitdir:")))
    }

    @Test fun relativeGitdirResolves() {
        val root = tmp.newFolder().toPath()
        val gitdir = root.resolve("meta")
        Files.createDirectory(gitdir)
        Files.writeString(gitdir.resolve("asc-workspace"), "ws-1")
        Files.writeString(root.resolve(".git"), "gitdir: meta\n")
        assertTrue(AscRoots.isAscRoot(root))
    }

    @Test fun mappingReplacesGitAndKeepsOthers() {
        val current = listOf(
            MappingPlan.Mapping("/a", "Git"),
            MappingPlan.Mapping("/b", "hg4idea"),
            MappingPlan.Mapping("/c", "Git"),
        )
        val r = MappingPlan.plan(current, listOf("/a", "/b", "/d"))
        assertEquals(
            listOf(
                MappingPlan.Mapping("/a", "asc"),
                MappingPlan.Mapping("/b", "hg4idea"),
                MappingPlan.Mapping("/c", "Git"),
                MappingPlan.Mapping("/d", "asc"),
            ),
            r.mappings,
        )
        assertEquals(listOf("/a", "/d"), r.changedRoots)
    }

    @Test fun mappingIsIdempotent() {
        val current = listOf(MappingPlan.Mapping("/a", "asc"))
        assertTrue(MappingPlan.plan(current, listOf("/a")).changedRoots.isEmpty())
    }
}
