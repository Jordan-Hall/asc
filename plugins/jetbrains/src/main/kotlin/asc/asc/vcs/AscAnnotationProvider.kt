package asc.asc.vcs

import asc.v1.Repo.BlameResponse
import asc.v1.Workspace.WsBlameRequest
import com.google.protobuf.ByteString
import com.intellij.openapi.components.service
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.annotate.AnnotationProvider
import com.intellij.openapi.vcs.annotate.FileAnnotation
import com.intellij.openapi.vcs.annotate.LineAnnotationAspect
import com.intellij.openapi.vcs.annotate.LineAnnotationAspectAdapter
import com.intellij.openapi.vcs.history.VcsFileRevision
import com.intellij.openapi.vcs.history.VcsRevisionNumber
import com.intellij.openapi.vfs.VirtualFile
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService
import java.text.DateFormat
import java.util.Date
import kotlinx.coroutines.flow.toList

/** What the gutter and tooltip show for one line. A null entry in [AscBlame.load] is an uncommitted line. */
data class AscLineInfo(
    val revision: AscRevisionNumber,
    val author: String,
    val email: String,
    val time: Long,
    val subject: String,
    val change: String,
    val review: String,
)

/** `Workspace.Blame` as one entry per line (SPEC 24.5.1). Pure over [AscCalls], so tests need no IDE. */
object AscBlame {
    private fun hex(b: ByteString): String = b.toByteArray().joinToString("") { "%02x".format(it) }

    /**
     * Blames [relPath]. A non-null [buffer] (the unsaved editor bytes) is sent with an empty rev, so the daemon
     * maps its lines onto HEAD; lines it cannot map come back as null. Returns one slot per line, 0-based.
     */
    suspend fun load(
        calls: AscCalls,
        root: String,
        wsId: String,
        relPath: String,
        buffer: ByteArray?,
        lineCount: Int,
    ): List<AscLineInfo?> {
        val req = WsBlameRequest.newBuilder().setWsId(wsId).setRev("")
            .setPath(ByteString.copyFromUtf8(relPath.replace('\\', '/').trim('/')))
        if (buffer != null) req.setBuffer(ByteString.copyFrom(buffer))
        val lines = arrayOfNulls<AscLineInfo>(lineCount)
        for (chunk in calls.stream("asc.v1.Workspace/Blame", req.build().toByteArray(), root).toList()) {
            val resp = BlameResponse.parseFrom(chunk)
            val byId = HashMap<String, AscLineInfo>()
            for ((i, c) in resp.commitsList.withIndex()) {
                val review = resp.reviewsList.getOrNull(i)?.takeIf { it.number != 0L }
                val id = hex(c.id)
                byId[id] = AscLineInfo(
                    revision = AscRevisionNumber(id, c.committer.time),
                    author = c.author.name.toStringUtf8(),
                    email = c.author.email.toStringUtf8(),
                    time = c.committer.time,
                    subject = c.message.toStringUtf8().lineSequence().firstOrNull().orEmpty(),
                    change = if (c.change.isEmpty) "" else hex(c.change),
                    review = review?.let { "#${it.number}" }.orEmpty(),
                )
            }
            for (h in resp.hunksList) {
                if (h.origin.isEmpty) continue // uncommitted
                val info = byId[hex(h.origin)] ?: continue
                for (n in h.start until h.start + h.len) { // start is 1-based
                    if (n >= 1 && n <= lineCount) lines[n - 1] = info
                }
            }
        }
        return lines.toList()
    }

    /** Number of lines in [text] as an editor counts them. */
    fun lineCount(text: String): Int =
        if (text.isEmpty()) 0 else text.count { it == '\n' } + if (text.endsWith('\n')) 0 else 1

    fun tooltip(i: AscLineInfo?): String? {
        i ?: return null
        return buildString {
            append(i.revision.id.take(12)).append(' ').append(i.author)
            if (i.email.isNotEmpty()) append(" <").append(i.email).append('>')
            append('\n').append(i.subject)
            if (i.change.isNotEmpty()) append("\nchange ").append(i.change.take(12))
            if (i.review.isNotEmpty()) append("\nreview ").append(i.review)
        }
    }
}

/** The annotation of one file: revision, author and date columns, with change and review in the tooltip. */
class AscFileAnnotation(
    project: Project,
    private val file: VirtualFile,
    private val lines: List<AscLineInfo?>,
    private val text: String,
    private val head: AscRevisionNumber?,
) : FileAnnotation(project) {
    private fun info(line: Int) = lines.getOrNull(line)

    override fun getToolTip(line: Int): String? = AscBlame.tooltip(info(line))

    override fun getHtmlToolTip(line: Int): String? =
        getToolTip(line)?.replace("&", "&amp;")?.replace("<", "&lt;")?.replace(">", "&gt;")?.replace("\n", "<br/>")

    override fun getAnnotatedContent(): String = text

    override fun getFile(): VirtualFile = file

    override fun getCurrentRevision(): VcsRevisionNumber? = head

    override fun getRevisions(): List<VcsFileRevision>? = null

    override fun getLineRevisionNumber(lineNumber: Int): VcsRevisionNumber? = info(lineNumber)?.revision

    override fun getLineDate(lineNumber: Int): Date? = info(lineNumber)?.let { Date(it.time * 1000) }

    override fun getAspects(): Array<LineAnnotationAspect> = arrayOf(
        aspect(LineAnnotationAspect.REVISION) { it.revision.id.take(8) },
        aspect(LineAnnotationAspect.AUTHOR) { it.author },
        aspect(LineAnnotationAspect.DATE) { DateFormat.getDateInstance(DateFormat.SHORT).format(Date(it.time * 1000)) },
    )

    private fun aspect(id: String, value: (AscLineInfo) -> String): LineAnnotationAspect =
        object : LineAnnotationAspectAdapter(id, true) {
            override fun getValue(line: Int): String = info(line)?.let(value).orEmpty()

            override fun getTooltipText(line: Int): String? = AscBlame.tooltip(info(line))

            override fun getRevisionNumber(line: Int): VcsRevisionNumber? = info(line)?.revision
        }

    override fun dispose() {}
}

/** `AnnotationProvider` over `Workspace.Blame`, sending the unsaved buffer when the editor is dirty. */
class AscAnnotationProvider(private val project: Project) : AnnotationProvider {
    private val bridge get() = service<AscBridgeService>()

    override fun isAnnotationValid(rev: VcsFileRevision): Boolean = true

    override fun annotate(file: VirtualFile): FileAnnotation {
        val root = ProjectLevelVcsManager.getInstance(project).getVcsRootFor(file)?.path
            ?: throw VcsException("asc: ${file.path} is not under an asc root")
        val rel = AscPaths.relative(root, file.path) ?: throw VcsException("asc: ${file.path} is outside $root")
        val docs = FileDocumentManager.getInstance()
        val doc = docs.getCachedDocument(file)
        val dirty = doc != null && docs.isDocumentUnsaved(doc)
        val text = doc?.text ?: String(file.contentsToByteArray(), file.charset)
        val buffer = if (dirty) text.toByteArray(file.charset) else null
        val calls = AscCalls.of(bridge)
        try {
            return runBlockingCancellable {
                val info = bridge.hello(listOf(root)).rootsList.firstOrNull { it.wsId.isNotEmpty() }
                    ?: throw VcsException("asc: no workspace for $root")
                val lines = AscBlame.load(calls, root, info.wsId, rel, buffer, AscBlame.lineCount(text))
                AscFileAnnotation(project, file, lines, text, AscContent.head(calls, root, info.wsId))
            }
        } catch (e: BridgeException) {
            throw VcsException("asc: ${e.message}")
        }
    }
}
