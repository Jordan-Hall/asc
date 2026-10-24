package asc.asc.vcs

import asc.v1.Repo.LogRequest
import asc.v1.Repo.LogResponse
import asc.v1.Repo.ReadFileResponse
import asc.v1.Workspace.WsLogRequest
import asc.v1.Workspace.WsReadFileRequest
import asc.v1.Workspace.WsRevPath
import asc.v1.Workspace.WsStatusRequest
import asc.v1.Workspace.WsStatusResponse
import com.google.protobuf.ByteString
import com.intellij.openapi.vcs.FilePath
import com.intellij.openapi.vcs.changes.ByteBackedContentRevision
import com.intellij.openapi.vcs.history.VcsRevisionNumber
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService
import java.io.ByteArrayOutputStream
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.toList

/** gRPC NOT_FOUND: the path does not exist at that revision. */
private const val CODE_NOT_FOUND = 5

/** The bridge calls the content layer needs; the production adapter is [AscBridgeService], tests pass a fake. */
interface AscCalls {
    suspend fun unary(method: String, request: ByteArray, root: String): ByteArray

    suspend fun stream(method: String, request: ByteArray, root: String): Flow<ByteArray>

    companion object {
        fun of(service: AscBridgeService): AscCalls = object : AscCalls {
            override suspend fun unary(method: String, request: ByteArray, root: String) =
                service.unary(method, request, root)

            override suspend fun stream(method: String, request: ByteArray, root: String) =
                service.stream(method, request, root)
        }
    }
}

/** An asc revision: the lowercase hex change-commit id. [time] (epoch seconds, 0 when unknown) only orders revisions. */
class AscRevisionNumber(val id: String, val time: Long = 0) : VcsRevisionNumber {
    override fun asString(): String = id

    override fun compareTo(other: VcsRevisionNumber): Int =
        if (other is AscRevisionNumber && time != 0L && other.time != 0L) {
            time.compareTo(other.time)
        } else {
            id.compareTo(other.asString())
        }

    override fun equals(other: Any?): Boolean = other is AscRevisionNumber && other.id == id

    override fun hashCode(): Int = id.hashCode()

    override fun toString(): String = id
}

/** Reads of committed file content and revision ids through the bridge (`Workspace.ReadFile`, `Log`, `Status`). */
object AscContent {
    private fun slashed(relPath: String): String = relPath.replace('\\', '/').trim('/')

    private fun hex(b: ByteString): String = b.toByteArray().joinToString("") { "%02x".format(it) }

    /** The file at [rev] (blank = HEAD), or null when it does not exist there. Never reads the work tree. */
    suspend fun read(calls: AscCalls, root: String, wsId: String, rev: String, relPath: String): ByteArray? {
        val req = WsReadFileRequest.newBuilder().setWsId(wsId)
            .setRevPath(WsRevPath.newBuilder().setRev(rev).setPath(ByteString.copyFromUtf8(slashed(relPath))))
            .build()
        return try {
            val out = ByteArrayOutputStream()
            for (chunk in calls.stream("asc.v1.Workspace/ReadFile", req.toByteArray(), root).toList()) {
                out.write(ReadFileResponse.parseFrom(chunk).data.toByteArray())
            }
            out.toByteArray()
        } catch (e: BridgeException) {
            if (e.code == CODE_NOT_FOUND) null else throw e
        }
    }

    /** The commit the work tree is based on (`Status.head`), or null for an unborn head. */
    suspend fun head(calls: AscCalls, root: String, wsId: String): AscRevisionNumber? {
        val req = WsStatusRequest.newBuilder().setWsId(wsId).build()
        val head = WsStatusResponse.parseFrom(calls.unary("asc.v1.Workspace/Status", req.toByteArray(), root)).head
        return if (head.isEmpty) null else AscRevisionNumber(hex(head))
    }

    /** The newest commit that touched [relPath] (renames followed), or null when none did. */
    suspend fun lastChange(calls: AscCalls, root: String, wsId: String, relPath: String): AscRevisionNumber? {
        val query = LogRequest.newBuilder().addPaths(slashed(relPath)).setFollow(true).setLimit(1).build()
        val req = WsLogRequest.newBuilder().setWsId(wsId).setQuery(query).build()
        for (chunk in calls.stream("asc.v1.Workspace/Log", req.toByteArray(), root).toList()) {
            val e = LogResponse.parseFrom(chunk).entriesList.firstOrNull() ?: continue
            return AscRevisionNumber(hex(e.id), e.committer.time)
        }
        return null
    }
}

/** Committed content of [file] at [revision], loaded lazily and once. A missing file reads as null (an add or a delete). */
class AscContentRevision(
    private val file: FilePath,
    private val revision: AscRevisionNumber,
    private val relPath: String,
    private val root: String,
    private val wsId: String,
    private val calls: AscCalls,
) : ByteBackedContentRevision {
    private val bytes: ByteArray? by lazy {
        com.intellij.openapi.progress.runBlockingCancellable {
            AscContent.read(calls, root, wsId, revision.id, relPath)
        }
    }

    override fun getFile(): FilePath = file

    override fun getRevisionNumber(): VcsRevisionNumber = revision

    override fun getContentAsBytes(): ByteArray? = bytes

    override fun getContent(): String? = bytes?.let { String(it, file.charset) }
}
