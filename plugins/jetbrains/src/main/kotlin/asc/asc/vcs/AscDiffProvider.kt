package asc.asc.vcs

import com.intellij.openapi.components.service
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.changes.ContentRevision
import com.intellij.openapi.vcs.diff.DiffProvider
import com.intellij.openapi.vcs.diff.ItemLatestState
import com.intellij.openapi.vcs.history.VcsRevisionNumber
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.vcsUtil.VcsUtil
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService

/** Pure path helpers, testable without a project. */
object AscPaths {
    /** [file] relative to [root] with `/` separators, or null when it is outside the root. */
    fun relative(root: String, file: String): String? {
        val r = root.replace('\\', '/').trimEnd('/')
        val f = file.replace('\\', '/')
        if (f == r) return ""
        return if (f.startsWith("$r/")) f.substring(r.length + 1) else null
    }
}

/**
 * Revision queries for the platform's diff and "compare with" actions (SPEC 24.5.1). Revisions are read
 * with `Workspace.ReadFile` at the hex commit id, so the work tree is never consulted. The platform calls
 * these on background threads; a bridge failure surfaces as a [VcsException].
 */
class AscDiffProvider(private val project: Project) : DiffProvider {
    private val bridge get() = service<AscBridgeService>()

    private class Ctx(val root: String, val rel: String, val wsId: String)

    private fun <T> guard(block: suspend () -> T): T =
        try {
            runBlockingCancellable { block() }
        } catch (e: BridgeException) {
            throw VcsException("ASC: ${e.message}")
        }

    private fun ctx(path: FilePath): Ctx? {
        val root = ProjectLevelVcsManager.getInstance(project).getVcsRootFor(path)?.path ?: return null
        val rel = AscPaths.relative(root, path.path) ?: return null
        val info = guard { bridge.hello(listOf(root)) }.rootsList.firstOrNull { it.wsId.isNotEmpty() } ?: return null
        return Ctx(root, rel, info.wsId)
    }

    override fun getCurrentRevision(file: VirtualFile): VcsRevisionNumber? {
        val c = ctx(VcsUtil.getFilePath(file)) ?: return null
        return guard { AscContent.head(AscCalls.of(bridge), c.root, c.wsId) }
    }

    override fun getLastRevision(file: VirtualFile): ItemLatestState? = getLastRevision(VcsUtil.getFilePath(file))

    override fun getLastRevision(filePath: FilePath): ItemLatestState? {
        val c = ctx(filePath) ?: return null
        val calls = AscCalls.of(bridge)
        return guard {
            val last = AscContent.lastChange(calls, c.root, c.wsId, c.rel)
            val exists = AscContent.read(calls, c.root, c.wsId, "", c.rel) != null
            val head = AscContent.head(calls, c.root, c.wsId)
            last?.let { ItemLatestState(it, exists, head == null || head == it) }
        }
    }

    override fun getLatestCommittedRevision(file: VirtualFile): VcsRevisionNumber? = getLastRevision(file)?.number

    override fun createFileContent(revisionNumber: VcsRevisionNumber, selectedFile: VirtualFile): ContentRevision? {
        val path = VcsUtil.getFilePath(selectedFile)
        val c = ctx(path) ?: return null
        val rev = revisionNumber as? AscRevisionNumber ?: AscRevisionNumber(revisionNumber.asString())
        return AscContentRevision(path, rev, c.rel, c.root, c.wsId, AscCalls.of(bridge))
    }
}
