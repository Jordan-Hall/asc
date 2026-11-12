package asc.asc.ui

import asc.v1.Cli.OpResult
import asc.v1.Cli.StashDetail
import asc.v1.Cli.StashEntry
import asc.v1.Cli.TagDetail
import com.google.protobuf.ByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class StashesTest {
    private fun entry(i: Int, msg: String) = StashEntry.newBuilder().setIndex(i)
        .setCommit(ByteString.copyFrom(ByteArray(20) { 0xcd.toByte() })).setMessage(ByteString.copyFromUtf8(msg)).build()

    private fun result(vararg e: StashEntry): ByteArray = OpResult.newBuilder()
        .setStash(StashDetail.newBuilder().setAction("list").addAllEntries(e.toList())).build().toByteArray()

    @Test fun aStashResultParsesAndAnythingElseIsEmpty() {
        val rows = StashPlan.parse(result(entry(0, "WIP on main: abc\nmore"), entry(1, "")))
        assertEquals(listOf(StashRow(0, "cd".repeat(20), "WIP on main: abc\nmore"), StashRow(1, "cd".repeat(20), "")), rows)
        assertEquals("stash@{0}  ${"cd".repeat(6)}  WIP on main: abc", StashPlan.label(rows[0]))
        assertEquals("stash@{1}  ${"cd".repeat(6)}", StashPlan.label(rows[1]))
        assertEquals(emptyList<StashRow>(), StashPlan.parse(null))
        assertEquals(emptyList<StashRow>(), StashPlan.parse(byteArrayOf(0x7f, 0x7f)))
        val tag = OpResult.newBuilder().setTag(TagDetail.getDefaultInstance()).build().toByteArray()
        assertEquals(emptyList<StashRow>(), StashPlan.parse(tag))
    }

    @Test fun theTextIsBoundedAndSaysHowManyMore() {
        assertEquals("There are no stashes.", StashPlan.text(emptyList()))
        val many = (0 until StashPlan.SHOWN + 3).map { StashRow(it, "ab".repeat(20), "m$it") }
        val text = StashPlan.text(many)
        assertEquals(StashPlan.SHOWN + 1, text.lines().size)
        assertTrue(text.endsWith("… and 3 more"))
    }

    @Test fun stashListIsAllowlistedWithNoFlags() {
        val v = GeneratedAllowlist.parse().verbs["stash list"]
        assertNotNull(v)
        assertTrue(v!!.flags.isEmpty())
        assertEquals(emptyList<String>(), StashPlan.listArgs())
    }
}
