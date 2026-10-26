package asc.asc.auth

import asc.asc.bridge.AscExecutable
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * "Configure agents": `asc -C <root> mcp install --scope project`. No secret is in argv. It runs only an
 * absolute asc path, from that executable's own directory (never the project root, where a planted
 * executable would be found first on Windows); the root goes through `-C`.
 */
object ConfigureAgents {
    fun args(root: String): List<String> = listOf("-C", root, "mcp", "install", "--scope", "project")

    /** Returns a user-facing message; never throws. */
    fun run(
        ascPath: String?,
        root: String?,
        exec: (List<String>, File) -> Int = ::runProcess,
        windows: Boolean = System.getProperty("os.name").orEmpty().startsWith("Windows", ignoreCase = true),
    ): String {
        if (ascPath == null || !AscExecutable.isAbsolute(ascPath, windows)) {
            return "asc was not found; set the path to asc."
        }
        if (root == null) return "Open an ASC workspace to configure agents."
        val ok = try {
            exec(listOf(ascPath) + args(root), File(ascPath).absoluteFile.parentFile) == 0
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
