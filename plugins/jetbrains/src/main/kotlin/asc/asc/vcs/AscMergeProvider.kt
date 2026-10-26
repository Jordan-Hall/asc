package asc.asc.vcs

import asc.v1.Workspace.WsConflictsRequest
import asc.v1.Workspace.WsConflictsResponse
import asc.v1.Workspace.WsReadFileRequest
import asc.v1.Workspace.WsStageRequest
import asc.v1.Workspace.WsStagePath
import asc.v1.Repo.ConflictKind
import com.google.protobuf.ByteString
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vcs.VcsException
import com.intellij.openapi.vcs.merge.MergeData
import com.intellij.openapi.vcs.merge.MergeProvider
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.vcsUtil.VcsUtil
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService
import java.io.ByteArrayOutputStream
import kotlinx.coroutines.flow.toList

/** Plain bytes of the three sides of a conflicted file; an absent side (add/add has no base) is empty. */
class AscMergeSides(val base: ByteArray, val ours: ByteArray, val theirs: ByteArray, val binary: Boolean)

/** The bridge-level merge operations, testable without a project (SPEC 24.5.1). */
object AscMerge {
    private const val CODE_NOT_FOUND = 5

    private fun slashed(relPath: String): String = relPath.replace('\', '/').trim('/')

    private suspend fun stage(calls: AscCalls, root: String, wsId: String, rel: String, n: Int): ByteArray {
        val req = WsReadFileRequest.newBuilder().setWsId(wsId)
            .setStage(WsStagePath.newBuilder().setStage(n).setPath(ByteString.copyFromUtf8(rel)))
            .build()
        return try {
            val out = ByteArrayOutputStream()
            for (chunk in calls.stream("asc.v1.Workspace/ReadFile", req.toByteArray(), root).toList()) {
                out.write(asc.v1.Repo.ReadFileResponse.parseFrom(chunk).data.toByteArray())
            }
            out.toByteArray()
        } catch (e: BridgeException) {
            if (e.code == CODE_NOT_FOUND) ByteArray(0) else throw e
        }
    }

    /** The conflict entry for [relPath] from `Workspace.Conflicts`, or null when the path is not conflicted. */
    suspend fun conflict(calls: AscCalls, root: String, wsId: String, relPath: String): asc.v1.Workspace.WsConflict? {
        val rel = slashed(relPath)
        val req = WsConflictsRequest.newBuilder().setWsId(wsId).build()
        val resp = WsConflictsResponse.parseFrom(calls.unary("asc.v1.Workspace/Conflicts", req.toByteArray(), root))
        return resp.conflictsList.firstOrNull { it.path.toStringUtf8() == rel }
    }

    /** Stages 1 (base), 2 (ours) and 3 (theirs) of a conflicted path, read from the local store only. */
    suspend fun load(calls: AscCalls, root: String, wsId: String, relPath: String): AscMergeSides {
        val rel = slashed(relPath)
        val c = conflict(calls, root, wsId, rel) ?: throw VcsException("ASC: $rel is not in conflict")
        return AscMergeSides(
            base = stage(calls, root, wsId, rel, 1),
            ours = stage(calls, root, wsId, rel, 2),
            theirs = stage(calls, root, wsId, rel, 3),
            binary = c.kind == ConflictKind.CONFLICT_KIND_BINARY,
        )
    }

    /** Marks [relPath] resolved by staging the merged work-tree file. */
    suspend fun resolved(calls: AscCalls, root: String, wsId: String, relPath: String) {
        val req = WsStageRequest.newBuilder().setWsId(wsId).addAdd(ByteString.copyFromUtf8(slashed(relPath))).build()
        calls.unary("asc.v1.Workspace/Stage", req.toByteArray(), root)
    }

    /** MergeData.ORIGINAL = base, CURRENT = ours, LAST = theirs. */
    fun toMergeData(s: AscMergeSides): MergeData = MergeData().also {
        it.ORIGINAL = s.base
        it.CURRENT = s.ours
        it.LAST = s.theirs
    }
}

/** `MergeProvider` over `Workspace.Conflicts`, `ReadFile` stages 1-3 and `Stage` (SPEC 24.5.1). */
class AscMergeProvider(private val project: Project) : MergeProvider {
    private val bridge get() = service<AscBridgeService>()

    private class Ctx(val root: String, val rel: String, val wsId: String)

    private fun <T> guard(block: suspend () -> T): T =
        try {
            runBlockingCancellable { block() }
        } catch (e: BridgeException) {
            throw VcsException("ASC: ${e.message}")
        }

    private fun ctx(file: VirtualFile): Ctx {
        val path = VcsUtil.getFilePath(file)
        val root = ProjectLevelVcsManager.getInstance(project).getVcsRootFor(path)?.path
            ?: throw VcsException("ASC: ${file.path} is not under an ASC root")
        val rel = AscPaths.relative(root, path.path) ?: throw VcsException("ASC: ${file.path} is outside $root")
        val info = guard { bridge.hello(listOf(root)) }.rootsList.firstOrNull { it.wsId.isNotEmpty() }
            ?: throw VcsException("ASC: no workspace for $root")
        return Ctx(root, rel, info.wsId)
    }

    override fun loadRevisions(file: VirtualFile): MergeData {
        val c = ctx(file)
        return AscMerge.toMergeData(guard { AscMerge.load(AscCalls.of(bridge), c.root, c.wsId, c.rel) })
    }

    override fun conflictResolvedForFile(file: VirtualFile) {
        val c = ctx(file)
        guard { AscMerge.resolved(AscCalls.of(bridge), c.root, c.wsId, c.rel) }
    }

    override fun isBinary(file: VirtualFile): Boolean = file.fileType.isBinary
}
