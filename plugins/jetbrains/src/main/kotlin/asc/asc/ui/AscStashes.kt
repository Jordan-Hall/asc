package asc.asc.ui

import asc.v1.Cli.OpResult
import com.intellij.openapi.project.Project
import java.nio.file.Path

/** One stash as `stash list` reports it (`cli.proto` `StashEntry`). */
data class StashRow(val index: Int, val commit: String, val message: String)

/** Pure planning and parsing for `stash list`; no platform types. */
object StashPlan {
    const val SHOWN = 50

    fun listArgs(): List<String> = emptyList()

    /** The entries of a `stash` result; [] for anything else. */
    fun parse(opResult: ByteArray?): List<StashRow> {
        if (opResult == null || opResult.isEmpty()) return emptyList()
        val r = try {
            OpResult.parseFrom(opResult)
        } catch (_: com.google.protobuf.InvalidProtocolBufferException) {
            return emptyList()
        }
        if (!r.hasStash()) return emptyList()
        return r.stash.entriesList.map { e ->
            StashRow(e.index, e.commit.toByteArray().joinToString("") { "%02x".format(it) }, e.message.toStringUtf8())
        }
    }

    /** `stash@{n}  12hex  message`, the message cut to its first line. */
    fun label(s: StashRow): String =
        listOf("stash@{${s.index}}", s.commit.take(12), s.message.lineSequence().firstOrNull().orEmpty())
            .filter { it.isNotEmpty() }.joinToString("  ")

    /** The notification text: at most [SHOWN] rows, then how many more there are. */
    fun text(rows: List<StashRow>): String = when {
        rows.isEmpty() -> "There are no stashes."
        rows.size <= SHOWN -> rows.joinToString("\n", transform = ::label)
        else -> rows.take(SHOWN).joinToString("\n", transform = ::label) + "\n… and ${rows.size - SHOWN} more"
    }
}

/** "Show Stashes": the stash list in a notification (Pop, Apply and Drop act on the latest). */
class AscListStashesAction : AscRootAction("Show Stashes") {
    override fun perform(project: Project, root: Path) {
        val title = "Stashes"
        runInBackground(project, title, root, "stash list", StashPlan.listArgs()) { out ->
            AscActions.notify(project, title, StashPlan.text(StashPlan.parse(out.opResult)), true)
        }
    }
}
