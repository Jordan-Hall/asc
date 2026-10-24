package asc.asc.vcs

import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.changes.IgnoredFileDescriptor
import com.intellij.openapi.vcs.changes.IgnoredFileProvider
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths

/**
 * A gitignore-style matcher for `.ascignore` (SPEC 24.5.1). Supports comments, `!` negation, a leading `/`
 * (anchored), a trailing `/` (directories only), `*`, `?` and `**`. The last matching rule wins.
 */
class IgnoreRules(lines: List<String>) {
    private class Rule(val regex: Regex, val negate: Boolean, val dirOnly: Boolean)

    private val rules: List<Rule> = lines.mapNotNull(::parse)

    /** True when [rel] (forward slashes, root-relative) is ignored, either directly or through an ignored parent directory. */
    fun isIgnored(rel: String, isDirectory: Boolean = false): Boolean {
        val parts = rel.trim('/').split('/').filter { it.isNotEmpty() }
        if (parts.isEmpty()) return false
        for (i in 1..parts.size) {
            val dir = i < parts.size || isDirectory
            if (matches(parts.subList(0, i).joinToString("/"), dir)) return true
        }
        return false
    }

    private fun matches(path: String, isDir: Boolean): Boolean {
        var ignored = false
        for (r in rules) {
            if (r.dirOnly && !isDir) continue
            if (r.regex.matches(path)) ignored = !r.negate
        }
        return ignored
    }

    companion object {
        private fun parse(raw: String): Rule? {
            var s = raw.trimEnd('\r', '\n')
            if (s.isBlank() || s.startsWith("#")) return null
            val negate = s.startsWith("!")
            if (negate) s = s.substring(1)
            s = s.trimEnd()
            val dirOnly = s.endsWith("/")
            if (dirOnly) s = s.trimEnd('/')
            if (s.isEmpty()) return null
            val anchored = s.startsWith("/") || s.trimStart('/').contains('/')
            s = s.trimStart('/')
            val body = StringBuilder()
            var i = 0
            while (i < s.length) {
                val c = s[i]
                when {
                    c == '*' && s.startsWith("**/", i) -> { body.append("(?:.*/)?"); i += 3; continue }
                    c == '*' && s.startsWith("**", i) -> { body.append(".*"); i += 2; continue }
                    c == '*' -> body.append("[^/]*")
                    c == '?' -> body.append("[^/]")
                    else -> body.append(Regex.escape(c.toString()))
                }
                i++
            }
            val prefix = if (anchored) "" else "(?:.*/)?"
            return Rule(Regex("$prefix$body"), negate, dirOnly)
        }
    }
}

/** Reports files matched by an asc root's `.ascignore` as ignored, so the Commit window and project view hide them. */
class AscIgnoreProvider : IgnoredFileProvider {
    private val cache = HashMap<Path, Pair<Long, IgnoreRules>>()

    override fun isIgnoredFile(project: Project, filePath: FilePath): Boolean {
        val abs = Paths.get(filePath.path)
        val root = CheckinPlan.rootOf(abs) ?: return false
        val rel = CheckinPlan.relative(root, abs) ?: return false
        return rulesFor(root).isIgnored(rel, filePath.isDirectory)
    }

    // Patterns are matched in isIgnoredFile; there is no file list to enumerate without walking the tree.
    override fun getIgnoredFiles(project: Project): Set<IgnoredFileDescriptor> = emptySet()

    override fun getIgnoredGroupDescription(): String = ".ascignore"

    @Synchronized
    private fun rulesFor(root: Path): IgnoreRules {
        val file = root.resolve(".ascignore")
        val stamp = try {
            if (Files.isRegularFile(file)) Files.getLastModifiedTime(file).toMillis() else -1L
        } catch (_: java.io.IOException) {
            -1L
        }
        cache[root]?.takeIf { it.first == stamp }?.let { return it.second }
        val rules = if (stamp < 0) IgnoreRules(emptyList()) else try {
            IgnoreRules(Files.readAllLines(file, Charsets.UTF_8))
        } catch (_: java.io.IOException) {
            IgnoreRules(emptyList())
        }
        cache[root] = stamp to rules
        return rules
    }
}
