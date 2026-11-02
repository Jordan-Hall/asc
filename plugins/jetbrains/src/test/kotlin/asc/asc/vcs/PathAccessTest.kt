package asc.asc.vcs

import java.nio.file.Files
import java.nio.file.Path
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class PathAccessTest {
    @get:Rule val tmp = TemporaryFolder()

    private val stored =
        "asc path-access 1\nremote origin\nrepo write\n" +
            "dir write\t\ndir read\tdocs\ndir write\tdocs/drafts\ndir none\tsecret\n"

    @Test fun theDeepestRuleDirectoryDecides() {
        val a = PathAccess.parse(stored)!!
        assertEquals(4, a.dirs.size)
        assertTrue(a.writable("src/main.rs"))
        assertFalse(a.writable("docs/a.md"))
        assertTrue(a.writable("docs/drafts/b.md"))
        assertTrue(a.writable("docsx/a.md"))
        assertFalse(a.writable("secret/x"))
        assertTrue(PathAccess.parse("asc path-access 1\nremote origin\nrepo read\n")!!.writable("src/a"))
    }

    @Test fun anUnknownOrUnsafeFileIsIgnored() {
        assertNull(PathAccess.parse("asc path-access 2\nremote o\nrepo read\n"))
        assertNull(PathAccess.parse(stored + "dir read\t../x\n"))
        assertNull(PathAccess.parse(stored + "dir sudo\tx\n"))
        for (p in listOf("a/../b", ".git/x", "a\\b", "c:x", "GIT~1", "a./b", "a /b", "a//b")) {
            assertFalse(p, PathAccess.safeRel(p))
        }
        assertTrue(PathAccess.safeRel("docs/drafts"))
    }

    @Test fun aWorktreeReadsTheCommonDirectoryAndFollowsChanges() {
        val common = tmp.newFolder().toPath()
        val gitDir = common.resolve("worktrees/w")
        Files.createDirectories(gitDir)
        Files.writeString(gitDir.resolve("asc-workspace"), "ws-1")
        Files.writeString(gitDir.resolve("commondir"), "../..\n")
        val root: Path = tmp.newFolder().toPath()
        Files.writeString(root.resolve(".git"), "gitdir: ${gitDir.toString().replace('\\', '/')}\n")

        assertFalse(PathAccess.locked(root, root.resolve("docs/a.md")))
        Files.writeString(common.resolve(PathAccess.FILE), stored)
        Files.setLastModifiedTime(common.resolve(PathAccess.FILE), java.nio.file.attribute.FileTime.fromMillis(1_000))
        assertTrue(PathAccess.locked(root, root.resolve("docs/a.md")))
        assertFalse(PathAccess.locked(root, root.resolve("src/a.rs")))
        assertFalse(PathAccess.locked(root, common.resolve("docs/a.md")))
        Files.delete(common.resolve(PathAccess.FILE))
        assertFalse(PathAccess.locked(root, root.resolve("docs/a.md")))
    }
}
