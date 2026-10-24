package asc.asc.spike

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class PartialPatchTest {
    /** A minimal `git apply` for the patch shape `build` emits, used to prove the patch reproduces the content. */
    private fun apply(base: String, patch: String): String {
        val src = if (base.isEmpty()) emptyList() else base.removeSuffix("\n").split("\n")
        val out = ArrayList<String>()
        var pos = 0
        val hunk = Regex("""^@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@$""")
        val ls = patch.removeSuffix("\n").split("\n")
        var i = 2
        while (i < ls.size) {
            val m = hunk.matchEntire(ls[i]) ?: error("bad hunk header " + ls[i])
            val len = m.groupValues[2].ifEmpty { "1" }.toInt()
            val start = if (len == 0) m.groupValues[1].toInt() else m.groupValues[1].toInt() - 1
            while (pos < start) out.add(src[pos++])
            i++
            while (i < ls.size && !ls[i].startsWith("@@")) {
                val body = ls[i].substring(1)
                when (ls[i][0]) {
                    ' ' -> { assertEquals(src[pos], body); out.add(body); pos++ }
                    '-' -> { assertEquals(src[pos], body); pos++ }
                    '+' -> out.add(body)
                    else -> fail("bad line")
                }
                i++
            }
        }
        while (pos < src.size) out.add(src[pos++])
        return if (out.isEmpty()) "" else out.joinToString("\n") + "\n"
    }

    private val base = (1..20).joinToString("") { "line$it\n" }
    private val current = base.replace("line2\n", "LINE2\n").replace("line15\n", "line15\nextra\n").replace("line18\n", "")

    @Test fun wholeSelectionEqualsCurrent() {
        val all = PartialPatch.changes(base, current).map { it.id }.toSet()
        assertEquals(current, PartialPatch.partialContent(base, current, all))
        assertEquals(current, apply(base, PartialPatch.build("a/b.txt", base, current, all)!!))
    }

    @Test fun emptySelectionIsNoPatch() {
        assertNull(PartialPatch.build("f.txt", base, current, emptySet()))
        assertEquals(base, PartialPatch.partialContent(base, current, emptySet()))
    }

    @Test fun everySubsetRoundTrips() {
        val ids = PartialPatch.changes(base, current).map { it.id }
        assertTrue(ids.size >= 4)
        for (mask in 0 until (1 shl ids.size)) {
            val sel = ids.filterIndexed { k, _ -> mask and (1 shl k) != 0 }.toSet()
            val want = PartialPatch.partialContent(base, current, sel)
            val patch = PartialPatch.build("f.txt", base, current, sel)
            assertEquals(want, if (patch == null) base else apply(base, patch))
        }
    }

    @Test fun onlyTheChosenLineLandsInThePatch() {
        val extra = PartialPatch.changes(base, current).first { it.insert && it.text == "extra" }.id
        val patch = PartialPatch.build("f.txt", base, current, setOf(extra))!!
        assertTrue(patch.contains("+extra\n"))
        assertTrue(!patch.contains("LINE2") && !patch.contains("-line18"))
        assertTrue(patch.startsWith("--- a/f.txt\n+++ b/f.txt\n@@ "))
    }

    @Test fun newFileFromEmptyBase() {
        val cur = "a\nb\n"
        val sel = PartialPatch.changes("", cur).filter { it.text == "b" }.map { it.id }.toSet()
        assertEquals("b\n", apply("", PartialPatch.build("n.txt", "", cur, sel)!!))
    }

    @Test fun unsafePathsAreRejected() {
        for (p in listOf("", "/etc/x", "-x", "../x", "a/../b", "a\nb", "a\"b", "a\\b", "a//b", "a/./b")) {
            try {
                PartialPatch.build(p, base, current, setOf(0))
                fail("accepted " + p)
            } catch (_: PartialPatchUnsupported) {
            }
        }
    }

    @Test fun missingFinalNewlineAndBinaryFallBack() {
        for ((b, c) in listOf("a\nb" to "a\nc\n", "a\n" to "a\u0000\n")) {
            try {
                PartialPatch.build("f", b, c, setOf(0))
                fail("accepted")
            } catch (_: PartialPatchUnsupported) {
            }
        }
        assertNotNull(PartialPatch.build("f", "a\n", "b\n", setOf(0)))
    }
}
