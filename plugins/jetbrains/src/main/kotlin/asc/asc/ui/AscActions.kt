package asc.asc.ui

import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.progress.ProgressIndicator
import com.intellij.openapi.progress.ProgressManager
import com.intellij.openapi.progress.Task
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.Messages
import asc.asc.bridge.BridgeException
import asc.asc.vcs.AscRun
import java.nio.file.Path
import java.nio.file.Paths
import java.util.concurrent.ConcurrentHashMap

/** The stack verbs of SPEC 24.5.1. Each runs through the allowlisted `Bridge.Run`; no argument is a secret. */
enum class StackVerb(val verb: String, val title: String) {
    UP("up", "Move Up the Stack"),
    DOWN("down", "Move Down the Stack"),
    RESTACK("restack", "Restack"),
    SYNC("sync", "Sync"),
    SUBMIT("submit", "Submit"),
    ABSORB("absorb", "Absorb"),
    UNDO("undo", "Undo"),
    REDO("redo", "Redo"),
}

/** Pure argv planning for the actions; no platform types. */
object ActionPlan {
    private const val END = "--end-of-options"

    /** Args of a verb run directly. */
    fun args(@Suppress("UNUSED_PARAMETER") v: StackVerb): List<String> = listOf(END)

    /** `absorb -n` shows what would move; `absorb -f` applies it after the person confirms. */
    fun absorbPreview(): List<String> = listOf("-n", END)

    fun absorbApply(): List<String> = listOf("-f", END)

    /** `submit` publishes, so it asks first (its allowlist row is `confirm: modal`). */
    fun needsConfirm(v: StackVerb): Boolean = v == StackVerb.SUBMIT

    /** The preview shown in the absorb confirmation: the last 20 non-blank stderr lines. */
    fun previewText(stderr: List<String>): String =
        stderr.filter { it.isNotBlank() }.takeLast(20).joinToString("\n")

    /** An absorb preview with no lines has nothing to confirm. */
    fun nothingToAbsorb(stderr: List<String>): Boolean = stderr.none { it.isNotBlank() }
}

/** Entry points shared by the actions and the status bar widget. */
object AscActions {
    private val locks = ConcurrentHashMap<Path, Any>()

    fun firstRoot(project: Project): Path? =
        AscToolWindowFactory.ascRoots(project).firstOrNull()?.let { Paths.get(it) }

    /** Runs [v] on [root] in a background task, serialised per root, with a confirmation first where the plan asks. */
    fun runStackVerb(project: Project, root: Path, v: StackVerb) {
        if (ActionPlan.needsConfirm(v) &&
            Messages.showYesNoDialog(project, "Submit this stack for review?", v.title, Messages.getQuestionIcon()) !=
            Messages.YES
        ) return
        ProgressManager.getInstance().run(object : Task.Backgroundable(project, "asc ${v.verb}", true) {
            override fun run(indicator: ProgressIndicator) {
                synchronized(locks.computeIfAbsent(root) { Any() }) {
                    if (v == StackVerb.ABSORB) {
                        absorb(project, root)
                    } else {
                        report(project, v, execute(root, v.verb, ActionPlan.args(v)))
                    }
                }
            }
        })
    }

    private fun execute(root: Path, verb: String, args: List<String>): Pair<Boolean, String> = try {
        val out = AscRun.run(root, verb, args)
        out.ok to out.message()
    } catch (e: BridgeException) {
        false to (e.message ?: "The asc bridge is unavailable")
    }

    private fun absorb(project: Project, root: Path) {
        val title = StackVerb.ABSORB.title
        val preview = try {
            AscRun.run(root, "absorb", ActionPlan.absorbPreview())
        } catch (e: BridgeException) {
            return notify(project, title, e.message ?: "The asc bridge is unavailable", false)
        }
        if (!preview.ok) return notify(project, title, preview.message(), false)
        if (ActionPlan.nothingToAbsorb(preview.stderr)) return notify(project, title, "Nothing to absorb.", true)
        var yes = false
        ApplicationManager.getApplication().invokeAndWait {
            yes = Messages.showYesNoDialog(
                project, ActionPlan.previewText(preview.stderr), "Absorb into the Stack", "Absorb", "Cancel",
                Messages.getQuestionIcon(),
            ) == Messages.YES
        }
        if (yes) report(project, StackVerb.ABSORB, execute(root, "absorb", ActionPlan.absorbApply()))
    }

    private fun report(project: Project, v: StackVerb, r: Pair<Boolean, String>) =
        notify(project, v.title, if (r.first) "Done." else r.second, r.first)

    private fun notify(project: Project, title: String, text: String, ok: Boolean) {
        NotificationGroupManager.getInstance().getNotificationGroup("asc")
            .createNotification(title, text, if (ok) NotificationType.INFORMATION else NotificationType.ERROR)
            .notify(project)
    }
}

/** One menu action per verb; visible only in a project with an asc root. */
open class AscStackAction(private val verb: StackVerb) : AnAction(verb.title), DumbAware {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        val p = e.project
        e.presentation.isEnabledAndVisible = p != null && AscActions.firstRoot(p) != null
    }

    override fun actionPerformed(e: AnActionEvent) {
        val p = e.project ?: return
        val root = AscActions.firstRoot(p) ?: return
        AscActions.runStackVerb(p, root, verb)
    }
}

class AscUpAction : AscStackAction(StackVerb.UP)
class AscDownAction : AscStackAction(StackVerb.DOWN)
class AscRestackAction : AscStackAction(StackVerb.RESTACK)
class AscSyncAction : AscStackAction(StackVerb.SYNC)
class AscSubmitAction : AscStackAction(StackVerb.SUBMIT)
class AscAbsorbAction : AscStackAction(StackVerb.ABSORB)
class AscUndoAction : AscStackAction(StackVerb.UNDO)
class AscRedoAction : AscStackAction(StackVerb.REDO)
