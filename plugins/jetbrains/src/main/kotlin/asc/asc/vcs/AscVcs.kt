package asc.asc.vcs

import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.AbstractVcs
import com.intellij.openapi.vcs.VcsKey
import com.intellij.openapi.vcs.changes.ChangeProvider
import com.intellij.openapi.vcs.annotate.AnnotationProvider
import com.intellij.openapi.vcs.diff.DiffProvider
import com.intellij.openapi.vcs.history.VcsHistoryProvider
import com.intellij.openapi.vcs.checkin.CheckinEnvironment
import com.intellij.openapi.vcs.rollback.RollbackEnvironment
import com.intellij.openapi.vcs.merge.MergeProvider

/** The `asc` version control system (SPEC 24.5.1). Features arrive with P37b onwards. */
class AscVcs(project: Project) : AbstractVcs(project, NAME) {
    override fun getDisplayName(): String = "asc"

    override fun getConfigurable() = null

    private val changeProvider by lazy { AscChangeProvider(myProject, this) }

    override fun getChangeProvider(): ChangeProvider = changeProvider

    private val diffProvider by lazy { AscDiffProvider(project) }

    override fun getDiffProvider(): DiffProvider = diffProvider

    private val annotationProvider by lazy { AscAnnotationProvider(project) }

    override fun getAnnotationProvider(): AnnotationProvider = annotationProvider

    private val historyProvider by lazy { AscHistoryProvider(project) }

    override fun getVcsHistoryProvider(): VcsHistoryProvider = historyProvider

    private val mergeProvider by lazy { AscMergeProvider(project) }

    override fun getMergeProvider(): MergeProvider = mergeProvider

    private val checkinEnvironment by lazy { AscCheckinEnvironment(project, this) }

    override fun getCheckinEnvironment(): CheckinEnvironment = checkinEnvironment

    private val rollbackEnvironment by lazy { AscRollbackEnvironment(project) }

    override fun getRollbackEnvironment(): RollbackEnvironment = rollbackEnvironment

    companion object {
        const val NAME = "asc"

        @JvmField
        val KEY: VcsKey = createKey(NAME)
    }
}
