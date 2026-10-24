package asc.asc.vcs

import asc.v1.WorkspaceOuterClass.WatchEvent
import asc.v1.WorkspaceOuterClass.WatchRequest
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.ProjectActivity
import com.intellij.openapi.vcs.changes.VcsDirtyScopeManager
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.util.messages.Topic
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService

import java.nio.file.Paths
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.channelFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** What a batch of `Watch` events makes dirty: the whole root, or these root-relative paths. */
data class DirtyPlan(val everything: Boolean, val paths: Set<String>) {
    fun merge(o: DirtyPlan): DirtyPlan {
        val all = everything || o.everything || paths.size + o.paths.size > MAX_PATHS
        return if (all) EVERYTHING else DirtyPlan(false, paths + o.paths)
    }

    val isEmpty get() = !everything && paths.isEmpty()

    companion object {
        const val MAX_PATHS = 1000
        val NOTHING = DirtyPlan(false, emptySet())
        val EVERYTHING = DirtyPlan(true, emptySet())
    }
}

/** Pure `Watch` handling: event kind to dirty plan, coalescing and token tracking. */
object WatchPlan {
    /** Window in which events are merged (SPEC 24.4.12: `Watch` 50 ms). */
    const val COALESCE_MS = 50L

    fun plan(e: WatchEvent): DirtyPlan = when (e.kind) {
        WatchEvent.Kind.KIND_PATHS ->
            if (e.pathsCount == 0) DirtyPlan.NOTHING
            else DirtyPlan(false, e.pathsList.map { it.toStringUtf8() }.toSet())
        // HEAD, staging, a finished op, a switch or a resync can change the status of any file.
        WatchEvent.Kind.KIND_HEAD, WatchEvent.Kind.KIND_STAGE, WatchEvent.Kind.KIND_OP_FINISHED,
        WatchEvent.Kind.KIND_SWITCH_DONE, WatchEvent.Kind.KIND_RESYNC,
        -> DirtyPlan.EVERYTHING
        // Other refs, started ops, stack and cloud events do not change file status.
        else -> DirtyPlan.NOTHING
    }

    /** Merges events that arrive within [windowMs] of the first into one plan; empty plans are dropped. */
    fun coalesce(events: Flow<WatchEvent>, windowMs: Long = COALESCE_MS): Flow<DirtyPlan> = channelFlow {
        val lock = Mutex()
        var acc = DirtyPlan.NOTHING
        var timer: Job? = null
        events.collect { e ->
            val p = plan(e)
            if (p.isEmpty) return@collect
            lock.withLock {
                acc = acc.merge(p)
                if (timer == null) {
                    timer = launch {
                        delay(windowMs)
                        lock.withLock {
                            val out = acc
                            acc = DirtyPlan.NOTHING
                            timer = null
                            send(out)
                        }
                    }
                }
            }
        }
    }
}

/** Published on the project bus after each batch of `Watch` events for a root. */
fun interface AscStateListener {
    fun stateChanged(root: String, plan: DirtyPlan)

    companion object {
        @JvmField
        val ASC_STATE: Topic<AscStateListener> = Topic.create("asc state", AscStateListener::class.java)
    }
}

/**
 * Subscribes to `Workspace.Watch` for each asc root and turns events into `VcsDirtyScopeManager` dirty marks.
 * After a bridge restart it resubscribes from the last token, with backoff.
 */
@Service(Service.Level.PROJECT)
class AscWatchService(private val project: Project, private val scope: CoroutineScope) {
    private val jobs = HashMap<String, Job>()

    @Synchronized
    fun watch(root: String) {
        if (jobs[root]?.isActive == true) return
        jobs[root] = scope.launch { run(root) }
    }

    private suspend fun run(root: String) {
        var token = ""
        var failures = 0
        val bridge = service<AscBridgeService>()
        while (true) {
            try {
                val req = WatchRequest.newBuilder().setSinceToken(token).build().toByteArray()
                val raw = bridge.stream("asc.v1.Workspace/Watch", req, root)
                val events = raw.map { b ->
                    WatchEvent.parseFrom(b).also { if (it.token.isNotEmpty()) token = it.token }
                }
                WatchPlan.coalesce(events).collect { plan ->
                    failures = 0
                    apply(root, plan)
                }
                failures++ // the stream ended without an error: resubscribe
            } catch (e: BridgeException) {
                if (e.code == asc.asc.bridge.Code.CANCELLED && !e.bridgeGone) return
                failures++
            }
            // The bridge may have missed events while it was down; mark everything dirty once it is back.
            delay(minOf(30_000L, 250L shl minOf(failures, 7)))
            apply(root, DirtyPlan.EVERYTHING)
        }
    }

    internal fun apply(root: String, plan: DirtyPlan) {
        val mgr = VcsDirtyScopeManager.getInstance(project)
        val rootPath = Paths.get(root)
        if (plan.everything) {
            LocalFileSystem.getInstance().findFileByNioFile(rootPath)?.let { mgr.dirDirtyRecursively(it) }
        } else {
            val files = plan.paths.mapNotNull { StatusMapping.resolve(rootPath, com.google.protobuf.ByteString.copyFromUtf8(it)) }
                .map { com.intellij.vcsUtil.VcsUtil.getFilePath(it.toFile(), false) }
            mgr.filePathsDirty(files, emptyList())
        }
        project.messageBus.syncPublisher(AscStateListener.ASC_STATE).stateChanged(root, plan)
    }
}

/** Starts a watch for every asc root of the project. */
class AscWatchStartup : ProjectActivity {
    override suspend fun execute(project: Project) {
        val watcher = project.service<AscWatchService>()
        val roots = linkedSetOf<String>()
        project.basePath?.let { roots += it }
        com.intellij.openapi.roots.ProjectRootManager.getInstance(project).contentRoots.forEach { roots += it.path }
        roots.filter { AscRoots.isAscRoot(it) }.forEach { watcher.watch(it) }
    }
}
