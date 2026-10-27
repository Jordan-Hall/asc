package asc.asc.vcs

import asc.v1.RepoOuterClass.LineHistoryRequest
import asc.v1.RepoOuterClass.LogEntry
import asc.v1.RepoOuterClass.LogRequest
import asc.v1.RepoOuterClass.LogResponse
import asc.v1.WorkspaceOuterClass.WsLogRequest
import com.google.protobuf.ByteString
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.history.VcsAbstractHistorySession
import com.intellij.openapi.vcs.history.VcsAppendableHistorySessionPartner
import com.intellij.openapi.vcs.history.VcsDependentHistoryComponents
import com.intellij.openapi.vcs.history.VcsFileRevision
import com.intellij.openapi.vcs.history.VcsHistoryProvider
import com.intellij.openapi.vcs.history.VcsHistorySession
import com.intellij.openapi.vcs.history.VcsRevisionNumber
import com.intellij.openapi.vfs.VirtualFile
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService
import java.util.Date
import javax.swing.JComponent
import kotlinx.coroutines.flow.toList

/**
 * One page of commits that touched a path. [nextToken] is the next page ("" when none). [truncated] means a
 * line-history scan budget ran out, and [resume] continues it.
 */
class AscHistoryPage(
    val entries: List<LogEntry>,
    val nextToken: String,
    val truncated: Boolean = false,
    val resume: String = "",
)

/** File and line history over the bridge (SPEC 24.5.1). Pure over [AscCalls]. */
object AscHistory {
    const val PAGE = 50

    private fun slashed(p: String) = p.replace('\\', '/').trim('/')

    /** A page of at most [PAGE] commits for [relPath], renames followed, starting at [pageToken]. */
    suspend fun page(calls: AscCalls, root: String, wsId: String, relPath: String, pageToken: String): AscHistoryPage {
        val q = LogRequest.newBuilder().addPaths(slashed(relPath)).setFollow(true).setLimit(PAGE)
        if (pageToken.isNotEmpty()) q.setPageToken(pageToken)
        val req = WsLogRequest.newBuilder().setWsId(wsId).setQuery(q).build()
        return collect(calls.stream("asc.v1.Workspace/Log", req.toByteArray(), root).toList())
    }

    /**
     * Commits that touched lines [startLine]..[endLine] (1-based, inclusive) of [relPath] at [rev] (blank = HEAD).
     * Follows `resume_token` while the scan budget truncates, up to [maxCalls] calls, then reports what it has
     * with `truncated` set.
     * `Repo.LineHistory` has no `Workspace` wrapper, so the repo ref is left empty for the local repo.
     */
    suspend fun lineHistory(
        calls: AscCalls,
        root: String,
        relPath: String,
        rev: String,
        startLine: Int,
        endLine: Int,
        maxCalls: Int = 8,
    ): AscHistoryPage {
        val all = ArrayList<LogEntry>()
        var token = ""
        var last = AscHistoryPage(emptyList(), "")
        repeat(maxCalls) {
            val req = LineHistoryRequest.newBuilder().setRev(rev)
                .setPath(ByteString.copyFromUtf8(slashed(relPath)))
                .setStartLine(startLine).setEndLine(endLine).setLimit(PAGE)
            if (token.isNotEmpty()) req.setResumeToken(token)
            last = collect(calls.stream("asc.v1.Repo/LineHistory", req.build().toByteArray(), root).toList())
            all += last.entries
            token = last.resume
            if (!last.truncated || token.isEmpty()) return AscHistoryPage(all, "")
        }
        return AscHistoryPage(all, "", truncated = last.truncated, resume = last.resume)
    }

    private fun collect(chunks: List<ByteArray>): AscHistoryPage {
        val entries = ArrayList<LogEntry>()
        var next = ""
        var truncated = false
        var resume = ""
        for (c in chunks) {
            val r = LogResponse.parseFrom(c)
            entries += r.entriesList
            if (r.nextPageToken.isNotEmpty()) next = r.nextPageToken
            if (r.truncated) truncated = true
            if (r.resumeToken.isNotEmpty()) resume = r.resumeToken
        }
        return AscHistoryPage(entries, next, truncated, resume)
    }
}

/** A commit as the history table shows it. Content loads lazily through `Workspace.ReadFile`. */
class AscFileRevision(
    private val entry: LogEntry,
    private val relPath: String,
    private val root: String,
    private val wsId: String,
    private val calls: AscCalls,
) : VcsFileRevision {
    private val number = AscRevisionNumber(
        entry.id.toByteArray().joinToString("") { "%02x".format(it) },
        entry.committer.time,
    )

    override fun getRevisionNumber(): VcsRevisionNumber = number

    override fun getBranchName(): String? = null

    override fun getRevisionDate(): Date = Date(entry.committer.time * 1000)

    override fun getAuthor(): String = entry.author.name.toStringUtf8()

    override fun getCommitMessage(): String = entry.message.toStringUtf8()

    override fun getChangedRepositoryPath() = null

    override fun loadContent(): ByteArray? =
        runBlockingCancellable { AscContent.read(calls, root, wsId, number.id, relPath) }

    override fun getContent(): ByteArray? = loadContent()
}

/**
 * File history in pages of 50 through `reportAppendableHistory`. History for a selection is [reportBlockHistory].
 */
class AscHistoryProvider(private val project: Project) : VcsHistoryProvider {
    private val bridge get() = service<AscBridgeService>()

    private class Ctx(val root: String, val rel: String, val wsId: String)

    private fun <T> guard(block: suspend () -> T): T =
        try {
            runBlockingCancellable { block() }
        } catch (e: BridgeException) {
            throw VcsException("ASC: ${e.message}")
        }

    private fun ctx(path: FilePath): Ctx {
        val root = ProjectLevelVcsManager.getInstance(project).getVcsRootFor(path)?.path
            ?: throw VcsException("ASC: ${path.path} is not under an ASC root")
        val rel = AscPaths.relative(root, path.path) ?: throw VcsException("ASC: ${path.path} is outside $root")
        val info = guard { bridge.hello(listOf(root)) }.rootsList.firstOrNull { it.wsId.isNotEmpty() }
            ?: throw VcsException("ASC: no workspace for $root")
        return Ctx(root, rel, info.wsId)
    }

    override fun reportAppendableHistory(path: FilePath, partner: VcsAppendableHistorySessionPartner) {
        val c = ctx(path)
        val calls = AscCalls.of(bridge)
        partner.reportCreatedEmptySession(AscHistorySession(emptyList()))
        var token = ""
        do {
            val page = guard { AscHistory.page(calls, c.root, c.wsId, c.rel, token) }
            for (e in page.entries) partner.acceptRevision(AscFileRevision(e, c.rel, c.root, c.wsId, calls))
            token = page.nextToken
        } while (token.isNotEmpty())
        partner.finished()
    }

    /** History for the selected lines (1-based, inclusive), newest first. A truncated scan is reported, not hidden. */
    fun reportBlockHistory(path: FilePath, startLine: Int, endLine: Int, partner: VcsAppendableHistorySessionPartner) {
        val c = ctx(path)
        val calls = AscCalls.of(bridge)
        partner.reportCreatedEmptySession(AscHistorySession(emptyList()))
        val page = guard { AscHistory.lineHistory(calls, c.root, c.rel, "", startLine, endLine) }
        for (e in page.entries) partner.acceptRevision(AscFileRevision(e, c.rel, c.root, c.wsId, calls))
        if (page.truncated) {
            partner.reportException(VcsException("ASC: the line history scan budget ran out; the list is partial"))
        }
        partner.finished()
    }

    override fun createSessionFor(filePath: FilePath): VcsHistorySession? = null

    override fun getUICustomization(session: VcsHistorySession, forShortcutComponent: JComponent) =
        VcsDependentHistoryComponents.createOnlyColumns(emptyArray())

    override fun getAdditionalActions(refresher: Runnable): Array<AnAction> = emptyArray()

    override fun isDateOmittable(): Boolean = false

    override fun getHelpId(): String? = null

    override fun supportsHistoryForDirectories(): Boolean = true

    override fun getHistoryDiffHandler() = null

    override fun canShowHistoryFor(file: VirtualFile): Boolean = true
}

private class AscHistorySession(revs: List<VcsFileRevision>) : VcsAbstractHistorySession(revs) {
    override fun calcCurrentRevisionNumber(): VcsRevisionNumber? = null

    override fun copyWithCachedRevision(): VcsHistorySession = AscHistorySession(revisionList)

    override fun isCurrentRevision(rev: VcsRevisionNumber): Boolean = false
}
