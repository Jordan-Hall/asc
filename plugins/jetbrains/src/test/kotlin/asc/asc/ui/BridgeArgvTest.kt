package asc.asc.ui

import asc.asc.vcs.CheckinPlan
import java.nio.file.Files
import java.nio.file.Path
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Every argv the actions plan passes the same allowlist checks the bridge applies (the Kotlin copy in
 * [RelayChecks] mirrors `argv_schema.rs`): no `--`, no `--end-of-options`, flag values joined with `=`.
 */
class BridgeArgvTest {
    private val json = Path.of("..", "..", "packages", "asc-client-ts", "src", "transport", "allowlist.json")
    private val al: Allowlist = Allowlist.parse(Files.readString(json))
    private val root: Path = Files.createTempDirectory("asc-argv")
    private val scope = RunScope(root.toString(), "$root/tmp")

    private fun allowed(verb: String, args: List<String>?) {
        val d = RelayChecks.checkRun(al, verb, args ?: error("refused: $verb"), scope)
        assertTrue("$verb $args: $d", d is Decision.Allow)
    }

    @Test fun everyPlannedArgvIsAllowed() {
        Files.createDirectories(root.resolve("a"))
        Files.writeString(root.resolve("a/b.txt"), "x")
        Files.writeString(root.resolve("-rf"), "x")
        Files.createDirectories(root.resolve("wt/feat"))
        for (v in StackVerb.values()) allowed(v.verb, ActionPlan.args(v))
        allowed("absorb", ActionPlan.absorbPreview())
        allowed("absorb", ActionPlan.absorbApply())
        allowed("create", CheckinPlan.createArgs("fix x\n\nbody"))
        allowed("modify", CheckinPlan.amendArgs(""))
        allowed("modify", CheckinPlan.amendArgs("new"))
        allowed("submit", CheckinPlan.submitArgs())
        allowed("restore", CheckinPlan.rollbackArgs(listOf("-rf", "a/b.txt")))
        allowed("tag --list", TagPlan.listArgs())
        allowed("tag", TagPlan.createArgs("v1", ""))
        allowed("tag", TagPlan.createArgs("v1", "first"))
        allowed("tag --delete", TagPlan.deleteArgs("v1"))
        allowed("push", TagPlan.pushArgs())
        allowed("stack get", TagPlan.stackGetArgs(root, root.resolve("wt/feat"), "feat"))
    }
}
