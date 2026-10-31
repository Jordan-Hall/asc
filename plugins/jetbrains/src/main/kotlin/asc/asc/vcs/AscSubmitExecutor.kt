package asc.asc.vcs

import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.changes.Change
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

    override fun getHelpId(): String? = null

    private class SubmitSession(private val project: Project) : CommitSession {
        // A custom session replaces the platform's own commit, so the create runs here first, then the submit.
        override fun execute(changes: Collection<Change>, commitMessage: String?) {
            val paths = changes.flatMap { c ->
                listOfNotNull(c.afterRevision?.file, c.beforeRevision?.file).map { java.nio.file.Paths.get(it.path) }
            }
            val failures = ArrayList<String>()
            val created = ArrayList<Path>()
            for ((root, files) in CheckinPlan.groupByRoot(paths)) {
                val args = CheckinPlan.createArgs(commitMessage.orEmpty())
                if (args == null) {
                    failures.add("The commit message is empty")
                    break
                }
                try {
                    AscRun.stage(root, CheckinPlan.relativePaths(root, files))
                    val outcome = AscRun.run(root, "create", args)
                    if (outcome.ok) created.add(root) else failures.add(outcome.message())
                } catch (e: BridgeException) {
                    failures.add(e.message ?: "The asc bridge is unavailable")
                }
            }
            // A failed submit leaves the new change in place.
            failures.addAll(submitAll(created))
            if (failures.isNotEmpty()) {
                Messages.showErrorDialog(project, failures.joinToString("\n"), "asc create and submit failed")
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
