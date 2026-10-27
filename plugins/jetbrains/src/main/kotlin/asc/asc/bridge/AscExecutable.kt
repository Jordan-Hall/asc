package asc.asc.bridge

import java.io.File

/**
 * Where the `asc` executable is. Only an absolute path outside the open project roots is ever returned, so a
 * child process never looks the name up in the working directory (Windows does that first) and a repository
 * cannot plant an `asc.exe` for the IDE to start. The pure functions take their inputs, so a test can name a
 * Windows layout on any host.
 */
object AscExecutable {
    /** An absolute path that does not depend on the current directory or the current drive. */
    fun isAbsolute(path: String, windows: Boolean): Boolean {
        if (path.isEmpty() || path.contains('\u0000')) return false
        if (!windows) return path.startsWith("/")
        val drive = path.length >= 3 && path[0].isLetter() && path[1] == ':' && (path[2] == '\\' || path[2] == '/')
        val unc = path.startsWith("\\\\") && path.drop(2).split('\\', '/').let { it.size >= 2 && it[0].isNotEmpty() && it[1].isNotEmpty() }
        return drive || unc
    }

    /**
     * `setting` when it is an absolute file (or a bare name, looked up like `PATH`), else the absolute entries of
     * `path`, then `<ascHome or ~/.asc>/bin`. A relative setting or entry, and anything inside one of
     * `projectRoots`, is skipped. Null when no executable is found.
     */
    fun resolve(
        setting: String?,
        path: String?,
        ascHome: String?,
        home: String?,
        windows: Boolean,
        projectRoots: List<String>,
        isFile: (String) -> Boolean = { File(it).isFile },
    ): String? {
        val sep = if (windows) '\\' else '/'
        fun join(dir: String, name: String) = dir.trimEnd('/', '\\') + sep + name
        fun usable(p: String) = isAbsolute(p, windows) && projectRoots.none { inside(p, it, windows) } && isFile(p)

        val given = setting?.trim().orEmpty()
        var name = if (windows) "asc.exe" else "asc"
        if (given.isNotEmpty()) {
            if (isAbsolute(given, windows)) return if (usable(given)) given else null
            if (given.any { it == '/' || it == '\\' || it == ':' || it == '\u0000' }) return null
            name = if (windows && !given.endsWith(".exe", ignoreCase = true)) "$given.exe" else given
        }
        val dirs = (path.orEmpty()).split(if (windows) ';' else ':')
            .map { if (windows) it.removeSurrounding("\"") else it }
            .filter { isAbsolute(it, windows) }
            .toMutableList()
        val root = ascHome?.takeIf { it.isNotEmpty() } ?: home?.takeIf { it.isNotEmpty() }?.let { join(it, ".asc") }
        if (root != null && isAbsolute(root, windows)) dirs.add(join(root, "bin"))
        return dirs.map { join(it, name) }.firstOrNull { usable(it) }
    }

    /** The roots an executable must stay out of: those registered so far and those of the open projects, without blanks. */
    fun forbiddenRoots(registered: List<String>, open: List<String>): List<String> =
        (registered + open).filter { it.isNotBlank() }.distinct()

    /** True when `child` is `parent` or below it, comparing segments (`.` and `..` resolved, case folded on Windows). */
    fun inside(child: String, parent: String, windows: Boolean): Boolean {
        val c = segments(child, windows)
        val p = segments(parent, windows)
        return c.size >= p.size && c.subList(0, p.size) == p
    }

    private fun segments(path: String, windows: Boolean): List<String> {
        val out = ArrayList<String>()
        for (raw in path.split('/', '\\')) {
            val s = if (windows) raw.lowercase() else raw
            when (s) {
                "", "." -> {}
                ".." -> if (out.isNotEmpty()) out.removeAt(out.size - 1)
                else -> out.add(s)
            }
        }
        return out
    }
}
