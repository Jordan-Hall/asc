package asc.asc.vcs

import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.AbstractVcs
import com.intellij.openapi.vcs.VcsKey

/** The `asc` version control system (SPEC 24.5.1). Features arrive with P37b onwards. */
class AscVcs(project: Project) : AbstractVcs(project, NAME) {
    override fun getDisplayName(): String = "asc"

    override fun getConfigurable() = null

    companion object {
        const val NAME = "asc"

        @JvmField
        val KEY: VcsKey = createKey(NAME)
    }
}
