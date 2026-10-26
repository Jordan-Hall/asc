package asc.asc.vcs

import asc.v1.RepoOuterClass.ReadFileResponse
import asc.v1.WorkspaceOuterClass.WsFileStatus
import asc.v1.WorkspaceOuterClass.WsReadFileRequest
import asc.v1.WorkspaceOuterClass.WsRevPath
import asc.v1.WorkspaceOuterClass.WsStatusRequest
import asc.v1.WorkspaceOuterClass.WsStatusResponse
import com.google.protobuf.ByteString
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.ProcessCanceledException
import com.intellij.openapi.progress.ProgressIndicator
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.FileStatus
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.changes.Change
import com.intellij.openapi.vcs.changes.ChangeListManagerGate
import com.intellij.openapi.vcs.changes.ChangeProvider
import com.intellij.openapi.vcs.changes.ChangelistBuilder
import com.intellij.openapi.vcs.changes.ContentRevision
import com.intellij.openapi.vcs.changes.CurrentContentRevision
import com.intellij.openapi.vcs.changes.VcsDirtyScope
import com.intellij.openapi.vcs.history.VcsRevisionNumber
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.vcsUtil.VcsUtil
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService
import java.io.ByteArrayOutputStream
import java.nio.charset.StandardCharsets
import java.nio.file.Path
import java.nio.file.Paths
import kotlinx.coroutines.flow.toList

/** The kind of working-copy change `Workspace.Status` reports for one file, from its porcelain v2 `XY`. */
enum class ChangeKind { ADDED, MODIFIED, DELETED, RENAMED, CONFLICT, UNTRACKED }

/** Pure mapping from `Workspace.Status` to IDE change kinds; no platform types, so tests run without an IDE. */
object StatusMapping {
    private val conflicts = setOf("DD", "AU", "UD", "UA", "DU", "AA", "UU")

    /**
     * XY compares HEAD with the index (X) and the index with the work tree (Y). The IDE shows HEAD against the
     * work tree, so the two letters are folded into one kind. A file added and then deleted again is no change.
     */
    fun classify(xy: String): ChangeKind? {
        if (xy == "??") return ChangeKind.UNTRACKED
        if (xy == "!!") return null // ignored files are never requested; the bridge refuses `ignored`
        if (xy in conflicts) return ChangeKind.CONFLICT
        val x = xy.getOrElse(0) { '.' }
        val y = xy.getOrElse(1) { '.' }
        fun none(c: Char) = c == '.' || c == ' '
        return when {
            none(x) && none(y) -> null
            x == 'A' && y == 'D' -> null
            y == 'D' || (x == 'D' && none(y)) -> ChangeKind.DELETED
            x == 'A' || x == 'C' -> ChangeKind.ADDED
            x == 'R' || y == 'R' -> ChangeKind.RENAMED
            else -> ChangeKind.MODIFIED
        }
    }

    /** [rel] as a path under [root], or null when it is absolute, empty, contains NUL or escapes the root. */
    fun resolve(root: Path, rel: ByteString): Path? {
        val s = rel.toString(StandardCharsets.UTF_8)
        if (s.isEmpty() || s.indexOf(Char(0)) >= 0 || s.startsWith("/") || s.startsWith("\\")) return null
        val p = try {
            Paths.get(s.replace('\\', '/'))
        } catch (_: Exception) {
            return null
        }
        if (p.isAbsolute) return null
        val r = root.normalize()
        val out = r.resolve(p).normalize()
        return if (out.startsWith(r) && out != r) out else null
    }

    /**
     * The paths for a `WsStatusRequest`: an empty list (the whole work tree) when the scope covers the root,
     * null when nothing in the scope lies under the root.
     */
    fun scopePaths(root: Path, recursive: Collection<Path>, files: Collection<Path>): List<String>? {
        val r = root.normalize()
        if (recursive.any { it.normalize() == r }) return emptyList()
        val out = (recursive + files).map { it.normalize() }.filter { it.startsWith(r) && it != r }
            .map { r.relativize(it).toString().replace('\\', '/') }.distinct()
        return if (out.isEmpty()) null else out
    }
}

/** HEAD content of a file through `Workspace.ReadFile`. TODO(AscContentRevision): replaced by the shared revision class. */
class AscHeadRevision(private val root: Path, private val file: FilePath, private val relPath: String) : ContentRevision {
    override fun getFile(): FilePath = file

    override fun getRevisionNumber(): VcsRevisionNumber = HeadNumber

    override fun getContent(): String? {
        val req = WsReadFileRequest.newBuilder().setRevPath(
            WsRevPath.newBuilder().setPath(ByteString.copyFromUtf8(relPath)),
        ).build().toByteArray()
        val bytes = try {
            runBlockingCancellable {
                val out = ByteArrayOutputStream()
                service<AscBridgeService>().stream("asc.v1.Workspace/ReadFile", req, root.toString()).toList().forEach {
                    out.write(ReadFileResponse.parseFrom(it).data.toByteArray())
                }
                out.toByteArray()
            }
        } catch (e: ProcessCanceledException) {
            throw e
        } catch (_: BridgeException) {
            throw VcsException("ASC: cannot read the HEAD version of $relPath")
        }
        return String(bytes, file.charset)
    }

    private object HeadNumber : VcsRevisionNumber {
        override fun asString(): String = "HEAD"
        override fun compareTo(other: VcsRevisionNumber): Int = if (other === this) 0 else -1
    }
}

class AscChangeProvider(private val project: Project, private val vcs: AscVcs) : ChangeProvider {
    override fun getChanges(
        dirtyScope: VcsDirtyScope,
        builder: ChangelistBuilder,
        progress: ProgressIndicator,
        addGate: ChangeListManagerGate,
    ) {
        for (vf in dirtyScope.affectedContentRoots) {
            val root = vf.toNioPathOrNull() ?: continue
            if (!AscRoots.isAscRoot(root)) continue
            val recursive = dirtyScope.recursivelyDirtyDirectories.map { Paths.get(it.path) }
            val files = dirtyScope.dirtyFiles.map { Paths.get(it.path) }
            val paths = StatusMapping.scopePaths(root, recursive, files) ?: continue
            progress.checkCanceled()
            val resp = status(root, paths) ?: continue
            report(root, resp.filesList, builder)
        }
    }

    private fun status(root: Path, paths: List<String>): WsStatusResponse? {
        val req = WsStatusRequest.newBuilder().setUntracked(true)
            .addAllPaths(paths.map { ByteString.copyFromUtf8(it) }).build().toByteArray()
        return try {
            runBlockingCancellable {
                WsStatusResponse.parseFrom(service<AscBridgeService>().unary("asc.v1.Workspace/Status", req, root.toString()))
            }
        } catch (e: ProcessCanceledException) {
            throw e
        } catch (_: BridgeException) {
            null // the bridge is down; the next dirty event retries
        }
    }

    private fun report(root: Path, files: List<WsFileStatus>, builder: ChangelistBuilder) {
        for (f in files) {
            val kind = StatusMapping.classify(f.xy) ?: continue
            val abs = StatusMapping.resolve(root, f.path) ?: continue
            val after = VcsUtil.getFilePath(abs.toFile(), false)
            val rel = root.relativize(abs).toString().replace('\\', '/')
            when (kind) {
                ChangeKind.UNTRACKED -> builder.processUnversionedFile(after)
                ChangeKind.ADDED ->
                    builder.processChange(Change(null, CurrentContentRevision(after), FileStatus.ADDED), AscVcs.KEY)
                ChangeKind.DELETED ->
                    builder.processChange(Change(AscHeadRevision(root, after, rel), null, FileStatus.DELETED), AscVcs.KEY)
                ChangeKind.CONFLICT -> builder.processChange(
                    Change(AscHeadRevision(root, after, rel), CurrentContentRevision(after), FileStatus.MERGED_WITH_CONFLICTS),
                    AscVcs.KEY,
                )
                ChangeKind.RENAMED, ChangeKind.MODIFIED -> {
                    val origAbs = if (f.origPath.isEmpty) null else StatusMapping.resolve(root, f.origPath)
                    val beforePath = origAbs?.let { VcsUtil.getFilePath(it.toFile(), false) } ?: after
                    val beforeRel = origAbs?.let { root.relativize(it).toString().replace('\\', '/') } ?: rel
                    builder.processChange(
                        Change(AscHeadRevision(root, beforePath, beforeRel), CurrentContentRevision(after), FileStatus.MODIFIED),
                        AscVcs.KEY,
                    )
                }
            }
        }
    }

    override fun isModifiedDocumentTrackingRequired(): Boolean = true

    override fun doCleanup(files: MutableList<VirtualFile>) {}
}

private fun VirtualFile.toNioPathOrNull(): Path? = try {
    Paths.get(path)
} catch (_: Exception) {
    null
}
