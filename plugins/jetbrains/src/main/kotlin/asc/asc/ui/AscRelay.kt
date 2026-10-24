package asc.asc.ui

import asc.v1.Bridge.RunRequest
import asc.v1.Changes.RequestMergeRequest
import asc.v1.Workspace.WsStageRequest
import com.google.gson.JsonArray
import com.google.gson.JsonElement
import com.google.gson.JsonNull
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.google.gson.JsonPrimitive
import asc.asc.bridge.BridgeException
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.nio.file.InvalidPathException
import java.nio.file.Path
import java.time.Duration
import java.util.Base64
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

// The JetBrains side of the asc-ui relay (SPEC 24.4.5, 24.4.5.1, 24.5.2, IP-D4). It is a port of
// packages/asc-client-ts/src/transport/relay.ts: untrusted JSON messages from the JCEF page are forwarded to the
// bridge only when they match an entry of allowlist.json. Deny by default: an unknown method, a `never` or
// unclassified RPC, a verb or flag outside the schema, a Forgejo pair outside the list, a body field outside the
// pair's list or a request over a limit is dropped, logged without its payload and answered PERMISSION_DENIED.
// The confirmation text is composed here from the parsed request, never taken from the page. The page never
// sees a token: the Forgejo executor adds it.
//
// TODO(P48): `Allowlist.kt` is meant to be generated from allowlist.json by the Gradle task `generateAllowlist`.
// Until that task exists this file parses the same json at runtime ([Allowlist.parse]) from the classpath
// resource [Allowlist.RESOURCE]; when the resource is missing the relay denies everything.
//
// Wire (JSON strings, bytes as standard base64). Page to host: {id, method, bytes, root?}, {id, cancel:true},
// {id, forgejo:{method, path, body?}}, {id, avatar:url}, {id, host:{op, ...}}. Host to page: {id, reply},
// {id, end:{code, message, detail?}}, {id, forgejo:{status, body}}, {id, avatar}, {theme:{...}}.

enum class Confirm { NONE, MODAL }

class WriteEntry(val name: String, val confirm: Confirm, val patchBytes: Long?, val paths: Long?)

class VerbEntry(val flags: Map<String, String>, val pos: String, val confirm: Confirm)

class ForgejoPair(val method: String, val path: String, val body: List<String>?, val confirm: Confirm)

/** The checked form of allowlist.json. */
class Allowlist(
    val read: Set<String>,
    val write: Map<String, WriteEntry>,
    val reject: Set<String>,
    val maxArgs: Int,
    val maxArgBytes: Int,
    val verbs: Map<String, VerbEntry>,
    val pairs: List<ForgejoPair>,
) {
    companion object {
        const val RESOURCE = "/asc/asc/allowlist.json"

        /** Parses and checks allowlist.json; throws [IllegalArgumentException] on anything odd. */
        fun parse(text: String): Allowlist {
            val root = obj(JsonParser.parseString(text), "allowlist")
            require(root.get("version")?.asIntOrNull() == 1) { "allowlist: version 1 expected" }
            val rpc = obj(root.get("rpc"), "rpc")
            val run = obj(root.get("run"), "run")
            val forgejo = obj(root.get("forgejo"), "forgejo")
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
            val pairs = arr(forgejo.get("pairs")).map {
                val o = obj(it, "pair")
                ForgejoPair(
                    str(o.get("m"), "pair method"),
                    str(o.get("p"), "pair path"),
                    o.get("body")?.let { b -> arr(b).map { s -> str(s, "pair body") } },
                    conf(o.get("confirm"), "pair"),
                )
            }
            return Allowlist(
                read = arr(rpc.get("read")).map { str(it, "rpc.read") }.toSet(),
                write = write,
                reject = arr(run.get("reject")).map { str(it, "run.reject") }.toSet(),
                maxArgs = run.get("maxArgs")?.asIntOrNull() ?: throw IllegalArgumentException("allowlist: run limits missing"),
                maxArgBytes = run.get("maxArgBytes")?.asIntOrNull() ?: throw IllegalArgumentException("allowlist: run limits missing"),
                verbs = verbs,
                pairs = pairs,
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

    data class Forgejo(val method: String, val template: String, val params: Map<String, String>, val body: JsonObject?) : Parsed
}

sealed interface Decision {
    class Allow(val parsed: Parsed, val confirm: Confirm, val bytes: ByteArray? = null) : Decision

    class Deny(val reason: String) : Decision
}

class RunScope(val root: String, val tmpDir: String? = null)

class ForgeScope(val repos: List<String>)

/** The pure checks. Mirrors relay.ts `checkRun`, `checkRpc` and `checkForgejo`; the bridge stays the authority. */
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

    fun checkForgejo(al: Allowlist, method: String, path: String, body: JsonElement?, scope: ForgeScope): Decision {
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
        for (pair in al.pairs) {
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
                for (k in b.keySet()) if (k !in pair.body) return no("body field not allowed")
                if (pair.path.endsWith("/reviews") && b.has("event")) return no("pending review only")
                if (b.has("Do")) {
                    val d = b.get("Do")
                    if (!d.isJsonPrimitive || !d.asJsonPrimitive.isString || d.asString !in MERGE_DO) return no("bad merge mode")
                }
                parsedBody = b
            } else if (hasBody) {
                return no("no body allowed")
            }
            return Decision.Allow(Parsed.Forgejo(pair.method, pair.path, params, parsedBody), pair.confirm)
        }
        return no("forgejo operation not allowed")
    }

    /** Native confirmation text, composed only from the parsed request. */
    fun describe(p: Parsed): Pair<String, List<String>> = when (p) {
        is Parsed.Run -> "Run \"asc ${p.verb}\"" to p.args.map { if (it.length > 120) it.take(117) + "..." else it }
        is Parsed.Write -> (if (p.method.endsWith("RequestMerge")) "Merge change" else "Stage changes") to
            listOf(p.change ?: "${p.paths} path(s)")
        is Parsed.Forgejo -> {
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

class ForgejoCall(val method: String, val path: String, val body: JsonObject?, val repo: String?)

class ForgejoResult(val status: Int, val body: JsonElement)

class RelayLog(val event: String, val reason: String, val what: String? = null)

class RelayDeps(
    val upstream: RelayUpstream,
    val allowlist: Allowlist?,
    /** Sends one JSON message to the page. */
    val send: (String) -> Unit,
    /** Absolute roots the user opened; a call's root must be one of them (or empty). */
    val roots: () -> List<String>,
    /** `owner/repo` of the forge repos of the opened roots. */
    val forgeRepos: () -> List<String>,
    val tmpDir: String? = null,
    /** Shows the native modal. Without it, every entry that needs confirmation is denied. */
    val confirm: (suspend (title: String, targets: List<String>) -> Boolean)? = null,
    /** Runs a checked Forgejo request and adds the token; without it Forgejo requests are denied. */
    val forgejo: (suspend (ForgejoCall) -> ForgejoResult)? = null,
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

        if (m.has("forgejo")) {
            val f = m.get("forgejo")
            if (f == null || !f.isJsonObject) return deny(id, RelayLog("dropped", "bad forgejo request"))
            val fo = f.asJsonObject
            val method = fo.get("method")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
            val path = fo.get("path")?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isString }?.asString
            if (method == null || path == null) return deny(id, RelayLog("dropped", "bad forgejo request"))
            val d = RelayChecks.checkForgejo(al, method, path, fo.get("body"), ForgeScope(deps.forgeRepos()))
            if (d !is Decision.Allow) return deny(id, RelayLog("denied", (d as Decision.Deny).reason, "forgejo"))
            if (deps.forgejo == null || (d.confirm == Confirm.MODAL && deps.confirm == null)) {
                return deny(id, RelayLog("denied", "unavailable", "forgejo"))
            }
            val pf = d.parsed as Parsed.Forgejo
            val repo = pf.params["o"]?.let { "$it/${pf.params["r"]}" }
            launch(id) { runForgejo(id, d, ForgejoCall(method, path, pf.body, repo)) }
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

    private suspend fun runForgejo(id: Long, d: Decision.Allow, call: ForgejoCall) {
        try {
            if (!confirmed(d)) return end(id, PERMISSION_DENIED, "not confirmed")
            val r = deps.forgejo?.invoke(call) ?: return end(id, PERMISSION_DENIED, "forgejo is not available")
            val f = JsonObject()
            f.addProperty("status", r.status)
            f.add("body", r.body)
            val o = JsonObject()
            o.addProperty("id", id)
            o.add("forgejo", f)
            deps.send(o.toString())
        } catch (e: CancellationException) {
            throw e
        } catch (_: Exception) {
            end(id, INTERNAL, "forgejo request failed")
        }
    }

    fun close() {
        for (j in live.values) j.cancel()
        live.clear()
    }
}

/**
 * Runs a Forgejo request that the relay already matched against the allowlist (SPEC 24.4.5.1, 24.6). The token
 * comes from `Bridge.Token` through [token], is attached here as a bearer header and is never logged, never
 * put in a URL and never returned to the page. https only, no redirects (a redirect could carry the token to
 * another origin), the host must be the forge host of an opened root, and the reply is capped.
 */
class AscForgejoClient(
    /** `owner/repo` to forge URL of the opened roots (credentials already removed by the bridge). */
    private val forges: () -> Map<String, String>,
    private val token: suspend (host: String) -> String?,
    private val send: (HttpRequest) -> HttpResponse<java.io.InputStream> = { client.send(it, HttpResponse.BodyHandlers.ofInputStream()) },
) {
    companion object {
        const val MAX_BODY = 8 * 1024 * 1024
        private val client: HttpClient by lazy {
            HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NEVER).connectTimeout(Duration.ofSeconds(10)).build()
        }

        /** The request URL, or null when the forge URL is not an https origin without credentials. */
        fun url(forge: String, path: String): URI? {
            val base = try {
                URI(forge.trimEnd('/'))
            } catch (_: Exception) {
                return null
            }
            if (base.scheme != "https" || base.host == null || base.userInfo != null || base.rawQuery != null || base.rawFragment != null) {
                return null
            }
            val full = try {
                URI(base.toString() + "/api/v1" + path)
            } catch (_: Exception) {
                return null
            }
            return if (full.host == base.host && full.port == base.port && full.scheme == "https") full else null
        }
    }

    suspend fun call(c: ForgejoCall): ForgejoResult {
        val map = forges()
        val forge = (c.repo?.let { map[it] } ?: map.values.firstOrNull()) ?: throw IllegalStateException("no forge")
        val uri = url(forge, c.path) ?: throw IllegalStateException("forge is not https")
        val bearer = token(uri.host) ?: return ForgejoResult(401, JsonNull.INSTANCE)
        return withContext(Dispatchers.IO) {
            val b = HttpRequest.newBuilder(uri).timeout(Duration.ofSeconds(30))
                .header("accept", "application/json").header("authorization", "Bearer $bearer")
            val body = c.body
            val req = if (body == null) {
                b.method(c.method, HttpRequest.BodyPublishers.noBody())
            } else {
                b.header("content-type", "application/json").method(c.method, HttpRequest.BodyPublishers.ofString(body.toString()))
            }.build()
            val res = send(req)
            res.body().use { input ->
                val buf = java.io.ByteArrayOutputStream()
                val chunk = ByteArray(8192)
                while (true) {
                    val n = input.read(chunk)
                    if (n < 0) break
                    if (buf.size() + n > MAX_BODY) throw IllegalStateException("reply too large")
                    buf.write(chunk, 0, n)
                }
                val text = String(buf.toByteArray(), Charsets.UTF_8)
                val el: JsonElement = if (text.isBlank()) {
                    JsonNull.INSTANCE
                } else {
                    try {
                        JsonParser.parseString(text)
                    } catch (_: Exception) {
                        com.google.gson.JsonPrimitive(text)
                    }
                }
                ForgejoResult(res.statusCode(), el)
            }
        }
    }
}