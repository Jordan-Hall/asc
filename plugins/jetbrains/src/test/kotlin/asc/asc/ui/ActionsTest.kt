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

    @Test fun everyVerbStopsOptionParsing() {
        for (v in StackVerb.values()) assertEquals(listOf("--end-of-options"), ActionPlan.args(v))
        assertEquals(
            listOf("up", "down", "restack", "sync", "submit", "absorb", "undo", "redo"),
            StackVerb.values().map { it.verb },
        )
    }

    @Test fun absorbPreviewsBeforeApplying() {
        assertEquals(listOf("-n", "--end-of-options"), ActionPlan.absorbPreview())
        assertEquals(listOf("-f", "--end-of-options"), ActionPlan.absorbApply())
        assertTrue(ActionPlan.nothingToAbsorb(listOf("", "  ")))
        assertFalse(ActionPlan.nothingToAbsorb(listOf("move a.txt into abc")))
    }

    @Test fun onlySubmitAsksFirst() {
        assertTrue(ActionPlan.needsConfirm(StackVerb.SUBMIT))
        assertFalse(ActionPlan.needsConfirm(StackVerb.RESTACK))
    }
}
