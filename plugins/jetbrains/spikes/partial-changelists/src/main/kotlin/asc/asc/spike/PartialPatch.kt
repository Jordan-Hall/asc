package asc.asc.spike

/** Thrown when a partial patch cannot be built safely; the caller then falls back to a whole-file commit. */
class PartialPatchUnsupported(message: String) : Exception(message)

/** One changed line between base and current. [id] is stable for a given (base, current) pair. */
data class ChangeLine(val id: Int, val insert: Boolean, val text: String)

/**
 * Spike P53: turn "base content (a `Workspace.ReadFile` revision) + current content + chosen changed lines" into the
 * `git apply --cached` patch that `WsStageRequest.patch` carries. This is the part `AscCheckinEnvironment` needs for
 * `arePartialChangelistsSupported`; the platform tracker only supplies the chosen line ids.
 */
object PartialPatch {
    private enum class Kind { EQ, DEL, INS }

    private data class Op(val kind: Kind, val text: String, val id: Int)

    private const val CONTEXT = 3

    /** Paths reach a patch header, so they must be plain root-relative paths. */
    fun checkPath(path: String) {
        if (path.isEmpty() || path.startsWith("/") || path.startsWith("-") || path.contains('\\')) {
            throw PartialPatchUnsupported("unsafe path")
        }
        if (path.any { it == '\n' || it == '\r' || it == '\u0000' || it == '"' || it == '\t' }) {
            throw PartialPatchUnsupported("unsafe path")
        }
        if (path.split('/').any { it == ".." || it == "." || it.isEmpty() }) throw PartialPatchUnsupported("unsafe path")
    }

    private fun lines(text: String): List<String> {
        if (text.isEmpty()) return emptyList()
        if (text.contains('\u0000')) throw PartialPatchUnsupported("binary content")
        if (!text.endsWith("\n")) throw PartialPatchUnsupported("no newline at end of file")
        return text.removeSuffix("\n").split("\n")
    }

    private fun diff(a: List<String>, b: List<String>): List<Op> {
        if (a.size.toLong() * b.size > 4_000_000L) throw PartialPatchUnsupported("file too large for line selection")
        val n = a.size
        val m = b.size
        val lcs = Array(n + 1) { IntArray(m + 1) }
        for (i in n - 1 downTo 0) {
            for (j in m - 1 downTo 0) {
                lcs[i][j] = if (a[i] == b[j]) lcs[i + 1][j + 1] + 1 else maxOf(lcs[i + 1][j], lcs[i][j + 1])
            }
        }
        val ops = ArrayList<Op>()
        var i = 0
        var j = 0
        var id = 0
        while (i < n || j < m) {
            if (i < n && j < m && a[i] == b[j]) {
                ops.add(Op(Kind.EQ, a[i], -1))
                i++
                j++
            } else if (i < n && (j >= m || lcs[i + 1][j] >= lcs[i][j + 1])) {
                ops.add(Op(Kind.DEL, a[i], id++))
                i++
            } else {
                ops.add(Op(Kind.INS, b[j], id++))
                j++
            }
        }
        return ops
    }

    /** The changed lines, in order, so a UI or tracker can name them by id. */
    fun changes(base: String, current: String): List<ChangeLine> =
        diff(lines(base), lines(current)).filter { it.kind != Kind.EQ }
            .map { ChangeLine(it.id, it.kind == Kind.INS, it.text) }

    /** The text that results from applying only [selected] changes to [base]. */
    fun partialContent(base: String, current: String, selected: Set<Int>): String =
        resolve(diff(lines(base), lines(current)), selected).filter { it.kind != Kind.DEL }
            .joinToString("") { it.text + "\n" }

    private fun resolve(ops: List<Op>, selected: Set<Int>): List<Op> = ops.mapNotNull {
        when (it.kind) {
            Kind.EQ -> it
            Kind.DEL -> if (it.id in selected) it else Op(Kind.EQ, it.text, -1)
            Kind.INS -> if (it.id in selected) it else null
        }
    }

    /** A `git apply --cached` patch for [path] that moves base to the selected content; null when nothing is selected. */
    fun build(path: String, base: String, current: String, selected: Set<Int>): String? {
        checkPath(path)
        val ops = resolve(diff(lines(base), lines(current)), selected)
        if (ops.none { it.kind != Kind.EQ }) return null
        val sb = StringBuilder()
        sb.append("--- a/").append(path).append('\n').append("+++ b/").append(path).append('\n')
        // Runs of changes padded by CONTEXT lines, merged when the padding overlaps.
        val changed = ops.indices.filter { ops[it].kind != Kind.EQ }
        var s = 0
        while (s < changed.size) {
            var e = s
            while (e + 1 < changed.size && changed[e + 1] - changed[e] <= 2 * CONTEXT) e++
            val from = maxOf(0, changed[s] - CONTEXT)
            val to = minOf(ops.size - 1, changed[e] + CONTEXT)
            val slice = ops.subList(from, to + 1)
            val oldStart = ops.subList(0, from).count { it.kind != Kind.INS }
            val newStart = ops.subList(0, from).count { it.kind != Kind.DEL }
            val oldLen = slice.count { it.kind != Kind.INS }
            val newLen = slice.count { it.kind != Kind.DEL }
            sb.append("@@ -").append(range(oldStart, oldLen)).append(" +").append(range(newStart, newLen)).append(" @@\n")
            for (op in slice) {
                val sign = if (op.kind == Kind.EQ) ' ' else if (op.kind == Kind.DEL) '-' else '+'
                sb.append(sign).append(op.text).append('\n')
            }
            s = e + 1
        }
        return sb.toString()
    }

    private fun range(start: Int, len: Int): String =
        if (len == 0) "$start,0" else if (len == 1) "${start + 1}" else "${start + 1},$len"
}
