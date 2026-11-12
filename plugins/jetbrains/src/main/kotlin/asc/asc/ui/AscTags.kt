package asc.asc.ui

import asc.asc.bridge.BridgeException
import asc.asc.vcs.AscRun
import asc.asc.vcs.RunOutcome
import asc.v1.Cli.OpResult
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.fileChooser.FileChooser
import com.intellij.openapi.fileChooser.FileChooserDescriptorFactory
import com.intellij.openapi.progress.ProgressIndicator
import com.intellij.openapi.progress.ProgressManager
import com.intellij.openapi.progress.Task
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.Messages
import com.intellij.openapi.vfs.LocalFileSystem
import java.nio.file.Path
import java.nio.file.Paths

/** One local tag as the `tag` verb reports it (`cli.proto` `TagEntry`). */
data class TagRow(val name: String, val target: String, val annotated: Boolean, val subject: String)

/**
 * Pure argv planning for the tag verbs and `stack get --worktree` (SPEC 24.8.1); no platform types. The
 * bridge allowlist bounds a tag name to 255 bytes and a message to 1024; a name is refused here first
 * with the same rules the VS Code plugin applies, so the person sees why before the bridge is called.
 */
object TagPlan {
    const val NAME_MAX = 255
    const val MESSAGE_MAX = 1024
    private val REFUSED = Regex("""[\s~^:?*\[\\]""")

    /** Why [name] cannot be a tag, or null. */
    fun nameProblem(name: String): String? = when {
        name.isEmpty() -> "A tag needs a name."
        name.toByteArray(Charsets.UTF_8).size > NAME_MAX -> "The tag name is too long."
        name.startsWith("-") -> "A tag name cannot start with '-'."
        name.any { it.code < 0x20 || it.code == 0x7f } -> "The tag name has a control character."
        REFUSED.containsMatchIn(name) -> "The tag name has a character git refuses."
        else -> null
    }

    /** Why [message] cannot be a tag message, or null. A blank message makes a lightweight tag. */
    fun messageProblem(message: String): String? = when {
        message.trim().toByteArray(Charsets.UTF_8).size > MESSAGE_MAX -> "The tag message is too long."
        message.contains('\u0000') -> "The tag message has a NUL character."
        else -> null
    }

    fun listArgs(): List<String> = emptyList()

    /** `tag [-m=<message>] <name>`: annotated when [message] is not blank, at HEAD. Null when refused. */
    fun createArgs(name: String, message: String): List<String>? {
        if (nameProblem(name) != null || messageProblem(message) != null) return null
        val m = message.trim()
        return (if (m.isEmpty()) emptyList() else listOf("-m=$m")) + name
    }

    fun deleteArgs(name: String): List<String>? = if (nameProblem(name) != null) null else listOf(name)

    /** `push --tags`: every local tag to the default remote. */
    fun pushArgs(): List<String> = listOf("--tags")

    /** The tags a `tag` result lists; [] for anything else. */
    fun parse(opResult: ByteArray?): List<TagRow> {
        if (opResult == null || opResult.isEmpty()) return emptyList()
        val r = try {
            OpResult.parseFrom(opResult)
        } catch (_: com.google.protobuf.InvalidProtocolBufferException) {
            return emptyList()
        }
        if (!r.hasTag()) return emptyList()
        return r.tag.tagsList.map { t ->
            TagRow(
                t.name,
                t.target.toByteArray().joinToString("") { "%02x".format(it) },
                t.annotated,
                t.subject.toStringUtf8(),
            )
        }
    }

    /** The delete prompt, from the parsed tag and never from caller text. */
    fun deleteQuestion(t: TagRow): String = "Delete tag ${t.name} (${t.target.take(12)})?"

    /** One line per tag for the chooser: name, short target and the subject of an annotated tag. */
    fun label(t: TagRow): String =
        listOf(t.name, t.target.take(12), if (t.annotated) t.subject else "").filter { it.isNotEmpty() }
            .joinToString("  ")

    /**
     * `stack get --worktree=<dir> <rev>`: the bridge requires [dir] inside [root], so a directory outside
     * gives null, as does a blank revision or one that starts with '-'.
     */
    fun stackGetArgs(root: Path, dir: Path, rev: String): List<String>? {
        val r = rev.trim()
        if (r.isEmpty() || r.startsWith("-") || r.any { it.code < 0x20 }) return null
        val base = root.toAbsolutePath().normalize()
        val d = dir.toAbsolutePath().normalize()
        if (d == base || !d.startsWith(base)) return null
        val rel = base.relativize(d).joinToString("/")
        return listOf("--worktree=$rel", r)
    }
}

/** Runs one verb on a background task and reports the outcome; [then] gets the outcome on success. */
private fun runInBackground(
    project: Project,
    title: String,
    root: Path,
    verb: String,
    args: List<String>,
    then: (RunOutcome) -> Unit = { AscActions.notify(project, title, "Done.", true) },
) {
    ProgressManager.getInstance().run(object : Task.Backgroundable(project, "asc $verb", true) {
        override fun run(indicator: ProgressIndicator) {
            val out = try {
                AscRun.run(root, verb, args)
            } catch (e: BridgeException) {
                return AscActions.notify(project, title, e.message ?: "The asc bridge is unavailable", false)
            }
            if (!out.ok) return AscActions.notify(project, title, out.message(), false)
            then(out)
        }
    })
}

/** Base for the actions here: visible only in a project with an asc root. */
abstract class AscRootAction(title: String) : AnAction(title), DumbAware {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        val p = e.project
        e.presentation.isEnabledAndVisible = p != null && AscActions.firstRoot(p) != null
    }

    override fun actionPerformed(e: AnActionEvent) {
        val p = e.project ?: return
        val root = AscActions.firstRoot(p) ?: return
        perform(p, root)
    }

    abstract fun perform(project: Project, root: Path)
}

/** "Create Tag...": a name, then an optional message (a message makes an annotated tag). */
class AscCreateTagAction : AscRootAction("Create Tag...") {
    override fun perform(project: Project, root: Path) {
        val title = "Create Tag"
        val name = Messages.showInputDialog(project, "Tag name", title, null, "", object :
            com.intellij.openapi.ui.InputValidatorEx {
            override fun getErrorText(inputString: String): String? = TagPlan.nameProblem(inputString.trim())
            override fun checkInput(inputString: String): Boolean = getErrorText(inputString) == null
            override fun canClose(inputString: String): Boolean = checkInput(inputString)
        })?.trim() ?: return
        val message = Messages.showMultilineInputDialog(
            project, "Message (leave empty for a lightweight tag)", title, "", null, object :
                com.intellij.openapi.ui.InputValidatorEx {
                override fun getErrorText(inputString: String): String? = TagPlan.messageProblem(inputString)
                override fun checkInput(inputString: String): Boolean = getErrorText(inputString) == null
                override fun canClose(inputString: String): Boolean = checkInput(inputString)
            },
        ) ?: return
        val args = TagPlan.createArgs(name, message) ?: return
        runInBackground(project, title, root, "tag", args) {
            AscActions.notify(project, title, "Created tag $name.", true)
        }
    }
}

/** "Delete Tag...": lists the local tags, asks which one, then confirms (the allowlist marks it modal). */
class AscDeleteTagAction : AscRootAction("Delete Tag...") {
    override fun perform(project: Project, root: Path) {
        val title = "Delete Tag"
        runInBackground(project, title, root, "tag --list", TagPlan.listArgs()) { out ->
            val tags = TagPlan.parse(out.opResult)
            if (tags.isEmpty()) return@runInBackground AscActions.notify(project, title, "There are no tags.", true)
            ApplicationManager.getApplication().invokeLater {
                val labels = tags.map(TagPlan::label).toTypedArray()
                val i = Messages.showChooseDialog(
                    project, "Tag to delete", title, null, labels, labels.first(),
                )
                val tag = tags.getOrNull(i) ?: return@invokeLater
                if (Messages.showYesNoDialog(project, TagPlan.deleteQuestion(tag), title, Messages.getQuestionIcon()) !=
                    Messages.YES
                ) return@invokeLater
                val args = TagPlan.deleteArgs(tag.name) ?: return@invokeLater
                runInBackground(project, title, root, "tag --delete", args) {
                    AscActions.notify(project, title, "Deleted tag ${tag.name}.", true)
                }
            }
        }
    }
}

/** "Show Tags": the local tags in a notification. */
class AscListTagsAction : AscRootAction("Show Tags") {
    override fun perform(project: Project, root: Path) {
        val title = "Tags"
        runInBackground(project, title, root, "tag --list", TagPlan.listArgs()) { out ->
            val tags = TagPlan.parse(out.opResult)
            val text = if (tags.isEmpty()) "There are no tags." else tags.take(50).joinToString("\n", transform = TagPlan::label)
            AscActions.notify(project, title, text, true)
        }
    }
}

/** "Get a Stack into a Worktree...": a revision, then a directory inside the root; runs `stack get --worktree`. */
class AscStackGetWorktreeAction : AscRootAction("Get a Stack into a Worktree...") {
    override fun perform(project: Project, root: Path) {
        val title = "Get a Stack into a Worktree"
        val rev = Messages.showInputDialog(project, "Change, branch or revision of the stack", title, null)
            ?.trim()?.takeIf { it.isNotEmpty() } ?: return
        val start = LocalFileSystem.getInstance().findFileByNioFile(root)
        val chosen = FileChooser.chooseFile(
            FileChooserDescriptorFactory.createSingleFolderDescriptor().withTitle("Worktree Directory (inside the root)"),
            project,
            start,
        ) ?: return
        val args = TagPlan.stackGetArgs(root, Paths.get(chosen.path), rev)
            ?: return AscActions.notify(
                project, title, "Choose a directory inside ${root.fileName} and a revision that does not start with '-'.",
                false,
            )
        runInBackground(project, title, root, "stack get", args)
    }
}
