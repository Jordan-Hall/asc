package asc.asc.bridge

/**
 * The environment of the bridge child (SPEC 24.6). It never prompts: no terminal prompt, no Git Credential
 * Manager dialog, no askpass. A host-supplied token goes in `ASC_TOKEN` (never argv) and wins over an
 * inherited one, together with `ASC_TOKEN_HOST`, the one host it was given for: asc sends `ASC_TOKEN` to no
 * other host. A token without a host is not passed at all.
 */
object BridgeEnv {
    private val dropped = setOf("GIT_ASKPASS", "SSH_ASKPASS", "GIT_EDITOR", "EDITOR", "VISUAL")

    fun build(inherited: Map<String, String>, token: String?, tokenHost: String?): Map<String, String> {
        val env = LinkedHashMap<String, String>()
        for ((k, v) in inherited) if (k.uppercase() !in dropped) env[k] = v
        env["GIT_TERMINAL_PROMPT"] = "0"
        env["GCM_INTERACTIVE"] = "never"
        if (token != null && !tokenHost.isNullOrEmpty()) {
            env["ASC_TOKEN"] = token
            env["ASC_TOKEN_HOST"] = tokenHost
        }
        return env
    }

    /** The fixed bridge command line; the asc path is the only variable part and no secret is ever in it. */
    fun command(ascPath: String): List<String> = listOf(ascPath, "api", "--stdio")
}
