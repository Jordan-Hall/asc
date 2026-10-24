package asc.asc.vcs

import asc.v1.WorkspaceOuterClass.StatsRequest
import asc.v1.WorkspaceOuterClass.StatsResponse
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.components.service
import com.intellij.openapi.project.Project
import com.intellij.openapi.roots.impl.DirectoryIndexExcludePolicy
import com.intellij.openapi.startup.ProjectActivity
import asc.asc.bridge.AscBridgeService
import java.nio.file.Path
import java.nio.file.Paths
import kotlinx.coroutines.delay

/**
 * Hydration guard (SPEC 24.4.11, IP-D14). On a virtual workspace an IDE indexer reads file bodies and hydrates them,
 * so generated folders are excluded from the index and the per-process hydration counters are watched.
 * Source roots stay indexed. Nothing here reads a working file. The `.ascignore` provider is [AscIgnoreProvider].
 */
object IndexGuard {
    /** Generated folders excluded from indexing, relative to an asc root (SPEC 24.4.11). */
    val EXCLUDED_DIRS: List<String> = listOf("node_modules", "dist", ".turbo", "target", ".next", ".asc")

    /** Default `asc.vfs.hydrationBudget`: hydrations per process per day before the notification fires. */
    const val DEFAULT_BUDGET = 5_000L
    const val POLL_MS = 60_000L
    const val NOTIFY_INTERVAL_MS = 24L * 60 * 60 * 1000

    /** Absolute `file://` URLs of the excluded folders under each root; never touches the disk. */
    fun excludeUrls(roots: Collection<Path>): List<String> =
        roots.flatMap { root -> EXCLUDED_DIRS.map { "file://" + root.resolve(it).toString().replace('\\', '/') } }

    /** Roots of the project that are asc workspaces. Empty (inert) for any other project. */
    fun ascRoots(basePath: String?, contentRoots: Collection<String>): List<Path> =
        (listOfNotNull(basePath) + contentRoots).distinct()
            .mapNotNull { runCatching { Paths.get(it) }.getOrNull() }
            .filter { AscRoots.isAscRoot(it) }

    data class Alert(val process: String, val count: Long, val topFolders: List<String>)

    /**
     * The alert to show, or null. One notification per [NOTIFY_INTERVAL_MS] ([lastNotifiedMs] is null before the first).
     * The offender is the process with the most hydrations over [budget]; the top three folders come from the path counters.
     */
    fun evaluate(
        byProcess: List<Pair<String, Long>>,
        byPath: List<Pair<String, Long>>,
        budget: Long,
        lastNotifiedMs: Long?,
        nowMs: Long,
    ): Alert? {
        if (lastNotifiedMs != null && nowMs - lastNotifiedMs < NOTIFY_INTERVAL_MS) return null
        val worst = byProcess.filter { it.second > budget }.maxByOrNull { it.second } ?: return null
        val folders = byPath.groupBy({ folderOf(it.first) }, { it.second })
            .mapValues { e -> e.value.sum() }
            .entries.sortedByDescending { it.value }.take(3).map { it.key }
        return Alert(worst.first, worst.second, folders)
    }

    private fun folderOf(path: String): String {
        val p = path.replace('\\', '/').trim('/')
        return if (p.contains('/')) p.substringBeforeLast('/') else "."
    }

    fun message(a: Alert): String =
        "Search is hydrating files (${a.process}: ${a.count}). Use asc: Search code, or exclude folders" +
            if (a.topFolders.isEmpty()) "." else ": ${a.topFolders.joinToString(", ")}."
}

/** Excludes the generated folders of every asc root from the index. */
class AscIndexExcludePolicy(private val project: Project) : DirectoryIndexExcludePolicy {
    override fun getExcludeUrlsForProject(): Array<String> =
        IndexGuard.excludeUrls(IndexGuard.ascRoots(project.basePath, emptyList())).toTypedArray()
}

/** Reads `Workspace.Stats` once a minute and shows at most one notification per 24 hours. */
class AscHydrationWatch : ProjectActivity {
    override suspend fun execute(project: Project) {
        val roots = IndexGuard.ascRoots(project.basePath, emptyList())
        if (roots.isEmpty()) return // inert in non-asc projects
        val budget = System.getProperty("asc.vfs.hydrationBudget")?.toLongOrNull() ?: IndexGuard.DEFAULT_BUDGET
        var last: Long? = null
        while (true) {
            delay(IndexGuard.POLL_MS)
            for (root in roots) {
                val reply = runCatching {
                    service<AscBridgeService>().unary(
                        "asc.v1.Workspace/Stats", StatsRequest.getDefaultInstance().toByteArray(), root.toString(),
                    )
                }.getOrNull() ?: continue
                val stats = runCatching { StatsResponse.parseFrom(reply) }.getOrNull() ?: continue
                val alert = IndexGuard.evaluate(
                    stats.byProcessList.map { it.key to it.count },
                    stats.byPathList.map { it.key to it.count },
                    budget, last, System.currentTimeMillis(),
                ) ?: continue
                last = System.currentTimeMillis()
                NotificationGroupManager.getInstance().getNotificationGroup("asc")
                    .createNotification(IndexGuard.message(alert), NotificationType.WARNING).notify(project)
            }
        }
    }
}
