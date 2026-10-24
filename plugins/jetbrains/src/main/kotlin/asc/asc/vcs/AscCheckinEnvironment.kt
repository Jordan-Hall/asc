package asc.asc.vcs

import asc.v1.Bridge.RunEvent
import asc.v1.Bridge.RunRequest
import asc.v1.WorkspaceOuterClass.WsStageRequest
import com.google.protobuf.ByteString
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Key
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.changes.Change
import com.intellij.openapi.vcs.changes.CommitContext
import com.intellij.openapi.vcs.checkin.CheckinEnvironment
import com.intellij.openapi.vfs.VirtualFile
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService
import java.nio.file.Path
import java.nio.file.Paths
import kotlinx.coroutines.flow.toList

/** The result of one `Bridge.Run`: the exit code and the child's stderr lines (shown to the user, never logged). */
data class RunOutcome(val exit: Int, val stderr: List<String>) {
    val ok: Boolean get() = exit == 0

    fun message(): String = stderr.lastOrNull { it.isNotBlank() } ?: "asc exited with status $exit"

    companion object {
        /** Fold the event stream. A stream that ends with no `exit` event counts as a failure. */
        fun fold(events: List<RunEvent>): RunOutcome {
            var exit = -1
            val lines = ArrayList<String>()
            for (e in events) {
                when (e.eventCase) {
                    RunEvent.EventCase.STDERR_LINE -> lines.add(e.stderrLine)
                    RunEvent.EventCase.EXIT -> exit = e.exit
                    else -> {}
                }
            }
            return RunOutcome(exit, lines)
        }
    }
}

/** Pure argv and path planning for check-in, amend, submit and rollback; no platform types. */
object CheckinPlan {
    /** Args for `create`. `--no-branch` is explicit so the branchless setting cannot matter. Null for a blank message. */
    fun createArgs(message: String): List<String>? =
        if (message.isBlank()) null else listOf("-m", message, "--no-branch", "--end-of-options")

    /** Args for `modify`, which folds the staged changes into the current change; a blank message keeps the old one. */
    fun amendArgs(message: String): List<String> =
        if (message.isBlank()) listOf("--end-of-options") else listOf("-m", message, "--end-of-options")

    fun submitArgs(): List<String> = listOf("--end-of-options")

    /** Args for `restore --staged --worktree`; `--end-of-options` and `--` stop a path being read as a flag. */
    fun rollbackArgs(paths: List<String>): List<String>? =
        if (paths.isEmpty()) null else listOf("--staged", "--worktree", "--end-of-options", "--") + paths

    /** The root-relative, forward-slash path of [abs], or null when it lies outside [root] or is the root. */
    fun relative(root: Path, abs: Path): String? {
        val r = root.toAbsolutePath().normalize()
        val a = abs.toAbsolutePath().normalize()
        if (a == r || !a.startsWith(r)) return null
        return r.relativize(a).joinToString("/") { it.toString() }
    }

    /** Distinct root-relative paths for [files], preserving order; paths outside the root are dropped. */
    fun relativePaths(root: Path, files: Collection<Path>): List<String> =
        files.mapNotNull { relative(root, it) }.distinct()

    /** The nearest ancestor (or self) of [path] for which [isRoot] holds. */
    fun rootOf(path: Path, isRoot: (Path) -> Boolean = AscRoots::isAscRoot): Path? {
        var p: Path? = path.toAbsolutePath().normalize()
        while (p != null) {
            if (isRoot(p)) return p
            p = p.parent
        }
        return null
    }

    /** Group absolute paths by their asc root; paths under no root are dropped. */
    fun groupByRoot(paths: Collection<Path>, isRoot: (Path) -> Boolean = AscRoots::isAscRoot): Map<Path, List<Path>> {
        val out = LinkedHashMap<Path, MutableList<Path>>()
        for (p in paths) {
            val root = rootOf(p, isRoot) ?: continue
            out.getOrPut(root) { ArrayList() }.add(p)
        }
        return out
    }
}

/** Runs asc verbs and stages paths through the bridge. Blocking: call from a background progress only. */
object AscRun {
    fun run(root: Path, verb: String, args: List<String>): RunOutcome {
        val req = RunRequest.newBuilder().setVerb(verb).addAllArgs(args).setRoot(root.toString()).build().toByteArray()
        return runBlockingCancellable {
            val frames = service<AscBridgeService>().stream("asc.v1.Bridge/Run", req, root.toString()).toList()
            RunOutcome.fold(frames.map { RunEvent.parseFrom(it) })
        }
    }

    fun stage(root: Path, add: List<String>) {
        if (add.isEmpty()) return
        val req = WsStageRequest.newBuilder().addAllAdd(add.map { ByteString.copyFromUtf8(it) }).build().toByteArray()
        runBlockingCancellable { service<AscBridgeService>().unary("asc.v1.Workspace/Stage", req, root.toString()) }
    }
}

/**
 * `CheckinEnvironment` for asc (SPEC 24.5.1). Whole-file commits: the selected files are staged with
 * `Workspace.Stage`, then `create --no-branch` (or `modify` when [AMEND] is set on the commit context) runs.
 * Partial-line commits are not offered until spike P53 passes; hunks and lines are staged in `asc-ui`.
 */
class AscCheckinEnvironment(private val project: Project, private val vcs: AscVcs) : CheckinEnvironment {
    override fun getHelpId(): String? = null

    override fun getCheckinOperationName(): String = "Create"

    override fun isRefreshAfterCommitNeeded(): Boolean = true

    override fun commit(
        changes: List<Change>,
        commitMessage: String,
        commitContext: CommitContext,
        feedback: MutableSet<in String>,
    ): List<VcsException>? {
        val amend = commitContext.getUserData(AMEND) == true
        val paths = changes.flatMap { c ->
            listOfNotNull(c.afterRevision?.file, c.beforeRevision?.file).map { Paths.get(it.path) }
        }
        val errors = ArrayList<VcsException>()
        for ((root, files) in CheckinPlan.groupByRoot(paths)) {
            try {
                AscRun.stage(root, CheckinPlan.relativePaths(root, files))
                val outcome = if (amend) {
                    AscRun.run(root, "modify", CheckinPlan.amendArgs(commitMessage))
                } else {
                    val args = CheckinPlan.createArgs(commitMessage)
                        ?: return listOf(VcsException("The commit message is empty"))
                    AscRun.run(root, "create", args)
                }
                if (!outcome.ok) errors.add(VcsException(outcome.message()))
            } catch (e: BridgeException) {
                errors.add(VcsException(e.message ?: "The asc bridge is unavailable"))
            }
        }
        return errors.ifEmpty { null }
    }

    // Stage records a deletion for a missing path at the next commit, so there is no separate schedule step.
    override fun scheduleMissingFileForDeletion(files: List<FilePath>): List<VcsException>? = null

    override fun scheduleUnversionedFilesForAddition(files: List<VirtualFile>): List<VcsException>? {
        val errors = ArrayList<VcsException>()
        for ((root, abs) in CheckinPlan.groupByRoot(files.map { Paths.get(it.path) })) {
            try {
                AscRun.stage(root, CheckinPlan.relativePaths(root, abs))
            } catch (e: BridgeException) {
                errors.add(VcsException(e.message ?: "The asc bridge is unavailable"))
            }
        }
        return errors.ifEmpty { null }
    }

    companion object {
        /**
         * Set on the [CommitContext] by the amend toggle.
         * TODO(P41a): wire `AmendCommitAware` once it is verified against the 2026.1 SDK; no SDK is available here.
         */
        @JvmField
        val AMEND: Key<Boolean> = Key.create("asc.asc.amend")
    }
}
