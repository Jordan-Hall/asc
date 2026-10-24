package asc.asc.vcs

import com.intellij.openapi.vcs.VcsKey
import com.intellij.openapi.vcs.VcsRootChecker
import java.io.IOException
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.InvalidPathException
import java.nio.file.Path
import java.nio.file.Paths

/**
 * Recognition of an asc workspace root: a `.git` gitfile whose gitdir holds the `asc-workspace` marker
 * (SPEC 9.4, IP-D6). A non-asc folder costs one stat of `.git` and nothing else.
 * Trust checks (ownership, registration) belong to the asc binary, not to this detector.
 */
object AscRoots {
    const val MARKER = "asc-workspace"
    private const val MAX_GITFILE_BYTES = 4096L
    private const val PREFIX = "gitdir:"

    fun isAscRoot(root: Path): Boolean = gitDir(root) != null

    /** The gitdir named by the root's `.git` gitfile when it holds the marker, else null. */
    fun gitDir(root: Path): Path? {
        try {
            val dotGit = root.resolve(".git")
            if (!Files.isRegularFile(dotGit)) return null
            if (Files.size(dotGit) > MAX_GITFILE_BYTES) return null
            val line = Files.readAllLines(dotGit, StandardCharsets.UTF_8).firstOrNull()?.trim() ?: return null
            if (!line.startsWith(PREFIX)) return null
            val target = line.removePrefix(PREFIX).trim()
            if (target.isEmpty()) return null
            val dir = root.resolve(Paths.get(target)).normalize()
            return if (Files.isRegularFile(dir.resolve(MARKER))) dir else null
        } catch (_: IOException) {
            return null
        } catch (_: InvalidPathException) {
            return null
        } catch (_: SecurityException) {
            return null
        }
    }

    fun isAscRoot(path: String): Boolean =
        try {
            isAscRoot(Paths.get(path))
        } catch (_: InvalidPathException) {
            false
        }
}

class AscRootChecker : VcsRootChecker() {
    override fun getSupportedVcs(): VcsKey = AscVcs.KEY

    override fun isRoot(path: String): Boolean = AscRoots.isAscRoot(path)

    override fun isVcsDir(path: String): Boolean = path.substringAfterLast('/').substringAfterLast('\\') == ".git"
}
