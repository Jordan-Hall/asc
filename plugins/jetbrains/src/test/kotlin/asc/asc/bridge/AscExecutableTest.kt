package asc.asc.bridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AscExecutableTest {
    private fun win(
        setting: String? = "",
        path: String? = "",
        home: String? = null,
        roots: List<String> = emptyList(),
        files: Set<String>,
    ) = AscExecutable.resolve(
        setting, path, null, home, windows = true, projectRoots = roots,
        isFile = { it.lowercase() in files.map { f -> f.lowercase() } },
    )

    private fun nix(
        setting: String? = "",
        path: String? = "",
        home: String? = null,
        roots: List<String> = emptyList(),
        files: Set<String>,
    ) = AscExecutable.resolve(setting, path, null, home, windows = false, projectRoots = roots, isFile = { it in files })

    @Test fun absoluteMeansIndependentOfTheCurrentDirectoryAndDrive() {
        for (p in listOf("C:\\bin\\asc.exe", "c:/bin/asc.exe", "\\\\srv\\share\\asc.exe")) assertTrue(p, AscExecutable.isAbsolute(p, true))
        for (p in listOf("", "asc", "asc.exe", ".\\asc.exe", "..\\asc.exe", "\\asc.exe", "C:asc.exe", "bin\\asc.exe", "\\\\srv")) {
            assertFalse(p, AscExecutable.isAbsolute(p, true))
        }
        assertTrue(AscExecutable.isAbsolute("/usr/bin/asc", false))
        for (p in listOf("", "asc", "./asc", "bin/asc")) assertFalse(p, AscExecutable.isAbsolute(p, false))
    }

    @Test fun settingThenPathThenAscHome() {
        assertEquals("/x/asc", nix(setting = "/x/asc", path = "/a", files = setOf("/x/asc")))
        assertEquals("/b/asc", nix(path = "/a:/b", files = setOf("/b/asc")))
        assertEquals("/h/.asc/bin/asc", nix(path = "/a", home = "/h", files = setOf("/h/.asc/bin/asc")))
        assertEquals("C:\\ok\\asc.exe", win(path = "C:\\ok", files = setOf("C:\\ok\\asc.exe")))
        assertEquals("C:\\ok\\asc.exe", win(setting = "asc", path = "C:\\ok", files = setOf("C:\\ok\\asc.exe")))
    }

    @Test fun noneFoundIsNullNeverABareName() {
        assertNull(nix(path = "/a", files = emptySet()))
        assertNull(nix(setting = "/x/asc", files = emptySet()))
        assertNull(win(path = "C:\\ok", files = emptySet()))
    }

    @Test fun relativeSettingsAndPathEntriesAreNeverSearched() {
        val any = setOf("./asc", "asc", "/a/asc")
        assertNull(nix(setting = "./asc", path = "/a", files = any))
        assertNull(nix(setting = "bin/asc", path = "/a", files = any))
        assertNull(nix(path = ".::rel", files = any))
        assertEquals("/ok/asc", nix(path = ".:/ok", files = setOf("/ok/asc")))
        val files = setOf("C:\\ok\\asc.exe")
        for (s in listOf(".\\asc.exe", "..\\asc.exe", "\\asc.exe", "C:asc.exe", "sub\\asc.exe")) {
            assertNull(s, win(setting = s, path = "C:\\ok", files = files))
        }
        assertEquals("C:\\ok\\asc.exe", win(path = ".;;rel;C:\\ok", files = files))
    }

    @Test fun aPlantedAscInTheProjectIsNeverChosen() {
        val ws = "C:\\work\\repo"
        val files = setOf("C:\\work\\repo\\asc.exe", "C:\\work\\repo\\bin\\asc.exe", "C:\\ok\\asc.exe")
        assertEquals(
            "C:\\ok\\asc.exe",
            win(path = "$ws;C:\\WORK\\Repo\\bin;C:\\ok", roots = listOf(ws), files = files),
        )
        assertNull(win(setting = "$ws\\asc.exe", roots = listOf(ws), files = files))
        assertNull(win(path = ws, roots = listOf(ws), files = files))
        // ".." does not escape the check; a sibling with the same prefix is not inside.
        assertTrue(AscExecutable.inside("C:\\work\\repo\\..\\repo\\asc.exe", ws, true))
        assertFalse(AscExecutable.inside("C:\\work\\repo2\\asc.exe", ws, true))
        assertNull(nix(path = "/w/repo:/w/repo/bin", roots = listOf("/w/repo"), files = setOf("/w/repo/asc", "/w/repo/bin/asc")))
        assertEquals("/w/repo2/asc", nix(path = "/w/repo2", roots = listOf("/w/repo"), files = setOf("/w/repo2/asc")))
    }
}

class AscBridgeServiceExecutableTest {
    private val ws = "C:/work/repo"
    private val planted = "c:/work/repo/asc.exe"
    private val real = "c:/ok/asc.exe"

    private fun service(path: String, open: List<String>): AscBridgeService {
        val s = AscBridgeService()
        s.windows = true
        s.environment = mapOf("PATH" to path)
        s.openRoots = { open }
        s.isFile = { it.replace('\\', '/').lowercase() in setOf(planted, real) }
        return s
    }

    @Test fun aPlantedBinaryUnderAnOpenProjectIsNeverChosenBeforeAnyHello() {
        // No Hello, no registered root: the open projects alone forbid the workspace.
        assertEquals("C:/ok\\asc.exe", service("$ws;C:/ok", listOf(ws)).executablePath())
        assertNull(service(ws, listOf(ws)).executablePath())
    }

    @Test fun aRootNamedByACallOrTheActionIsForbiddenAtOnce() {
        val s = service(ws, emptyList())
        // Not yet known to be a project: the planted file would be chosen.
        assertEquals("C:/work/repo\\asc.exe", s.executablePath())
        s.registerRoot(ws)
        assertNull(s.executablePath())
        s.registerRoot("")
        assertEquals(listOf(ws), s.projectRoots)
    }

    @Test fun aSubfolderOfTheProjectIsForbiddenToo() {
        assertNull(service("$ws/bin", listOf(ws)).also { it.isFile = { true } }.executablePath())
    }
}
