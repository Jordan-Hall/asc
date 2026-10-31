package asc.asc.ui

import asc.v1.BridgeOuterClass.RunRequest
import asc.v1.ChangesOuterClass.RequestMergeRequest
import asc.v1.WorkspaceOuterClass.WsStageRequest
import com.google.gson.JsonArray
import com.google.gson.JsonElement
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.google.gson.JsonPrimitive
import asc.asc.bridge.BridgeException
import java.nio.file.InvalidPathException
import java.nio.file.Path
import java.util.Base64
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.launch

// The JetBrains side of the asc-ui relay (SPEC 24.4.5, 24.4.5.1, 24.5.2, IP-D4). It is a port of
// packages/asc-client-ts/src/transport/relay.ts: untrusted JSON messages from the JCEF page are forwarded to the
// bridge only when they match an entry of allowlist.json. Deny by default: an unknown method, a `never` or
// unclassified RPC, a verb or flag outside the schema, a host API pair outside its provider's list, a body field outside the
// pair's list or a request over a limit is dropped, logged without its payload and answered PERMISSION_DENIED.
// The confirmation text is composed here from the parsed request, never taken from the page. The page never
// sees a token: the host API executor adds it.
//
// The Gradle task `generateAllowlist` embeds allowlist.json (with its SHA-256) as `GeneratedAllowlist` and puts
// it on the classpath as the resource [Allowlist.RESOURCE]; this file parses that json at runtime
// ([Allowlist.parse]), and when the resource is missing the relay denies everything.
//
// Wire (JSON strings, bytes as standard base64). Page to host: {id, method, bytes, root?}, {id, cancel:true},
// {id, hostApi:{provider, method, path, body?}}, {id, avatar:url}, {id, host:{op, ...}}. Host to page: {id, reply},
// {id, end:{code, message, detail?}}, {id, hostApi:{status, body}}, {id, avatar}, {theme:{...}}.

enum class Confirm { NONE, MODAL }

class WriteEntry(val name: String, val confirm: Confirm, val patchBytes: Long?, val paths: Long?)

class VerbEntry(val flags: Map<String, String>, val pos: String, val confirm: Confirm)

class HostApiPair(val method: String, val path: String, val body: List<String>?, val confirm: Confirm)

/** The checked form of allowlist.json. */
class Allowlist(
    val read: Set<String>,
    val write: Map<String, WriteEntry>,
    val reject: Set<String>,
    val maxArgs: Int,
    val maxArgBytes: Int,
    val verbs: Map<String, VerbEntry>,
    val providers: Map<String, List<HostApiPair>>,
) {
    companion object {
        const val RESOURCE = "/asc/asc/allowlist.json"

        /** Parses and checks allowlist.json; throws [IllegalArgumentException] on anything odd. */
        fun parse(text: String): Allowlist {
            val root = obj(JsonParser.parseString(text), "allowlist")
            require(root.get("version")?.asIntOrNull() == 1) { "allowlist: version 1 expected" }
            val rpc = obj(root.get("rpc"), "rpc")
            val run = obj(root.get("run"), "run")
            val hostApi = obj(root.get("hostApi"), "hostApi")
            val providerList = obj(hostApi.get("providers"), "providers")
            val write = LinkedHashMap<String, WriteEntry>()
            for (w in arr(rpc.get("write"))) {
                val o = obj(w, "write entry")
                val name = str(o.get("name"), "write name")
                val limits = o.get("limits")?.let { obj(it, "limits") } ?: JsonObject()
                for (k in limits.keys()) require(k == "patchBytes" || k == "paths") { "allowlist: unknown limit $k" }
                write[name] = WriteEntry(name, conf(o.get("confirm"), name), limits.get("patchBytes")?.asLong, limits.get("paths")?.asLong)
            }
            val verbs = LinkedHashMap<String, VerbEntry>()
            for ((name, v) in obj(run.get("verbs"), "verbs").entrySet()) {
                val e = obj(v, name)
                val flags = LinkedHashMap<String, String>()
                e.get("flags")?.let { f -> for ((k, t) in obj(f, "flags").entrySet()) flags[k] = str(t, "flag type") }
                verbs[name] = VerbEntry(flags, e.get("pos")?.let { str(it, "pos") } ?: "", conf(e.get("confirm"), name))
            }
            val providers = LinkedHashMap<String, List<HostApiPair>>()
            for ((providerId, prov) in providerList.entrySet()) {
                providers[providerId] = arr(obj(prov, providerId).get("pairs")).map {
                    val o = obj(it, "pair")
                    HostApiPair(
                        str(o.get("m"), "pair method"),
                        str(o.get("p"), "pair path"),
                        o.get("body")?.let { b -> arr(b).map { s -> str(s, "pair body") } },
                        conf(o.get("confirm"), "pair"),
                    )
                }
            }
            return Allowlist(
                read = arr(rpc.get("read")).map { str(it, "rpc.read") }.toSet(),
                write = write,
                reject = arr(run.get("reject")).map { str(it, "run.reject") }.toSet(),
                maxArgs = run.get("maxArgs")?.asIntOrNull() ?: throw IllegalArgumentException("allowlist: run limits missing"),
                maxArgBytes = run.get("maxArgBytes")?.asIntOrNull() ?: throw IllegalArgumentException("allowlist: run limits missing"),
                verbs = verbs,
                providers = providers,
            )
        }

        /** The packaged allowlist, or null when the resource is missing or malformed (the relay then denies all). */
        fun load(): Allowlist? = try {
            Allowlist::class.java.getResourceAsStream(RESOURCE)?.use { parse(String(it.readBytes(), Charsets.UTF_8)) }
        } catch (_: Exception) {
            null
        }

        private fun JsonElement.asIntOrNull(): Int? =
            if (isJsonPrimitive && asJsonPrimitive.isNumber) asInt else null

        private fun obj(e: JsonElement?, what: String): JsonObject {
            require(e != null && e.isJsonObject) { "allowlist: $what must be an object" }
            return e.asJsonObject
        }

        private fun arr(e: JsonElement?): JsonArray = when {
            e == null || e.isJsonNull -> JsonArray()
            e.isJsonArray -> e.asJsonArray
            else -> throw IllegalArgumentException("allowlist: array expected")
        }

        private fun str(e: JsonElement?, what: String): String {
            require(e != null && e.isJsonPrimitive && e.asJsonPrimitive.isString) { "allowlist: $what must be a string" }
            return e.asString
        }

        private fun conf(e: JsonElement?, what: String): Confirm = when {
            e == null || e.isJsonNull -> Confirm.NONE
            e.isJsonPrimitive && e.asString == "none" -> Confirm.NONE
            e.isJsonPrimitive && e.asString == "modal" -> Confirm.MODAL
            else -> throw IllegalArgumentException("allowlist: $what: bad confirm")
        }

        private fun JsonObject.keys(): Set<String> = keySet()
    }
}

/** What a request turned out to be, after parsing. The confirmation text is built from this. */
sealed interface Parsed {
    data class Rpc(val method: String) : Parsed

    data class Write(val method: String, val paths: Int, val patchBytes: Int, val change: String? = null) : Parsed

    data class Run(val verb: String, val args: List<String>) : Parsed

    data class HostApi(val provider: String, val method: String, val template: String, val params: Map<String, String>, val body: JsonObject?) : Parsed
}

sealed interface Decision {
    class Allow(val parsed: Parsed, val confirm: Confirm, val bytes: ByteArray? = null) : Decision

    class Deny(val reason: String) : Decision
}

class RunScope(val root: String, val tmpDir: String? = null)

class ForgeScope(val repos: List<String>)

/** The pure checks. Mirrors relay.ts `checkRun`, `checkRpc` and `checkHostApi`; the bridge stays the authority. */
object RelayChecks {
    private val USER = Regex("^[A-Za-z0-9._-]{1,40}$")
    private val SEGMENT = Regex("^[A-Za-z0-9._~-]+$")
    private val MERGE_DO = setOf("merge", "rebase", "rebase-merge", "squash", "fast-forward-only", "manually-merged")

    private fun no(reason: String): Decision = Decision.Deny(reason)

    private sealed interface Ty {
        data object Bool : Ty

        data class Str(val max: kotlin.Int) : Ty

        data class Num(val max: kotlin.Int) : Ty

        data class Named(val name: String) : Ty
    }

    private fun ty(word: String): Ty? {
        if (word == "bool") return Ty.Bool
        if (word == "rev" || word == "path" || word == "tmpfile" || word == "user") return Ty.Named(word)
        val m = Regex("^(str|int):(\\d+)$").matchEntire(word) ?: return null
        val max = m.groupValues[2].toIntOrNull() ?: return null
        return if (m.groupValues[1] == "str") Ty.Str(max) else Ty.Num(max)
    }

    private fun hasControl(s: String): Boolean = s.any { it.code < 0x20 || it.code == 0x7f }

    private fun isRev(s: String): Boolean =
        s.length in 1..256 && !s.startsWith("-") && !s[0].isWhitespace() && !hasControl(s)

    private fun inside(base: Path, v: Path): Boolean = v.startsWith(base) && v != base

    private fun checkValue(t: Ty, v: String, scope: RunScope, limit: Int): String? {
        if (v.contains('\u0000')) return "NUL byte"
        if (v.length > limit) return "too long"
        return when (t) {
            Ty.Bool -> "takes no value"
            is Ty.Str -> if ((v.isEmpty() && t.max <= 1024) || v.length > t.max) "bad length" else null
            is Ty.Num -> if (Regex("^\\d{1,9}$").matches(v) && v.toLong() <= t.max) null else "bad number"
            is Ty.Named -> when (t.name) {
                "rev" -> if (isRev(v)) null else "not a revision"
                "user" -> if (USER.matches(v)) null else "not a user"
                "path" -> {
                    if (v.isEmpty() || v.contains('\n') || v.contains('\r')) return "bad path"
                    try {
                        val root = Path.of(scope.root).toAbsolutePath().normalize()
                        val p = root.resolve(v).normalize()
                        if (p.startsWith(root)) null else "path leaves the workspace"
                    } catch (_: InvalidPathException) {
                        "bad path"
                    }
                }
                "tmpfile" -> {
                    val tmp = scope.tmpDir ?: return "bad temporary file"
                    if (v.contains('\n') || v.contains('\r')) return "bad temporary file"
                    try {
                        val p = Path.of(v)
                        if (!p.isAbsolute) return "bad temporary file"
                        if (inside(Path.of(tmp).toAbsolutePath().normalize(), p.normalize())) null else "temporary file leaves its directory"
                    } catch (_: InvalidPathException) {
                        "bad temporary file"
                    }
                }
                else -> "malformed schema"
            }
        }
    }

    fun checkRun(al: Allowlist, verb: String, args: List<String>, scope: RunScope): Decision {
        val entry = al.verbs[verb] ?: return no("verb not allowed")
        if (args.size > al.maxArgs) return no("too many arguments")
        val seen = HashSet<String>()
        val positional = ArrayList<String>()
        for (arg in args) {
            if (arg.length > al.maxArgBytes + 8 || arg.contains('\u0000')) return no("bad argument")
            if (!arg.startsWith("-")) {
                positional.add(arg)
                continue
            }
            val eq = arg.indexOf('=')
            val name = if (eq < 0) arg else arg.substring(0, eq)
            val value = if (eq < 0) null else arg.substring(eq + 1)
            if (name in al.reject) return no("flag rejected")
            val spec = entry.flags[name] ?: return no("unknown flag")
            val repeat = spec.endsWith("*")
            val t = ty(if (repeat) spec.dropLast(1) else spec) ?: return no("malformed schema")
            if (!repeat && !seen.add(name)) return no("flag given twice")
            if (t == Ty.Bool) {
                if (value != null) return no("flag takes no value")
            } else {
                if (value == null) return no("flag needs a value")
                if (checkValue(t, value, scope, al.maxArgBytes) != null) return no("bad flag value")
            }
        }
        var next = 0
        for (w in entry.pos.split(" ").filter { it.isNotEmpty() }) {
            val m = Regex("^(.*?)([?*+]?)$").matchEntire(w)
            val t = ty(m?.groupValues?.get(1) ?: "") ?: return no("malformed schema")
            val left = positional.size - next
            val mult = m?.groupValues?.get(2) ?: ""
            val take = when (mult) {
                "" -> 1
                "?" -> minOf(left, 1)
                else -> left
            }
            if (take > left || (mult == "+" && take == 0)) return no("missing argument")
            for (g in positional.subList(next, next + take)) {
                if (checkValue(t, g, scope, al.maxArgBytes) != null) return no("bad argument")
            }
            next += take
        }
        if (next < positional.size) return no("too many arguments")
        return Decision.Allow(Parsed.Run(verb, args.toList()), entry.confirm)
    }

    private fun hex(b: ByteArray): String = b.joinToString("") { "%02x".format(it) }

    fun checkRpc(al: Allowlist, method: String, bytes: ByteArray, scope: RunScope): Decision {
        if (method == "asc.v1.Bridge/Run") {
            val req = try {
                RunRequest.parseFrom(bytes)
            } catch (_: Exception) {
                return no("undecodable request")
            }
            val d = checkRun(al, req.verb, req.argsList, scope)
            if (d !is Decision.Allow) return d
            // Re-encode only the checked fields: `root` is chosen by the host, so it cannot ride along.
            val clean = RunRequest.newBuilder().setVerb(req.verb).addAllArgs(req.argsList).build().toByteArray()
            return Decision.Allow(d.parsed, d.confirm, clean)
        }
        if (method in al.read) return Decision.Allow(Parsed.Rpc(method), Confirm.NONE)
        val w = al.write[method] ?: return no("rpc not allowed")
        try {
            if (method == "asc.v1.Workspace/Stage") {
                val req = WsStageRequest.parseFrom(bytes)
                val paths = req.addCount + req.removeCount
                if (w.patchBytes != null && req.patch.size() > w.patchBytes) return no("patch too large")
                if (w.paths != null && paths > w.paths) return no("too many paths")
                return Decision.Allow(Parsed.Write(method, paths, req.patch.size()), w.confirm, req.toByteArray())
            }
            if (method == "asc.v1.Changes/RequestMerge") {
                val req = RequestMergeRequest.parseFrom(bytes)
                return Decision.Allow(Parsed.Write(method, 0, 0, hex(req.change.toByteArray())), w.confirm, req.toByteArray())
            }
        } catch (_: Exception) {
            return no("undecodable request")
        }
        // A write entry without a request check here must never pass.
        return no("write rpc has no request check")
    }

    fun checkHostApi(al: Allowlist, provider: String, method: String, path: String, body: JsonElement?, scope: ForgeScope): Decision {
        val pairs = al.providers[provider] ?: return no("host api provider not allowed")
        val q = path.indexOf('?')
        val rawPath = if (q < 0) path else path.substring(0, q)
        val query = if (q < 0) null else path.substring(q + 1)
        if (Regex("[\\\\#]|%2e|%2f|%5c|%00", RegexOption.IGNORE_CASE).containsMatchIn(path) || hasControl(path) || !rawPath.startsWith("/")) {
            return no("bad path")
        }
        if (query != null && !Regex("^[A-Za-z0-9_.~%=&,+-]*$").matches(query)) return no("bad query")
        val segs = rawPath.split("/").drop(1)
        if (segs.any { !SEGMENT.matches(it) || it == "." || it == ".." }) return no("bad path")
        val hasBody = body != null && !body.isJsonNull
        for (pair in pairs) {
            if (pair.method != method) continue
            val want = pair.path.split("/").drop(1)
            if (want.size != segs.size) continue
            val params = LinkedHashMap<String, String>()
            var hit = true
            for ((i, w) in want.withIndex()) {
                val g = segs[i]
                val name = Regex("^\\{(.+)}$").matchEntire(w)?.groupValues?.get(1)
                if (name == null) {
                    if (w != g) hit = false
                } else if ((name == "n" || name == "id") && !Regex("^\\d{1,18}$").matches(g)) {
                    hit = false
                } else {
                    params[name] = g
                }
            }
            if (!hit) continue
            if (params["o"] != null && "${params["o"]}/${params["r"]}" !in scope.repos) return no("repo not opened")
            var parsedBody: JsonObject? = null
            if (pair.body != null) {
                if (hasBody && !body!!.isJsonObject) return no("bad body")
                val b = if (hasBody) body!!.asJsonObject else JsonObject()
                if (pair.path.endsWith("/reviews") && b.has("event")) return no("pending review only")
                for (k in b.keySet()) if (k !in pair.body) return no("body field not allowed")
                if (b.has("Do")) {
                    val d = b.get("Do")
                    if (!d.isJsonPrimitive || !d.asJsonPrimitive.isString || d.asString !in MERGE_DO) return no("bad merge mode")
                }
                parsedBody = b
            } else if (hasBody) {
                return no("no body allowed")
            }
            return Decision.Allow(Parsed.HostApi(provider, pair.method, pair.path, params, parsedBody), pair.confirm)
        }
        return no("host api operation not allowed")
    }

    /** Native confirmation text, composed only from the parsed request. */
    fun describe(p: Parsed): Pair<String, List<String>> = when (p) {
        is Parsed.Run -> "Run \"asc ${p.verb}\"" to p.args.map { if (it.length > 120) it.take(117) + "..." else it }
        is Parsed.Write -> (if (p.method.endsWith("RequestMerge")) "Merge change" else "Stage changes") to
            listOf(p.change ?: "${p.paths} path(s)")
        is Parsed.HostApi -> {
            val title = when {
                p.template.endsWith("/merge") -> "Merge pull request"
                p.template.endsWith("/reviews/{id}") -> "Submit review"
                else -> "${p.method} ${p.template}"
            }
            title to p.params.map { "${it.key}=${it.value}" }
        }
        is Parsed.Rpc -> p.method to emptyList()
    }
}

/** The bridge calls the relay forwards to. The production adapter wraps `AscBridgeService.stream`. */
fun interface RelayUpstream {
    suspend fun stream(method: String, request: ByteArray, root: String): Flow<ByteArray>
}

class HostApiCall(val provider: String, val method: String, val path: String, val body: JsonObject?, val repo: String?)

class HostApiResult(val status: Int, val body: JsonElement)

class RelayLog(val event: String, val reason: String, val what: String? = null)

class RelayDeps(
    val upstream: RelayUpstream,
    val allowlist: Allowlist?,
    /** Sends one JSON message to the page. */
    val send: (String) -> Unit,
    /** Absolute roots the user opened; a call's root must be one of them (or empty). */
    val roots: () -> List<String>,
    /** `owner/repo` of the remote repos of the opened roots. */
    val remoteRepos: () -> List<String>,
    val tmpDir: String? = null,
    /** Shows the native modal. Without it, every entry that needs confirmation is denied. */
    val confirm: (suspend (title: String, targets: List<String>) -> Boolean)? = null,
    /** Runs a checked host API request and adds the token, by provider id; an unlisted provider is denied. */
    val hostApi: Map<String, suspend (HostApiCall) -> HostApiResult> = emptyMap(),
    val log: (RelayLog) -> Unit = {},
)

/** One relay per page. [handle] takes one raw page message and never throws. */
class AscRelay(private val scope: CoroutineScope, private val deps: RelayDeps) {
    private val live = ConcurrentHashMap<Long, Job>()

    companion object {
        const val PERMISSION_DENIED = 7
        const val INTERNAL = 13
        const val MAX_LIVE = 64
        const val MAX_MESSAGE_CHARS = 16 * 1024 * 1024
        private val B64 = Base64.getEncoder()
        private val B64D = Base64.getDecoder()
    }

    private fun end(id: Long, code: Int, message: String, detail: ByteArray? = null) {
        val e = JsonObject()
        e.addProperty("code", code)
        e.addProperty("message", message)
        if (detail != null && detail.isNotEmpty()) e.addProperty("detail", B64.encodeToString(detail))
        val o = JsonObject()
        o.addProperty("id", id)
        o.add("end", e)
        deps.send(o.toString())
    }

    private fun deny(id: Long, e: RelayLog, reply: Boolean = true) {
        deps.log(e)
        if (reply) end(id, PERMISSION_DENIED, "request not allowed")
    }

    private fun parseId(m: JsonObject): Long? {
        val p = m.get("id") as? JsonPrimitive ?: return null
        if (!p.isNumber) return null
        val d = p.asDouble
        if (d != Math.floor(d) || d < 1 || d > 4294967295.0) return null
        return d.toLong()
    }

    fun handle(message: String) {
        try {
            handleChecked(message)
        } catch (_: Exception) {
            deps.log(RelayLog("dropped", "malformed message"))
        }
    }

    private fun handleChecked(message: String) {
        if (message.length > MAX_MESSAGE_CHARS) return deny(0, RelayLog("dropped", "message too large"), false)
        val m = try {
            JsonParser.parseString(message).takeIf { it.isJsonObject }?.asJsonObject
        } catch (_: Exception) {
            null
        } ?: return deny(0, RelayLog("dropped", "not an object"), false)
        val id = parseId(m) ?: return deny(0, RelayLog("dropped", "bad id"), false)
        if (m.get("cancel")?.let { it.isJsonPrimitive && it.asJsonPrimitive.isBoolean && it.asBoolean } == true) {
            live.remove(id)?.cancel()
            return
        }
        if (live.containsKey(id)) return deny(id, RelayLog("dropped", "id in use"), false)
        if (live.size >= MAX_LIVE) return deny(id, RelayLog("dropped", "too many calls"))
        val al = deps.allowlist ?: return deny(id, RelayLog("denied", "no allowlist"))

        if (m.has("hostApi")) {
            val f = m.get("hostApi")
            if (f == null || !f.isJsonObject) return deny(id, RelayLog("dropped", "bad host api request"))
            val fo = f.asJsonObject
            val provider = fo.get("provider")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
            val method = fo.get("method")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
            val path = fo.get("path")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
            if (provider == null || method == null || path == null) return deny(id, RelayLog("dropped", "bad host api request"))
            val d = RelayChecks.checkHostApi(al, provider, method, path, fo.get("body"), ForgeScope(deps.remoteRepos()))
            if (d !is Decision.Allow) return deny(id, RelayLog("denied", (d as Decision.Deny).reason, "hostApi"))
            if (deps.hostApi[provider] == null || (d.confirm == Confirm.MODAL && deps.confirm == null)) {
                return deny(id, RelayLog("denied", "unavailable", "hostApi"))
            }
            val pf = d.parsed as Parsed.HostApi
            val repo = pf.params["o"]?.let { "$it/${pf.params["r"]}" }
            launch(id) { runHostApi(id, d, HostApiCall(provider, method, path, pf.body, repo)) }
            return
        }

        val method = m.get("method")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
        val bytes = m.get("bytes")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.let {
            try {
                B64D.decode(it.asString)
            } catch (_: IllegalArgumentException) {
                null
            }
        }
        if (method == null || bytes == null) return deny(id, RelayLog("dropped", "bad request"))
        val rootEl = m.get("root")
        if (rootEl != null && !rootEl.isJsonNull && !(rootEl.isJsonPrimitive && rootEl.asJsonPrimitive.isString)) {
            return deny(id, RelayLog("dropped", "bad root"))
        }
        val chosen = rootEl?.takeIf { it.isJsonPrimitive }?.asString ?: ""
        val roots = deps.roots()
        if (chosen != "" && chosen !in roots) return deny(id, RelayLog("denied", "root not opened", "rpc"))
        val d = RelayChecks.checkRpc(al, method, bytes, RunScope(chosen.ifEmpty { roots.firstOrNull() ?: "" }, deps.tmpDir))
        if (d !is Decision.Allow) {
            // The method name comes from the page; log it only when it is a known shape.
            val what = if (Regex("^asc\\.v1\\.[A-Za-z]+/[A-Za-z]+$").matches(method)) method else "rpc"
            return deny(id, RelayLog("denied", (d as Decision.Deny).reason, what))
        }
        if (d.confirm == Confirm.MODAL && deps.confirm == null) {
            return deny(id, RelayLog("denied", "no confirmation available", method))
        }
        launch(id) { runCall(id, d, method, bytes, chosen) }
    }

    private fun launch(id: Long, block: suspend () -> Unit) {
        val job = scope.launch {
            try {
                block()
            } finally {
                live.remove(id)
            }
        }
        live[id] = job
        if (job.isCompleted) live.remove(id)
    }

    private suspend fun confirmed(d: Decision.Allow): Boolean {
        if (d.confirm != Confirm.MODAL) return true
        val (title, targets) = RelayChecks.describe(d.parsed)
        return deps.confirm?.invoke(title, targets) == true
    }

    private suspend fun runCall(id: Long, d: Decision.Allow, method: String, original: ByteArray, root: String) {
        try {
            if (!confirmed(d)) return end(id, PERMISSION_DENIED, "not confirmed")
            deps.upstream.stream(method, d.bytes ?: original, root).collect { r ->
                val o = JsonObject()
                o.addProperty("id", id)
                o.addProperty("reply", B64.encodeToString(r))
                deps.send(o.toString())
            }
            end(id, 0, "")
        } catch (e: CancellationException) {
            throw e
        } catch (e: BridgeException) {
            end(id, e.code, e.message ?: "", e.errorDetail)
        } catch (_: Exception) {
            deps.log(RelayLog("upstream-error", "upstream failed", method))
            end(id, INTERNAL, "internal error")
        }
    }

    private suspend fun runHostApi(id: Long, d: Decision.Allow, call: HostApiCall) {
        try {
            if (!confirmed(d)) return end(id, PERMISSION_DENIED, "not confirmed")
            val r = deps.hostApi[call.provider]?.invoke(call) ?: return end(id, PERMISSION_DENIED, "host api is not available")
            val f = JsonObject()
            f.addProperty("status", r.status)
            f.add("body", r.body)
            val o = JsonObject()
            o.addProperty("id", id)
            o.add("hostApi", f)
            deps.send(o.toString())
        } catch (e: CancellationException) {
            throw e
        } catch (_: Exception) {
            end(id, INTERNAL, "host api request failed")
        }
    }

    fun close() {
        for (j in live.values) j.cancel()
        live.clear()
    }
}
