package asc.asc.auth

import java.io.File
import java.util.concurrent.TimeUnit

/**
 * "Configure agents": `asc mcp install --scope project`, run in the project root. No secret is in argv.
 * TODO(plugin.xml): an action entry for this and for sign-in is added by the unit that owns plugin.xml.
 */
object ConfigureAgents {
    fun args(): List<String> = listOf("mcp", "install", "--scope", "project")

    /** Returns a user-facing message; never throws. */
    fun run(ascPath: String?, root: String?, exec: (List<String>, File) -> Int = ::runProcess): String {
        if (ascPath == null) return "asc was not found; set the path to asc."
        if (root == null) return "Open an ASC workspace to configure agents."
        val ok = try {
            exec(listOf(ascPath) + args(), File(root)) == 0
        } catch (_: Exception) {
            false
        }
        return if (ok) {
            "Agents configured for this project (asc mcp install --scope project)."
        } else {
            "asc mcp install failed; run it in a terminal to see why."
        }
    }

    private fun runProcess(cmd: List<String>, cwd: File): Int {
        val pb = ProcessBuilder(cmd).directory(cwd)
        pb.environment()["GIT_TERMINAL_PROMPT"] = "0"
        pb.environment()["GCM_INTERACTIVE"] = "never"
        pb.redirectErrorStream(true).redirectOutput(ProcessBuilder.Redirect.DISCARD)
        val p = pb.start()
        if (!p.waitFor(60, TimeUnit.SECONDS)) {
            p.destroyForcibly()
            return -1
        }
        return p.exitValue()
    }
}
