package asc.asc.stack

import com.google.gson.JsonParser

/**
 * Live updates in the IDE (SPEC 21.3.8, 24.4, P59). Default off: the status bar shows "trunk moved N commits -
 * Restack" (N from `WsStack.behind`), a toggle runs `live pause|resume`, and a live-conflict notice shows until
 * `live resume`. The plugin never moves the working copy: its only verbs are `restack` and `live pause|resume`,
 * run through the bridge allowlist. Pure logic, no platform types; the verb runner is injected.
 * The bridge allowlist has no `live` entry, so [LiveVerbs] is the seam a runner fills in once it has one.
 */
data class LiveState(val paused: Boolean, val conflict: Boolean)

/** Runs one asc verb in the project root through `Bridge.Run`; returns the exit code. */
fun interface LiveVerbs {
    fun run(verb: String, args: List<String>): Int
}

object LiveStatus {
    const val BEHIND_CAP = 999
    private const val MAX_JSON = 65_536

    /** Reads `live status --json` defensively. Malformed input gives null. */
    fun parse(text: String?): LiveState? {
        if (text == null || text.isEmpty() || text.length > MAX_JSON) return null
        val o = try {
            JsonParser.parseString(text).takeIf { it.isJsonObject }?.asJsonObject
        } catch (_: Exception) {
            null
        } ?: return null
        val p = o.get("paused")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isBoolean } ?: return null
        if (!p.asBoolean) return LiveState(paused = false, conflict = false)
        val reason = o.get("pauseReason")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
        return LiveState(paused = true, conflict = reason == "conflict")
    }

    /** The "trunk moved" text; null (hidden) at N = 0. */
    fun restackText(behind: Int): String? {
        val n = behind.coerceIn(0, BEHIND_CAP)
        if (n == 0) return null
        return "trunk moved $n ${if (n == 1) "commit" else "commits"} - Restack"
    }

    /** The notice text; null (hidden) unless a conflict pauses live updates. */
    fun notice(s: LiveState?): String? =
        if (s != null && s.paused && s.conflict) "Live updates paused by a conflict. Resolve it, then resume." else null

    /** The verb a toggle click runs. */
    fun toggleVerb(s: LiveState?): String = if (s?.paused == true) "resume" else "pause"

    fun toggleLabel(s: LiveState?): String =
        if (s?.paused == true) "Resume live updates" else "Pause live updates"

    /** Runs the toggle's verb; true on exit 0. */
    fun toggle(s: LiveState?, verbs: LiveVerbs): Boolean = verbs.run("live", listOf(toggleVerb(s))) == 0

    /** Runs `restack` and nothing else. */
    fun restack(verbs: LiveVerbs): Boolean = verbs.run("restack", emptyList()) == 0
}
