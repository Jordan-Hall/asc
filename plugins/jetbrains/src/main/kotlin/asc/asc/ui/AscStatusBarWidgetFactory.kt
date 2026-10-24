package asc.asc.ui

import asc.v1.WorkspaceOuterClass.WsStacksRequest
import asc.v1.WorkspaceOuterClass.WsStacksResponse
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.wm.StatusBar
import com.intellij.openapi.wm.StatusBarWidget
import com.intellij.openapi.wm.StatusBarWidgetFactory
import com.intellij.openapi.wm.ToolWindowManager
import com.intellij.util.Consumer
import com.intellij.util.concurrency.AppExecutorUtil
import asc.asc.bridge.AscBridgeService
import asc.asc.vcs.AscStateListener
import asc.asc.vcs.DirtyPlan
import java.awt.event.MouseEvent
import java.nio.file.Paths
import java.util.concurrent.atomic.AtomicReference

/** The least of `Stacks` the status bar needs: the stack that holds the current change (SPEC 24.4.2, 24.5.1). */
data class StackSnap(
    val short: String,
    val position: Int,
    val total: Int,
    val behind: Int,
    val trunk: String,
    /** `StackEntry.state` of the current change: its cloud/review state, empty when unknown. */
    val cloudState: String,
    val needsRestack: Boolean,
)

/** Pure text for the widget; no platform types. */
object StatusModel {
    const val BEHIND_CAP = 999

    /** `kxyzq · 2/5 · ↓3 · draft`; `↓N` hidden at 0, the cloud state hidden when empty, a note when a restack is due. */
    fun text(s: StackSnap?): String {
        if (s == null) return ""
        val behind = s.behind.coerceIn(0, BEHIND_CAP)
        val parts = arrayListOf(s.short, "${s.position}/${s.total}")
        if (behind > 0) parts.add("↓$behind")
        if (s.cloudState.isNotBlank()) parts.add(s.cloudState)
        if (s.needsRestack) parts.add("needs restack")
        return parts.joinToString(" · ")
    }

    fun tooltip(s: StackSnap?): String {
        if (s == null) return "asc: this working copy is on no stack"
        val behind = s.behind.coerceIn(0, BEHIND_CAP)
        return buildString {
            append("Change ${s.short}, ${s.position} of ${s.total}.")
            if (behind > 0) append(" $behind commit(s) on ${s.trunk} are not in this stack.")
            if (s.needsRestack) append(" A change is behind its parent.")
            append(if (clickRestacks(s)) " Click to restack." else " Click to open the asc tool window.")
        }
    }

    /** True when a click should restack rather than open the tool window. */
    fun clickRestacks(s: StackSnap?): Boolean = s != null && (s.behind > 0 || s.needsRestack)

    /** The snapshot of the current change in a `Stacks` reply, or null when the work tree is on no stack. */
    fun snap(r: WsStacksResponse): StackSnap? {
        for (stack in r.stacksList) {
            val i = stack.changesList.indexOfFirst { it.current }
            if (i < 0) continue
            val c = stack.changesList[i]
            return StackSnap(
                short = c.short,
                position = i + 1,
                total = stack.changesCount,
                behind = stack.behind,
                trunk = stack.trunk,
                cloudState = c.state,
                needsRestack = stack.changesList.any { it.needsRestack },
            )
        }
        return null
    }
}

/** `statusBarWidgetFactory` for asc: available only for projects with an asc root. */
class AscStatusBarWidgetFactory : StatusBarWidgetFactory {
    override fun getId(): String = AscStatusWidget.ID

    override fun getDisplayName(): String = "asc"

    override fun isAvailable(project: Project): Boolean = AscToolWindowFactory.ascRoots(project).isNotEmpty()

    override fun createWidget(project: Project): StatusBarWidget = AscStatusWidget(project)

    override fun disposeWidget(widget: StatusBarWidget) = Disposer.dispose(widget)

    override fun canBeEnabledOn(statusBar: StatusBar): Boolean = true
}

class AscStatusWidget(private val project: Project) : StatusBarWidget, StatusBarWidget.TextPresentation {
    companion object {
        const val ID = "asc.asc.status"
    }

    private val snap = AtomicReference<StackSnap?>(null)

    @Volatile
    private var bar: StatusBar? = null

    override fun ID(): String = ID

    override fun getPresentation(): StatusBarWidget.WidgetPresentation = this

    override fun install(statusBar: StatusBar) {
        bar = statusBar
        project.messageBus.connect(this).subscribe(
            AscStateListener.ASC_STATE,
            AscStateListener { _: String, _: DirtyPlan -> refresh() },
        )
        refresh()
    }

    override fun dispose() {
        bar = null
    }

    override fun getText(): String = StatusModel.text(snap.get())

    override fun getAlignment(): Float = 0f

    override fun getTooltipText(): String = StatusModel.tooltip(snap.get())

    override fun getClickConsumer(): Consumer<MouseEvent> = Consumer {
        val root = AscToolWindowFactory.ascRoots(project).firstOrNull() ?: return@Consumer
        if (StatusModel.clickRestacks(snap.get())) {
            AscActions.runStackVerb(project, Paths.get(root), StackVerb.RESTACK)
        } else {
            ToolWindowManager.getInstance(project).getToolWindow("asc")?.activate(null)
        }
    }

    private fun refresh() {
        val root = AscToolWindowFactory.ascRoots(project).firstOrNull() ?: return
        AppExecutorUtil.getAppExecutorService().execute {
            val next = try {
                val req = WsStacksRequest.getDefaultInstance().toByteArray()
                val reply = runBlockingCancellable { service<AscBridgeService>().unary("asc.v1.Workspace/Stacks", req, root) }
                StatusModel.snap(WsStacksResponse.parseFrom(reply))
            } catch (_: Exception) {
                null // a bridge outage shows no text rather than a stale stack
            }
            snap.set(next)
            bar?.updateWidget(ID)
        }
    }
}
