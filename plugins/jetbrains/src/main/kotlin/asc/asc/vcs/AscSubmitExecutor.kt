package asc.asc.vcs

import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.changes.CommitContext
import com.intellij.openapi.vcs.changes.CommitSession
import com.intellij.openapi.vcs.changes.LocalCommitExecutor
import com.intellij.openapi.ui.Messages
import asc.asc.bridge.BridgeException
import java.nio.file.Path

/**
 * The "Create and submit" commit button (SPEC 24.3.1): the normal [AscCheckinEnvironment] create runs, then
 * `asc submit` runs for each asc root once the commit has succeeded. A failed submit leaves the new change in place.
 */
class AscSubmitExecutor(private val project: Project) : LocalCommitExecutor() {
    override fun getActionText(): String = "Create and Submit"

    override fun createCommitSession(commitContext: CommitContext): CommitSession = SubmitSession(project)

    private class SubmitSession(private val project: Project) : CommitSession {
        override fun executionCompleted() {
            val basePath = project.basePath ?: return
            val failures = submitAll(CheckinPlan.groupByRoot(listOf(java.nio.file.Paths.get(basePath))).keys)
            if (failures.isNotEmpty()) {
                Messages.showErrorDialog(project, failures.joinToString("\n"), "asc submit failed")
            }
        }
    }

    companion object {
        /** Submit every root; returns one message per failure. */
        fun submitAll(roots: Collection<Path>): List<String> {
            val failures = ArrayList<String>()
            for (root in roots) {
                try {
                    val outcome = AscRun.run(root, "submit", CheckinPlan.submitArgs())
                    if (!outcome.ok) failures.add(outcome.message())
                } catch (e: BridgeException) {
                    failures.add(e.message ?: "The asc bridge is unavailable")
                }
            }
            return failures
        }
    }
}
