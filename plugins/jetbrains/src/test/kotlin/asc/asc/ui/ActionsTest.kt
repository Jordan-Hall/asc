package asc.asc.ui

import asc.v1.WorkspaceOuterClass.StackEntry
import asc.v1.WorkspaceOuterClass.WsStack
import asc.v1.WorkspaceOuterClass.WsStacksResponse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ActionsTest {
    private fun snap(behind: Int = 0, restack: Boolean = false, cloud: String = "") =
        StackSnap("kxyzq", 2, 5, behind, "master", cloud, restack)

    @Test fun textShowsPositionBehindCloudAndRestack() {
        assertEquals("kxyzq · 2/5", StatusModel.text(snap()))
        assertEquals("kxyzq · 2/5 · ↓3 · draft", StatusModel.text(snap(3, cloud = "draft")))
        assertEquals("kxyzq · 2/5 · needs restack", StatusModel.text(snap(restack = true)))
        assertEquals("", StatusModel.text(null))
    }

    @Test fun behindIsCapped() {
        assertTrue(StatusModel.text(snap(5000)).contains("↓999"))
        assertFalse(StatusModel.text(snap(-4)).contains("↓"))
    }

    @Test fun clickRestacksOnlyWhenDue() {
        assertTrue(StatusModel.clickRestacks(snap(1)))
        assertTrue(StatusModel.clickRestacks(snap(restack = true)))
        assertFalse(StatusModel.clickRestacks(snap()))
        assertFalse(StatusModel.clickRestacks(null))
    }

    @Test fun snapFindsTheCurrentChange() {
        fun e(short: String, cur: Boolean, r: Boolean = false) =
            StackEntry.newBuilder().setShort(short).setCurrent(cur).setNeedsRestack(r).setState("public").build()
        val stack = WsStack.newBuilder().setTrunk("master").setBehind(2)
            .addChanges(e("a", false)).addChanges(e("b", true)).addChanges(e("c", false, true)).build()
        val s = StatusModel.snap(WsStacksResponse.newBuilder().addStacks(stack).build())!!
        assertEquals("b", s.short)
        assertEquals(2, s.position)
        assertEquals(3, s.total)
        assertEquals(2, s.behind)
        assertTrue(s.needsRestack)
        assertEquals("public", s.cloudState)
        assertNull(StatusModel.snap(WsStacksResponse.getDefaultInstance()))
    }

    @Test fun theBridgeAddsTheSeparatorSoNoVerbSendsOne() {
        for (v in StackVerb.values()) assertFalse(v.name, "--end-of-options" in ActionPlan.args(v))
        assertEquals(listOf("-u"), ActionPlan.args(StackVerb.STASH_PUSH_UNTRACKED))
        assertEquals(emptyList<String>(), ActionPlan.args(StackVerb.STASH_PUSH))
    }

    @Test fun everyVerbIsAllowlistedWithItsFlags() {
        val verbs = GeneratedAllowlist.parse().verbs
        for (v in StackVerb.values()) {
            val entry = verbs[v.verb] ?: throw AssertionError("not allowlisted: " + v.verb)
            for (f in v.flags) assertTrue("${v.verb} $f", f in entry.flags)
        }
    }

    @Test fun everyMenuVerbHasAnAction() {
        val xml = javaClass.classLoader.getResource("META-INF/plugin.xml")!!.readText()
        for (v in StackVerb.values()) assertTrue(v.title, xml.contains("text=\"${v.title}\""))
    }

    @Test fun absorbPreviewsBeforeApplying() {
        assertEquals(listOf("-n"), ActionPlan.absorbPreview())
        assertEquals(listOf("-f"), ActionPlan.absorbApply())
        assertTrue(ActionPlan.nothingToAbsorb(listOf("", "  ")))
        assertFalse(ActionPlan.nothingToAbsorb(listOf("move a.txt into abc")))
    }

    @Test fun theAllowlistDecidesWhatAsksFirst() {
        val asks = StackVerb.values().filter { ActionPlan.needsConfirm(it) }
        assertEquals(listOf(StackVerb.SUBMIT, StackVerb.PUSH, StackVerb.PUSH_TAGS, StackVerb.STASH_DROP), asks)
        for (v in asks) assertTrue(v.name, v.question.endsWith("?") || v.question.endsWith("."))
    }
}
