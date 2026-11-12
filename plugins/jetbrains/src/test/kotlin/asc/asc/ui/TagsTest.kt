package asc.asc.ui

import asc.asc.vcs.RunOutcome
import asc.v1.BridgeOuterClass.RunEvent
import asc.v1.Cli.OpResult
import asc.v1.Cli.TagDetail
import asc.v1.Cli.TagEntry
import com.google.protobuf.ByteString
import java.nio.file.Paths
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TagsTest {
    private fun result(vararg tags: TagEntry): ByteArray =
        OpResult.newBuilder().setTag(TagDetail.newBuilder().addAllTags(tags.toList())).build().toByteArray()

    private fun entry(name: String, subject: String = "") = TagEntry.newBuilder().setName(name)
        .setTarget(ByteString.copyFrom(ByteArray(20) { 0xab.toByte() }))
        .setAnnotated(subject.isNotEmpty()).setSubject(ByteString.copyFromUtf8(subject)).build()

    @Test fun namesFollowTheBridgeAndGitRules() {
        assertNull(TagPlan.nameProblem("v1.0.0"))
        assertNull(TagPlan.nameProblem("release/2026"))
        for (bad in listOf("", "-x", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b", "a\u0001")) {
            assertNotNull(bad, TagPlan.nameProblem(bad))
        }
        assertNull(TagPlan.nameProblem("x".repeat(TagPlan.NAME_MAX)))
        assertNotNull(TagPlan.nameProblem("x".repeat(TagPlan.NAME_MAX + 1)))
        assertNotNull(TagPlan.messageProblem("x".repeat(TagPlan.MESSAGE_MAX + 1)))
    }

    @Test fun argvIsFixedAndStopsOptionParsing() {
        assertEquals(listOf("v1"), TagPlan.createArgs("v1", "  "))
        assertEquals(listOf("-m=first", "v1"), TagPlan.createArgs("v1", " first "))
        assertNull(TagPlan.createArgs("-rf", ""))
        assertEquals(listOf("v1"), TagPlan.deleteArgs("v1"))
        assertNull(TagPlan.deleteArgs("a b"))
        assertEquals(listOf("--tags"), TagPlan.pushArgs())
        assertEquals(listOf("--tags"), ActionPlan.args(StackVerb.PUSH_TAGS))
    }

    @Test fun everyTagVerbIsAllowlisted() {
        val verbs = GeneratedAllowlist.parse().verbs
        for (v in listOf("tag", "tag --list", "tag --delete", "stack get", "share")) {
            assertNotNull(v, verbs[v])
        }
        assertTrue("-m" in verbs.getValue("tag").flags)
        assertTrue("--worktree" in verbs.getValue("stack get").flags)
        assertEquals(Confirm.MODAL, verbs.getValue("tag --delete").confirm)
    }

    @Test fun aTagResultParsesAndAnythingElseIsEmpty() {
        val rows = TagPlan.parse(result(entry("v1"), entry("v2", "second")))
        assertEquals(listOf("v1", "v2"), rows.map { it.name })
        assertEquals("ab".repeat(20), rows[0].target)
        assertEquals(TagRow("v2", "ab".repeat(20), true, "second"), rows[1])
        assertEquals("v2  ${"ab".repeat(6)}  second", TagPlan.label(rows[1]))
        assertEquals("Delete tag v1 (${"ab".repeat(6)})?", TagPlan.deleteQuestion(rows[0]))
        assertEquals(emptyList<TagRow>(), TagPlan.parse(null))
        assertEquals(emptyList<TagRow>(), TagPlan.parse(byteArrayOf(0x7f, 0x7f)))
        assertEquals(emptyList<TagRow>(), TagPlan.parse(OpResult.getDefaultInstance().toByteArray()))
    }

    @Test fun theRunOutcomeKeepsTheOpResult() {
        val bytes = result(entry("v1"))
        val out = RunOutcome.fold(
            listOf(
                RunEvent.newBuilder().setStderrLine("x").build(),
                RunEvent.newBuilder().setOpResult(ByteString.copyFrom(bytes)).build(),
                RunEvent.newBuilder().setExit(0).build(),
            ),
        )
        assertTrue(out.ok)
        assertEquals(listOf("v1"), TagPlan.parse(out.opResult).map { it.name })
        assertEquals(out, RunOutcome(0, listOf("x"), bytes.copyOf()))
    }

    @Test fun stackGetNeedsADirectoryInsideTheRoot() {
        val root = Paths.get("/work/repo")
        assertEquals(
            listOf("--worktree=wt/feat", "feat"),
            TagPlan.stackGetArgs(root, Paths.get("/work/repo/wt/feat"), " feat "),
        )
        assertNull(TagPlan.stackGetArgs(root, Paths.get("/work/other"), "feat"))
        assertNull(TagPlan.stackGetArgs(root, root, "feat"))
        assertNull(TagPlan.stackGetArgs(root, Paths.get("/work/repo/wt"), "--all"))
        assertNull(TagPlan.stackGetArgs(root, Paths.get("/work/repo/wt"), ""))
    }
}
