package asc.asc.ui

import com.intellij.ide.BrowserUtil
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.roots.ProjectRootManager
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory
import com.intellij.openapi.wm.ex.ToolWindowManagerListener
import com.intellij.ui.HyperlinkLabel
import com.intellij.ui.components.JBLabel
import com.intellij.ui.components.JBTabbedPane
import com.intellij.ui.content.ContentFactory
import com.intellij.ui.jcef.JBCefApp
import com.intellij.util.concurrency.AppExecutorUtil
import com.intellij.util.ui.JBUI
import asc.asc.bridge.AscBridgeService
import asc.asc.vcs.AscRoots
import java.awt.BorderLayout
import java.net.URI
import java.util.concurrent.TimeUnit
import javax.swing.BoxLayout
import javax.swing.JPanel
import javax.swing.SwingUtilities
import kotlinx.coroutines.runBlocking

/**
 * The `asc` tool window (SPEC 24.5.1, 24.5.2): `asc-ui` in JCEF with the tabs Stack, Graph, Review, Inbox,
 * Actions, Affected, Worktrees, Search and History. The browser is created on first open and disposed when
 * the tool window has been hidden for [IDLE_MS] (10 minutes); `asc-ui` restores its state through
 * `PropertiesComponent`. Without JCEF the window shows a notice and a link to the same view on the forge.
 */
class AscToolWindowFactory : ToolWindowFactory, DumbAware {
    companion object {
        const val IDLE_MS = 10L * 60 * 1000

        /** The asc roots of the project, as the VCS layer finds them. */
        fun ascRoots(project: Project): List<String> {
            val roots = linkedSetOf<String>()
            project.basePath?.let { roots += it }
            ProjectRootManager.getInstance(project).contentRoots.forEach { roots += it.path }
            return roots.filter { AscRoots.isAscRoot(it) }
        }
    }

    override fun shouldBeAvailable(project: Project): Boolean = ascRoots(project).isNotEmpty()

    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) {
        val panel = JPanel(BorderLayout())
        val content = ContentFactory.getInstance().createContent(panel, "", false)
        content.isCloseable = false
        toolWindow.contentManager.addContent(content)
        val roots = ascRoots(project)
        if (!JBCefApp.isSupported()) {
            panel.add(fallback(roots), BorderLayout.CENTER)
            return
        }
        val host = BrowserHost(project, panel, roots)
        Disposer.register(toolWindow.disposable, host)
        project.messageBus.connect(host).subscribe(
            ToolWindowManagerListener.TOPIC,
            object : ToolWindowManagerListener {
                override fun stateChanged(toolWindowManager: com.intellij.openapi.wm.ToolWindowManager) {
                    val visible = toolWindowManager.getToolWindow(ID)?.isVisible == true
                    SwingUtilities.invokeLater { if (visible) host.shown() else host.hidden() }
                }
            },
        )
        if (toolWindow.isVisible) host.shown()
    }

    private fun fallback(roots: List<String>): JPanel {
        val p = JPanel()
        p.layout = BoxLayout(p, BoxLayout.Y_AXIS)
        p.border = JBUI.Borders.empty(12)
        p.add(JBLabel("The asc views need the embedded browser (JCEF), which this IDE does not provide."))
        val link = HyperlinkLabel("Open the same view on the forge")
        link.addHyperlinkListener {
            val bridge = ApplicationManager.getApplication().getService(AscBridgeService::class.java)
            ApplicationManager.getApplication().executeOnPooledThread {
                val info = try {
                    runBlocking { bridge.hello(roots) }.rootsList.firstOrNull { it.forgeUrl.isNotEmpty() && it.repo.isNotEmpty() }
                } catch (_: Exception) {
                    null
                } ?: return@executeOnPooledThread
                val u = try {
                    URI(info.forgeUrl.trimEnd('/') + "/" + info.repo)
                } catch (_: Exception) {
                    return@executeOnPooledThread
                }
                if (u.scheme == "https" && u.host != null && u.userInfo == null) BrowserUtil.browse(u)
            }
        }
        p.add(link)
        return p
    }
}

private const val ID = "asc"

/** Owns the tabs and the one browser behind them; creates the browser on first show and drops it when idle. */
private class BrowserHost(private val project: Project, private val panel: JPanel, private val roots: List<String>) : Disposable {
    private val tabs = JBTabbedPane()
    private val views = AscPage.VIEWS.values.toList()
    private var browser: AscBrowser? = null
    private var visible = false
    private val idle = IdleTimer(
        AscToolWindowFactory.IDLE_MS,
        { ms, task ->
            val f = AppExecutorUtil.getAppScheduledExecutorService().schedule({ SwingUtilities.invokeLater(task) }, ms, TimeUnit.MILLISECONDS)
            AutoCloseable { f.cancel(false) }
        },
        { if (!visible) drop() },
    )

    init {
        for (title in AscPage.VIEWS.keys) tabs.addTab(title, JPanel())
        tabs.addChangeListener { browser?.select(views[tabs.selectedIndex]) }
        panel.add(tabs, BorderLayout.NORTH)
    }

    fun shown() {
        visible = true
        idle.shown()
        if (browser == null) {
            val b = AscBrowser(project, views[tabs.selectedIndex.coerceAtLeast(0)], roots)
            browser = b
            panel.add(b.component, BorderLayout.CENTER)
            panel.revalidate()
        }
    }

    fun hidden() {
        visible = false
        idle.hidden()
    }

    private fun drop() {
        val b = browser ?: return
        browser = null
        panel.remove(b.component)
        Disposer.dispose(b)
        panel.revalidate()
        panel.repaint()
    }

    override fun dispose() {
        idle.shown()
        drop()
    }
}
