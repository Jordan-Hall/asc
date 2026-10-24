package asc.asc.vcs

import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.project.Project
import com.intellij.openapi.roots.ProjectRootManager
import com.intellij.openapi.startup.ProjectActivity
import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vcs.VcsConfiguration
import com.intellij.openapi.vcs.VcsDirectoryMapping

/** Pure planning step so the decision is testable without a platform fixture. */
object MappingPlan {
    data class Mapping(val directory: String, val vcs: String)

    data class Result(val mappings: List<Mapping>, val changedRoots: List<String>)

    private fun norm(p: String) = p.replace('\\', '/').trimEnd('/')

    /**
     * Maps each asc root to `asc`, replacing an existing `Git` (or unset) mapping for that root.
     * Mappings of other VCSs, and mappings for other directories, are left as they are.
     */
    fun plan(current: List<Mapping>, ascRoots: Collection<String>): Result {
        val out = current.toMutableList()
        val changed = mutableListOf<String>()
        for (root in ascRoots.map(::norm).distinct()) {
            val idx = out.indexOfFirst { norm(it.directory) == root }
            when {
                idx < 0 -> {
                    out += Mapping(root, AscVcs.NAME)
                    changed += root
                }
                out[idx].vcs == "Git" || out[idx].vcs.isEmpty() -> {
                    out[idx] = Mapping(root, AscVcs.NAME)
                    changed += root
                }
            }
        }
        return Result(out, changed)
    }
}

class AscMappingActivity : ProjectActivity {
    override suspend fun execute(project: Project) {
        val basePath = project.basePath ?: return
        val candidates = linkedSetOf(basePath)
        ProjectRootManager.getInstance(project).contentRoots.forEach { candidates += it.path }
        val roots = candidates.filter { AscRoots.isAscRoot(it) }
        if (roots.isEmpty()) return // inert in non-asc projects

        val vcsManager = ProjectLevelVcsManager.getInstance(project)
        val current = vcsManager.directoryMappings.map { MappingPlan.Mapping(it.directory, it.vcs) }
        val plan = MappingPlan.plan(current, roots)

        // TODO(P52): the spike has not run. Mapping the root to `asc` registers it, which Git4Idea's
        // unregistered-root notice skips; ignoring the root as well is the fallback until P52 picks a fix.
        val config = VcsConfiguration.getInstance(project)
        val ignored = config.ignoredUnregisteredRoots.map { it.replace('\\', '/') }
        val toIgnore = roots.map { it.replace('\\', '/') }.filter { it !in ignored }
        if (toIgnore.isNotEmpty()) config.addIgnoredUnregisteredRoots(toIgnore)

        if (plan.changedRoots.isEmpty()) return
        vcsManager.setDirectoryMappings(plan.mappings.map { VcsDirectoryMapping(it.directory, it.vcs) })
        NotificationGroupManager.getInstance().getNotificationGroup("asc")
            .createNotification(
                "asc manages ${plan.changedRoots.size} root(s) in this project; the Git mapping was replaced.",
                NotificationType.INFORMATION,
            ).notify(project)
    }
}
