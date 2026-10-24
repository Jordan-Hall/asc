package asc.asc.ui

import asc.v1.Bridge.RootInfo
import com.google.gson.JsonElement
import com.google.gson.JsonNull
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.google.gson.JsonPrimitive
import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.ide.BrowserUtil
import com.intellij.ide.ui.LafManagerListener
import com.intellij.ide.util.PropertiesComponent
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.EDT
import com.intellij.openapi.editor.colors.EditorColorsManager
import com.intellij.openapi.fileEditor.OpenFileDescriptor
import com.intellij.openapi.fileTypes.FileTypeManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.Messages
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.vcs.FileStatus
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.ui.JBColor
import com.intellij.ui.jcef.JBCefApp
import com.intellij.ui.jcef.JBCefBrowser
import com.intellij.ui.jcef.JBCefBrowserBase
import com.intellij.ui.jcef.JBCefJSQuery
import asc.asc.auth.BridgeAuth
import asc.asc.bridge.AscBridgeService
import asc.asc.vcs.AscCalls
import asc.asc.vcs.AscContent
import java.awt.Color
import java.io.ByteArrayInputStream
import java.net.URI
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap
import javax.swing.JComponent
import javax.swing.UIManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.cef.CefApp
import org.cef.browser.CefBrowser
import org.cef.browser.CefFrame
import org.cef.callback.CefCallback
import org.cef.handler.CefLoadHandlerAdapter
import org.cef.handler.CefRequestHandlerAdapter
import org.cef.handler.CefResourceHandler
import org.cef.handler.CefResourceRequestHandler
import org.cef.handler.CefResourceRequestHandlerAdapter
import org.cef.handler.CefSchemeHandlerFactory
import org.cef.misc.BoolRef
import org.cef.misc.IntRef
import org.cef.misc.StringRef
import org.cef.network.CefRequest
import org.cef.network.CefResponse

// The JCEF host for asc-ui (SPEC 24.5.2). `https://asc-ui.local/` is served by a scheme handler from jar
// resources (`/asc-ui/…`) and from two generated documents, so nothing is loaded from the network: the page's
// CSP has `connect-src 'none'`, and every request to another host is cancelled. The page talks to Kotlin only
// through one `JBCefJSQuery` (the relay of AscRelay.kt, which is deny by default).
//
// TODO(P28a): the asc-ui bundle does not yet read `data-asc-view` / `data-asc-root` and does not use
// `window.ascHost`; its JetBrains `Platform` adapter (kind "jetbrains") is part of the asc-ui bootstrap.
// TODO(P48): the Gradle copy of `packages/asc-ui/dist` into `/asc-ui/` and of allowlist.json into
// `/asc/asc/allowlist.json` (generateAllowlist) is a build step this unit does not own.

/** The page shell. The root is passed as data, never executed. */
object AscPage {
    const val HOST = "asc-ui.local"
    const val ORIGIN = "https://asc-ui.local"
    const val INDEX = "index.html"
    const val HOST_SCRIPT = "asc-host.js"

    /** Tool window tab title to the asc-ui view it mounts. */
    val VIEWS: Map<String, String> = linkedMapOf(
        "Stack" to "smartlog",
        "Graph" to "graph",
        "Review" to "review",
        "Inbox" to "inbox",
        "Actions" to "actions",
        "Affected" to "affected",
        "Worktrees" to "worktrees",
        "Search" to "search",
        "History" to "history",
    )

    const val CSP = "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; " +
        "connect-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"

    fun escapeAttr(s: String): String =
        s.replace("&", "&amp;").replace("\"", "&quot;").replace("<", "&lt;").replace(">", "&gt;")

    fun html(view: String, root: String, hasStyle: Boolean): String {
        val v = if (view in VIEWS.values) view else "smartlog"
        val style = if (hasStyle) "<link rel=\"stylesheet\" href=\"/index.css\">\n" else ""
        return """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${escapeAttr(CSP)}">
<meta name="viewport" content="width=device-width, initial-scale=1">
$style</head>
<body data-asc-view="${escapeAttr(v)}" data-asc-root="${escapeAttr(root)}" data-asc-host="jetbrains">
<div id="root"></div>
<script src="/$HOST_SCRIPT"></script>
<script type="module" src="/index.js"></script>
</body>
</html>
"""
    }

    /** `window.ascHost` for the page: `post(json)` goes to Kotlin, `deliver(json)` comes back. [inject] is the query call. */
    fun hostScript(inject: String): String = """(function(){
var ls=[];var theme=null;
window.ascHost={
post:function(s){$inject},
listen:function(f){ls.push(f);if(theme)f(theme);return function(){var i=ls.indexOf(f);if(i>=0)ls.splice(i,1);};},
deliver:function(s){try{if(JSON.parse(s).theme)theme=s;}catch(e){}for(var i=0;i<ls.length;i++){try{ls[i](s);}catch(e){}}}
};
})();
"""
}

/** Jar resource lookup for the bundle. Paths come from the page, so they are checked before use. */
object AscAssets {
    private val SAFE = Regex("^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$")
    private val MIME = mapOf(
        "js" to "text/javascript", "mjs" to "text/javascript", "css" to "text/css", "woff2" to "font/woff2",
        "png" to "image/png", "svg" to "image/svg+xml", "ico" to "image/x-icon", "map" to "application/json",
    )

    /** The resource name for a request path, or null when it is not a plain relative file name. */
    fun resolve(rawPath: String): String? {
        val p = rawPath.removePrefix("/")
        if (!SAFE.matches(p) || p.split("/").any { it == "." || it == ".." || it.startsWith(".") }) return null
        return if (MIME.containsKey(p.substringAfterLast('.', ""))) p else null
    }

    fun mime(name: String): String = MIME[name.substringAfterLast('.', "")] ?: "application/octet-stream"

    fun read(name: String): ByteArray? =
        AscAssets::class.java.getResourceAsStream("/asc-ui/$name")?.use { it.readBytes() }
}

/** Disposes the browser after the tool window has been hidden for [delayMs] (10 minutes in production). */
class IdleTimer(
    private val delayMs: Long,
    private val schedule: (Long, Runnable) -> AutoCloseable,
    private val onIdle: () -> Unit,
) {
    private var pending: AutoCloseable? = null

    @Synchronized fun hidden() {
        if (pending != null) return
        pending = schedule(delayMs) {
            synchronized(this) { pending = null }
            onIdle()
        }
    }

    @Synchronized fun shown() {
        pending?.close()
        pending = null
    }
}

/** `--asc-*` tokens from the running IDE (read on the EDT). */
object IdeThemeSource : ThemeSource {
    override fun color(key: String): Color? = when (key) {
        "FileStatus.ADDED" -> FileStatus.ADDED.color
        "FileStatus.DELETED" -> FileStatus.DELETED.color
        "FileStatus.MODIFIED" -> FileStatus.MODIFIED.color
        "FileStatus.MERGED_WITH_CONFLICTS" -> FileStatus.MERGED_WITH_CONFLICTS.color
        else -> UIManager.getColor(key)
    }

    override fun fontFamily(key: String): String? =
        if (key == "editor.font") EditorColorsManager.getInstance().globalScheme.editorFontName else UIManager.getFont(key)?.family

    override fun fontSizePx(key: String): Int? = UIManager.getFont(key)?.size

    override val dark: Boolean get() = !JBColor.isBright()
}

/** Serves the page: the generated index and host script, and bundle files from the jar. */
private class PageServer(val view: () -> String, val root: () -> String, val hostScript: () -> String) {
    fun serve(path: String): CefResourceHandler? {
        val p = path.substringBefore('?').removePrefix("/").ifEmpty { AscPage.INDEX }
        return when (p) {
            AscPage.INDEX -> bytes(200, "text/html", AscPage.html(view(), root(), AscAssets.read("index.css") != null).toByteArray())
            AscPage.HOST_SCRIPT -> bytes(200, "text/javascript", hostScript().toByteArray())
            else -> {
                val name = AscAssets.resolve(p) ?: return bytes(404, "text/plain", ByteArray(0))
                val data = AscAssets.read(name) ?: return bytes(404, "text/plain", ByteArray(0))
                bytes(200, AscAssets.mime(name), data)
            }
        }
    }

    private fun bytes(status: Int, mime: String, data: ByteArray): CefResourceHandler = BytesHandler(status, mime, data)
}

private class BytesHandler(val status: Int, val mime: String, val data: ByteArray) : CefResourceHandler {
    private val input = ByteArrayInputStream(data)

    override fun processRequest(request: CefRequest, callback: CefCallback): Boolean {
        callback.Continue()
        return true
    }

    override fun getResponseHeaders(response: CefResponse, length: IntRef, redirectUrl: StringRef) {
        response.mimeType = mime
        response.status = status
        response.setHeaderByName("X-Content-Type-Options", "nosniff", true)
        response.setHeaderByName("Cache-Control", "no-store", true)
        length.set(data.size)
    }

    override fun readResponse(out: ByteArray, toRead: Int, read: IntRef, callback: CefCallback): Boolean {
        val n = input.read(out, 0, toRead)
        if (n <= 0) {
            read.set(0)
            return false
        }
        read.set(n)
        return true
    }

    override fun cancel() = Unit
}

/** One scheme factory for the application; it finds the page by the `CefBrowser` that asks. */
private object AscScheme {
    private val pages = ConcurrentHashMap<CefBrowser, PageServer>()
    private val registered: Unit by lazy {
        JBCefApp.getInstance()
        CefApp.getInstance().registerSchemeHandlerFactory(
            "https",
            AscPage.HOST,
            CefSchemeHandlerFactory { browser, _, _, request ->
                val uri = try {
                    URI(request.url)
                } catch (_: Exception) {
                    return@CefSchemeHandlerFactory null
                }
                if (uri.scheme != "https" || uri.host != AscPage.HOST) return@CefSchemeHandlerFactory null
                pages[browser]?.serve(uri.rawPath ?: "/")
            },
        )
    }

    fun add(browser: CefBrowser, server: PageServer) {
        registered
        pages[browser] = server
    }

    fun remove(browser: CefBrowser) {
        pages.remove(browser)
    }
}

/** One asc-ui page in a JCEF browser, its relay, avatar fetch and host actions. */
class AscBrowser(private val project: Project, initialView: String, private val roots: List<String>) : Disposable {
    private val bridge = ApplicationManager.getApplication().getService(AscBridgeService::class.java)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val avatars = AscAvatar()
    private val browser: JBCefBrowser = JBCefBrowser.createBuilder().setOffScreenRendering(false).build()
    private val query = JBCefJSQuery.create(browser as JBCefBrowserBase)

    @Volatile private var view = initialView

    @Volatile private var infos: List<RootInfo> = emptyList()

    @Volatile private var disposed = false
    private val relay: AscRelay
    val component: JComponent get() = browser.component

    init {
        browser.setProperty(JBCefBrowserBase.Properties.NO_CONTEXT_MENU, true)
        val calls = AscCalls.of(bridge)
        val auth = BridgeAuth(bridge)
        val deps = RelayDeps(
            upstream = RelayUpstream { m, r, root -> bridge.stream(m, r, root) },
            allowlist = Allowlist.load(),
            send = this::deliver,
            roots = { roots },
            remoteRepos = { infos.map { it.repo }.filter { it.isNotEmpty() } },
            tmpDir = System.getProperty("java.io.tmpdir"),
            confirm = { title, targets -> confirm(title, targets) },
            hostApi = hostApiAdapters(
                forges = { infos.filter { it.repo.isNotEmpty() && it.remoteUrl.isNotEmpty() }.associate { it.repo to it.remoteUrl } },
                token = { host -> auth.token(host) },
            ),
            log = { e -> com.intellij.openapi.diagnostic.Logger.getInstance(AscBrowser::class.java).info("relay ${e.event}: ${e.reason} ${e.what ?: ""}") },
        )
        relay = AscRelay(scope, deps)
        val actions = HostActions(project, calls) { infos }
        query.addHandler { message ->
            route(message, actions)
            null
        }
        AscScheme.add(
            browser.cefBrowser,
            PageServer({ view }, { roots.firstOrNull() ?: "" }, { AscPage.hostScript(query.inject("s")) }),
        )
        browser.getJBCefClient().addRequestHandler(
            object : CefRequestHandlerAdapter() {
                override fun onBeforeBrowse(b: CefBrowser?, f: CefFrame?, r: CefRequest, gesture: Boolean, redirect: Boolean): Boolean {
                    if (isOurs(r.url)) return false
                    if (gesture) openExternalChecked(r.url)
                    return true
                }

                override fun getResourceRequestHandler(
                    b: CefBrowser?, f: CefFrame?, r: CefRequest?, nav: Boolean, download: Boolean, initiator: String?, disable: BoolRef?,
                ): CefResourceRequestHandler = object : CefResourceRequestHandlerAdapter() {
                    // Nothing is loaded from the network: cancel every request that is not the page's own origin.
                    override fun onBeforeResourceLoad(b: CefBrowser?, f: CefFrame?, r: CefRequest): Boolean = !isOurs(r.url)
                }
            },
            browser.cefBrowser,
        )
        browser.getJBCefClient().addLoadHandler(
            object : CefLoadHandlerAdapter() {
                override fun onLoadEnd(b: CefBrowser?, f: CefFrame?, status: Int) {
                    if (f?.isMain == true) pushTheme()
                }
            },
            browser.cefBrowser,
        )
        ApplicationManager.getApplication().messageBus.connect(this)
            .subscribe(LafManagerListener.TOPIC, LafManagerListener { pushTheme() })
        // The root's forge facts (repo, forge url) are needed by the relay's host API scope, so load after Hello.
        scope.launch {
            infos = try {
                bridge.hello(roots).rootsList
            } catch (_: Exception) {
                emptyList()
            }
            if (!disposed) loadView()
        }
    }

    private fun isOurs(url: String): Boolean = try {
        URI(url).let { it.scheme == "https" && it.host == AscPage.HOST }
    } catch (_: Exception) {
        false
    }

    private fun loadView() {
        browser.loadURL("${AscPage.ORIGIN}/${AscPage.INDEX}?view=$view")
    }

    fun select(newView: String) {
        if (newView == view || newView !in AscPage.VIEWS.values) return
        view = newView
        loadView()
    }

    /** Page to host. Avatar and host-action messages are handled here; everything else goes to the relay. */
    private fun route(message: String, actions: HostActions) {
        val o = try {
            JsonParser.parseString(message).takeIf { it.isJsonObject }?.asJsonObject
        } catch (_: Exception) {
            null
        }
        val id = o?.get("id")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isNumber }?.asLong
        if (o != null && id != null && id > 0) {
            if (o.has("avatar")) {
                val url = o.get("avatar").takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
                if (o.size() != 2 || url == null) return relay.handle("{}")
                scope.launch {
                    val reply = JsonObject()
                    reply.addProperty("id", id)
                    reply.add("avatar", avatars.get(url)?.let { JsonPrimitive(it) } ?: JsonNull.INSTANCE)
                    deliver(reply.toString())
                }
                return
            }
            if (o.has("host")) {
                val op = o.get("host")
                scope.launch {
                    val reply = JsonObject()
                    reply.addProperty("id", id)
                    reply.add("host", if (op.isJsonObject) actions.run(op.asJsonObject, roots) else JsonNull.INSTANCE)
                    deliver(reply.toString())
                }
                return
            }
        }
        relay.handle(message)
    }

    private fun deliver(json: String) {
        if (disposed) return
        val literal = com.google.gson.Gson().toJson(json)
        browser.cefBrowser.executeJavaScript("window.ascHost&&window.ascHost.deliver($literal)", "${AscPage.ORIGIN}/", 0)
    }

    private fun pushTheme() {
        ApplicationManager.getApplication().invokeLater { if (!disposed) deliver(AscTheme.message(IdeThemeSource)) }
    }

    private suspend fun confirm(title: String, targets: List<String>): Boolean = withContext(Dispatchers.EDT) {
        val text = (listOf(title) + targets.take(12) + (if (targets.size > 12) listOf("... and ${targets.size - 12} more") else emptyList()))
            .joinToString("\n")
        Messages.showYesNoDialog(project, text, "asc", "Continue", "Cancel", Messages.getQuestionIcon()) == Messages.YES
    }

    private fun openExternalChecked(url: String) {
        val u = try {
            URI(url)
        } catch (_: Exception) {
            return
        }
        if (u.scheme == "https" && u.host != null && u.userInfo == null) BrowserUtil.browse(u)
    }

    override fun dispose() {
        if (disposed) return
        disposed = true
        relay.close()
        scope.cancel()
        AscScheme.remove(browser.cefBrowser)
        Disposer.dispose(query)
        Disposer.dispose(browser)
    }
}

/** Host actions of `Platform` (openFile, openDiff, openExternal, notify, state). Every input is checked. */
internal class HostActions(private val project: Project, private val calls: AscCalls, private val infos: () -> List<RootInfo>) {
    companion object {
        private const val STATE_PREFIX = "asc.asc.ui.state."
        private val KEY = Regex("^[A-Za-z0-9._-]{1,64}$")
        private const val MAX_STATE = 64 * 1024
    }

    private fun str(o: JsonObject, k: String): String? = o.get(k)?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString

    /** A workspace-relative path below [root], or null. */
    fun inside(root: String, rel: String): Path? {
        if (rel.isEmpty() || rel.contains('\u0000')) return null
        return try {
            val base = Path.of(root).toAbsolutePath().normalize()
            val p = base.resolve(rel).normalize()
            if (p.startsWith(base) && p != base) p else null
        } catch (_: Exception) {
            null
        }
    }

    suspend fun run(op: JsonObject, roots: List<String>): JsonElement {
        val root = roots.firstOrNull() ?: return JsonNull.INSTANCE
        return try {
            when (str(op, "op")) {
                "openFile" -> {
                    val p = inside(root, str(op, "path") ?: return JsonNull.INSTANCE) ?: return JsonNull.INSTANCE
                    val line = op.get("line")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isNumber }?.asInt?.coerceIn(0, 10_000_000) ?: 0
                    withContext(Dispatchers.EDT) {
                        val vf = LocalFileSystem.getInstance().refreshAndFindFileByNioFile(p)
                        if (vf != null) OpenFileDescriptor(project, vf, maxOf(0, line - 1), 0).navigate(true)
                    }
                    JsonPrimitive(true)
                }
                "openDiff" -> {
                    val rel = str(op, "path") ?: return JsonNull.INSTANCE
                    val p = inside(root, rel) ?: return JsonNull.INSTANCE
                    val left = str(op, "leftRev") ?: return JsonNull.INSTANCE
                    val right = str(op, "rightRev")
                    if (!validRev(left) || (right != null && !validRev(right))) return JsonNull.INSTANCE
                    val ws = infos().firstOrNull { it.root == root }?.wsId ?: return JsonNull.INSTANCE
                    val leftText = AscContent.read(calls, root, ws, left, rel)?.let { String(it, Charsets.UTF_8) } ?: ""
                    val rightText = if (right != null) {
                        AscContent.read(calls, root, ws, right, rel)?.let { String(it, Charsets.UTF_8) } ?: ""
                    } else {
                        runCatching { java.nio.file.Files.readString(p) }.getOrDefault("")
                    }
                    withContext(Dispatchers.EDT) {
                        val type = FileTypeManager.getInstance().getFileTypeByFileName(p.fileName.toString())
                        val f = DiffContentFactory.getInstance()
                        DiffManager.getInstance().showDiff(
                            project,
                            SimpleDiffRequest(rel, f.create(project, leftText, type), f.create(project, rightText, type), left.take(12), right?.take(12) ?: "Working copy"),
                        )
                    }
                    JsonPrimitive(true)
                }
                "openExternal" -> {
                    val u = URI(str(op, "url") ?: return JsonNull.INSTANCE)
                    if (u.scheme == "https" && u.host != null && u.userInfo == null) BrowserUtil.browse(u)
                    JsonPrimitive(true)
                }
                "notify" -> {
                    val type = when (str(op, "severity")) {
                        "error" -> NotificationType.ERROR
                        "warning" -> NotificationType.WARNING
                        else -> NotificationType.INFORMATION
                    }
                    val msg = (str(op, "message") ?: "").take(500)
                    NotificationGroupManager.getInstance().getNotificationGroup("asc").createNotification(msg, type).notify(project)
                    JsonPrimitive(true)
                }
                "getState" -> {
                    val k = str(op, "key")?.takeIf { KEY.matches(it) } ?: return JsonNull.INSTANCE
                    PropertiesComponent.getInstance(project).getValue(STATE_PREFIX + k)?.let { JsonPrimitive(it) } ?: JsonNull.INSTANCE
                }
                "setState" -> {
                    val k = str(op, "key")?.takeIf { KEY.matches(it) } ?: return JsonNull.INSTANCE
                    val v = str(op, "value")?.takeIf { it.length <= MAX_STATE }
                    PropertiesComponent.getInstance(project).setValue(STATE_PREFIX + k, v)
                    JsonPrimitive(true)
                }
                else -> JsonNull.INSTANCE
            }
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (_: Exception) {
            JsonNull.INSTANCE
        }
    }

    private fun validRev(s: String) = s.length in 1..256 && !s.startsWith("-") && s.none { it.code < 0x20 || it.code == 0x7f }
}
