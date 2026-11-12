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
import asc.asc.auth.ConfigureAgents
import asc.asc.bridge.AscBridgeService
import asc.asc.bridge.BridgeException
import asc.asc.vcs.AscRun
import java.nio.file.Path
import java.nio.file.Paths
import java.util.concurrent.ConcurrentHashMap

/**
 * The verbs the menus run (SPEC 24.5.1). Each runs through the allowlisted `Bridge.Run` with [flags] and no
 * positional argument; no argument is a secret. [question] is asked first when the allowlist marks the verb
 * `confirm: modal`.
 */
enum class StackVerb(val verb: String, val title: String, val flags: List<String> = emptyList(), val question: String = "") {
    UP("up", "Move Up the Stack"),
    DOWN("down", "Move Down the Stack"),
    TOP("top", "Go to the Top of the Stack"),
    BOTTOM("bottom", "Go to the Bottom of the Stack"),
    RESTACK("restack", "Restack"),
    SYNC("sync", "Sync"),
    SUBMIT("submit", "Submit", question = "Submit this stack for review?"),
    ABSORB("absorb", "Absorb"),
    UNDO("undo", "Undo"),
    REDO("redo", "Redo"),
    UNCOMMIT("uncommit", "Uncommit"),
    UNAMEND("unamend", "Unamend"),
    CONTINUE("continue", "Continue the Paused Operation"),
    ABORT("abort", "Abort the Paused Operation"),
    FETCH("fetch", "Fetch"),
    PULL("pull", "Pull"),
    PUSH("push", "Push", question = "Push the current branch to its remote?"),
    PUSH_TAGS("push", "Push All Tags", listOf("--tags"), question = "Push every local tag to the remote?"),
    SHARE("share", "Share the Current Change"),
    STASH_PUSH("stash push", "Stash Changes"),
    STASH_PUSH_UNTRACKED("stash push", "Stash Changes Including Untracked", listOf("-u")),
    STASH_POP("stash pop", "Pop Stash"),
    STASH_APPLY("stash apply", "Apply Stash"),
    STASH_DROP("stash drop", "Drop Stash", question = "Drop the latest stash? It cannot be recovered."),
}

/** Pure argv planning for the actions; no platform types. */
object ActionPlan {
    private val allowlist by lazy { GeneratedAllowlist.parse() }

    /**
     * Args of a verb run directly. The bridge refuses `--`, `--end-of-options` and a flag whose value is a
     * separate argument; it adds `--end-of-options` before the positionals itself.
     */
    fun args(v: StackVerb): List<String> = v.flags

    /** `absorb -n` shows what would move; `absorb -f` applies it after the person confirms. */
    fun absorbPreview(): List<String> = listOf("-n")

    fun absorbApply(): List<String> = listOf("-f")

    /** The allowlist decides which verbs ask first (`confirm: modal`); a verb it does not list is never run. */
    fun needsConfirm(v: StackVerb): Boolean =
        (allowlist.verbs[v.verb] ?: error("verb not in the allowlist: " + v.verb)).confirm == Confirm.MODAL

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
            Messages.showYesNoDialog(project, v.question, v.title, Messages.getQuestionIcon()) !=
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

    fun notify(project: Project, title: String, text: String, ok: Boolean) {
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

/** "Configure Agents": `asc -C <root> mcp install --scope project`, run from the asc executable's directory. */
class AscConfigureAgentsAction : AnAction("Configure Agents"), DumbAware {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        val p = e.project
        e.presentation.isEnabledAndVisible = p != null && AscActions.firstRoot(p) != null
    }

    override fun actionPerformed(e: AnActionEvent) {
        val p = e.project ?: return
        val root = AscActions.firstRoot(p) ?: return
        val bridge = ApplicationManager.getApplication().getService(AscBridgeService::class.java)
        // Before any lookup: an asc planted under this root is never chosen.
        bridge.registerRoot(root.toString())
        ProgressManager.getInstance().run(object : Task.Backgroundable(p, "asc mcp install", true) {
            override fun run(indicator: ProgressIndicator) {
                val message = ConfigureAgents.run(bridge.executablePath(), root.toString())
                AscActions.notify(p, "Configure Agents", message, message.startsWith("Agents configured"))
            }
        })
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
class AscTopAction : AscStackAction(StackVerb.TOP)
class AscBottomAction : AscStackAction(StackVerb.BOTTOM)
class AscUncommitAction : AscStackAction(StackVerb.UNCOMMIT)
class AscUnamendAction : AscStackAction(StackVerb.UNAMEND)
class AscContinueAction : AscStackAction(StackVerb.CONTINUE)
class AscAbortAction : AscStackAction(StackVerb.ABORT)
class AscFetchAction : AscStackAction(StackVerb.FETCH)
class AscPullAction : AscStackAction(StackVerb.PULL)
class AscPushAction : AscStackAction(StackVerb.PUSH)
class AscPushTagsAction : AscStackAction(StackVerb.PUSH_TAGS)
class AscShareAction : AscStackAction(StackVerb.SHARE)
class AscStashPushAction : AscStackAction(StackVerb.STASH_PUSH)
class AscStashPushUntrackedAction : AscStackAction(StackVerb.STASH_PUSH_UNTRACKED)
class AscStashPopAction : AscStackAction(StackVerb.STASH_POP)
class AscStashApplyAction : AscStackAction(StackVerb.STASH_APPLY)
class AscStashDropAction : AscStackAction(StackVerb.STASH_DROP)

/** "asc Views" menu: opens the tool window on one tab (SPEC 24.5.1); visible only in a project with an asc root. */
open class AscOpenViewAction(private val title: String) : AnAction(title), DumbAware {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        val p = e.project
        e.presentation.isEnabledAndVisible = p != null && AscActions.firstRoot(p) != null
    }

    override fun actionPerformed(e: AnActionEvent) {
        AscToolWindowFactory.show(e.project ?: return, title)
    }
}

class AscOpenStackAction : AscOpenViewAction("Stack")
class AscOpenGraphAction : AscOpenViewAction("Graph")
class AscOpenReviewAction : AscOpenViewAction("Review")
class AscOpenInboxAction : AscOpenViewAction("Inbox")
class AscOpenActionsAction : AscOpenViewAction("Actions")
class AscOpenAffectedAction : AscOpenViewAction("Affected")
class AscOpenWorktreesAction : AscOpenViewAction("Worktrees")
class AscOpenSearchAction : AscOpenViewAction("Search")
class AscOpenHistoryAction : AscOpenViewAction("History")
