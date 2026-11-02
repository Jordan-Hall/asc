package asc.asc.vcs

import com.intellij.openapi.vfs.WritingAccessProvider
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vfs.VirtualFile
import java.io.IOException
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.InvalidPathException
import java.nio.file.Path
import java.nio.file.Paths
import java.nio.file.attribute.FileTime
import java.util.concurrent.ConcurrentHashMap

/**
 * Read-only paths from the repository's path rules (SPEC 15.5.1). `asc fetch` stores the levels the
 * server gives this person in `<common dir>/asc-path-access`; a path under a rule directory below
 * `write` is read-only. The same format and checks as the CLI's `path_lock`.
 */
data class PathAccess(val remote: String, val repoLevel: String, val dirs: List<Pair<String, String>>) {
    /** As in the CLI, the repository level alone locks nothing: only a rule directory below write. */
    fun writable(path: String): Boolean {
        val best = dirs.filter { (d, _) -> under(path, d) }.maxByOrNull { it.first.length } ?: return true
        return LEVELS.indexOf(best.second) >= LEVELS.indexOf("write")
    }

    companion object {
        const val FILE = "asc-path-access"
        private const val HEADER = "asc path-access 1"
        val LEVELS = listOf("none", "build", "read", "write", "admin")

        fun parse(text: String): PathAccess? {
            val lines = text.split('\n').let { if (it.lastOrNull() == "") it.dropLast(1) else it }
            if (lines.firstOrNull() != HEADER) return null
            val remote = lines.getOrNull(1)?.takeIf { it.startsWith("remote ") }?.removePrefix("remote ") ?: return null
            val repo = lines.getOrNull(2)?.takeIf { it.startsWith("repo ") }?.removePrefix("repo ") ?: return null
            if (repo !in LEVELS) return null
            val dirs = lines.drop(3).map { line ->
                val m = Regex("^dir ([a-z]+)\t(.*)$").matchEntire(line) ?: return null
                val (level, dir) = m.destructured
                if (level !in LEVELS || !safeRel(dir)) return null
                dir to level
            }
            return PathAccess(remote, repo, dirs)
        }

        /** A plain `/`-separated path that never leaves the work tree. */
        fun safeRel(p: String): Boolean {
            if (p.isEmpty()) return true
            if (p.any { it == '\\' || it == ':' || it == '\u0000' || it == '\n' || it == '\r' }) return false
            return p.split('/').all { c ->
                c.isNotEmpty() && !c.endsWith('.') && !c.endsWith(' ') && '~' !in c && !c.equals(".git", ignoreCase = true)
            }
        }

        private fun under(path: String, dir: String) = dir.isEmpty() || path == dir || path.startsWith("$dir/")

        /** The repository's common git directory for a worktree's git dir. */
        fun commonDir(gitDir: Path): Path =
            try {
                val rel = Files.readString(gitDir.resolve("commondir"), StandardCharsets.UTF_8).trim()
                gitDir.resolve(Paths.get(rel)).normalize()
            } catch (_: IOException) {
                gitDir
            } catch (_: InvalidPathException) {
                gitDir
            }

        private class Seen(val stamp: FileTime?, val access: PathAccess?)

        private val cache = ConcurrentHashMap<Path, Seen>()

        /** The stored levels for the asc workspace at `root`, reread when the file changes. */
        fun load(root: Path): PathAccess? {
            val gitDir = AscRoots.gitDir(root) ?: return null
            val file = commonDir(gitDir).resolve(FILE)
            val stamp = try {
                Files.getLastModifiedTime(file)
            } catch (_: IOException) {
                null
            }
            cache[file]?.takeIf { it.stamp == stamp }?.let { return it.access }
            val access = stamp?.let {
                try {
                    parse(Files.readString(file, StandardCharsets.UTF_8))
                } catch (_: IOException) {
                    null
                }
            }
            cache[file] = Seen(stamp, access)
            return access
        }

        /** Whether the path rules keep `file` under the asc root `root` read-only. */
        fun locked(root: Path, file: Path): Boolean {
            if (!file.startsWith(root) || file == root) return false
            val rel = root.relativize(file).joinToString("/")
            return load(root)?.writable(rel) == false
        }
    }
}

/** Refuses edits to paths the path rules keep read-only, and names why. */
class AscWritingAccessProvider(private val project: Project) : WritingAccessProvider() {
    private fun locked(file: VirtualFile): Boolean {
        if (!file.isInLocalFileSystem) return false
        val root = ProjectLevelVcsManager.getInstance(project).getVcsRootFor(file) ?: return false
        return try {
            PathAccess.locked(Paths.get(root.path), Paths.get(file.path))
        } catch (_: InvalidPathException) {
            false
        }
    }

    override fun requestWriting(files: Collection<VirtualFile>): Collection<VirtualFile> = files.filter(::locked)

    override fun isPotentiallyWritable(file: VirtualFile): Boolean = !locked(file)

    override fun getReadOnlyMessage(): String = "Read-only: the repository's path rules"
}
