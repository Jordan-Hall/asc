package asc.asc.vcs

import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.changes.Change
import com.intellij.openapi.vcs.rollback.RollbackEnvironment
import com.intellij.openapi.vcs.rollback.RollbackProgressListener
import asc.asc.bridge.BridgeException
import java.nio.file.Paths

/** `RollbackEnvironment` for asc (SPEC 24.5.1): `restore --staged --worktree` over the selected files. */
class AscRollbackEnvironment(private val project: Project) : RollbackEnvironment {
    override fun getRollbackOperationName(): String = "Rollback"

    override fun rollbackChanges(
        changes: List<Change>,
        exceptions: MutableList<VcsException>,
        listener: RollbackProgressListener,
    ) {
        val paths = changes.flatMap { c ->
            listOfNotNull(c.afterRevision?.file, c.beforeRevision?.file).map { Paths.get(it.path) }
        }
        restore(paths, exceptions)
    }

    override fun rollbackMissingFileDeletion(
        files: List<FilePath>,
        exceptions: MutableList<VcsException>,
        listener: RollbackProgressListener,
    ) = restore(files.map { Paths.get(it.path) }, exceptions)

    override fun rollbackModifiedWithoutCheckout(
        files: List<com.intellij.openapi.vfs.VirtualFile>,
        exceptions: MutableList<VcsException>,
        listener: RollbackProgressListener,
    ) {}

    override fun rollbackIfUnchanged(file: com.intellij.openapi.vfs.VirtualFile) {}

    private fun restore(paths: List<java.nio.file.Path>, exceptions: MutableList<VcsException>) {
        for ((root, files) in CheckinPlan.groupByRoot(paths)) {
            val args = CheckinPlan.rollbackArgs(CheckinPlan.relativePaths(root, files)) ?: continue
            try {
                val outcome = AscRun.run(root, "restore", args)
                if (!outcome.ok) exceptions.add(VcsException(outcome.message()))
            } catch (e: BridgeException) {
                exceptions.add(VcsException(e.message ?: "The asc bridge is unavailable"))
            }
        }
    }
}
