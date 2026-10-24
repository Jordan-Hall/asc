package asc.asc.ui

import asc.v1.Bridge.RunRequest
import asc.v1.Workspace.WsStageRequest
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.google.protobuf.ByteString
import asc.asc.bridge.BridgeException
import java.awt.Color
import java.io.ByteArrayInputStream
import java.net.InetAddress
import java.nio.file.Files
import java.nio.file.Path
import java.util.Base64
import java.util.concurrent.CopyOnWriteArrayList
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.flow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RelayTest {
    private val json = Path.of("..", "..", "packages", "asc-client-ts", "src", "transport", "allowlist.json")
    private val al: Allowlist = Allowlist.parse(Files.readString(json))
    private val root: String = Files.createTempDirectory("asc-relay").toString()
    private val scope = RunScope(root, "$root/tmp")

    private fun run(verb: String, vararg args: String) = RelayChecks.checkRun(al, verb, args.toList(), scope)

    private fun ok(d: Decision) = assertTrue(d.toString(), d is Decision.Allow)

    private fun denied(d: Decision, why: String? = null) {
        assertTrue(d is Decision.Deny)
        if (why != null) assertEquals(why, (d as Decision.Deny).reason)
    }

    // ---- the checked-in allowlist ----

    @Test fun realAllowlistParses() {
        assertTrue(al.read.contains("asc.v1.Workspace/Status"))
        assertTrue(al.verbs.containsKey("stack get"))
        assertEquals(Confirm.MODAL, al.verbs["submit"]!!.confirm)
        assertEquals(Confirm.MODAL, al.write["asc.v1.Changes/RequestMerge"]!!.confirm)
        assertTrue(al.pairs.any { it.path.endsWith("/merge") && it.confirm == Confirm.MODAL })
    }

    @Test fun neverEntriesAreNotReadable() {
        for (m in listOf("asc.v1.Workspace/Shutdown", "asc.v1.Bridge/Token", "asc.v1.Bridge/StoreToken", "asc.v1.Changes/RestackChanges")) {
            assertFalse(m, m in al.read)
            assertFalse(m, al.write.containsKey(m))
        }
    }

    @Test fun wrongVersionAndBadConfirmAreRefused() {
        for (bad in listOf("{}", """{"version":2,"rpc":{},"run":{},"forgejo":{}}""")) {
            try {
                Allowlist.parse(bad)
                throw AssertionError(bad)
            } catch (_: IllegalArgumentException) {
            }
        }
        val txt = Files.readString(json).replaceFirst("\"modal\"", "\"sometimes\"")
        try {
            Allowlist.parse(txt)
            throw AssertionError("confirm")
        } catch (_: IllegalArgumentException) {
        }
    }

    // ---- Run verbs ----

    @Test fun allowedVerbs() {
        ok(run("up"))
        ok(run("up", "3"))
        ok(run("restack", "abc123"))
        ok(run("submit", "--draft", "--reviewer=alice", "--reviewer=bob"))
        ok(run("create", "-m=hello", "a.txt"))
        ok(run("stack get", "--worktree=wt", "main"))
    }

    @Test fun unknownVerbFlagAndRejectedFlagsAreDenied() {
        denied(run("nope"), "verb not allowed")
        denied(run("up", "--force"), "flag rejected")
        denied(run("sync", "-c"), "flag rejected")
        denied(run("sync", "--exec=sh"), "flag rejected")
        denied(run("sync", "--bogus"), "unknown flag")
        denied(run("sync", "--", "x"), "unknown flag")
        denied(run("undo", "extra"), "too many arguments")
    }

    @Test fun flagValueRules() {
        denied(run("submit", "--draft=1"), "flag takes no value")
        denied(run("create", "-m"), "flag needs a value")
        denied(run("create", "-m=a", "-m=b"), "flag given twice")
        denied(run("up", "101"), "bad argument")
        denied(run("up", "x"), "bad argument")
        denied(run("submit", "--reviewer=bad name"), "bad flag value")
        denied(run("switch", "-x"), "unknown flag")
        denied(run("op log", "-n=501"), "bad flag value")
        denied(run("create", "-m=" + "a".repeat(65537)), "bad flag value")
    }

    @Test fun revsCannotLookLikeFlagsOrHoldControlBytes() {
        denied(run("switch", "-rf"), "unknown flag")
        denied(run("switch", "a\nb"), "bad argument")
        denied(run("switch", "a\u0000b"), "bad argument")
        denied(run("switch"), "missing argument")
    }

    @Test fun pathsMustStayInsideTheRoot() {
        ok(run("create", "src/a.txt"))
        denied(run("create", "../outside"), "bad argument")
        denied(run("create", "a/../../outside"), "bad argument")
        denied(run("stack get", "--worktree=../x", "main"), "bad flag value")
    }

    @Test fun tmpFilesMustBeInsideTheSessionDirectory() {
        ok(run("reorder", "--todo=$root/tmp/plan.txt"))
        denied(run("reorder", "--todo=$root/other/plan.txt"), "bad flag value")
        denied(run("reorder", "--todo=plan.txt"), "bad flag value")
        denied(RelayChecks.checkRun(al, "reorder", listOf("--todo=$root/tmp/p"), RunScope(root, null)), "bad flag value")
    }

    @Test fun modalVerbsAskForConfirmation() {
        assertEquals(Confirm.MODAL, (run("hide", "abc") as Decision.Allow).confirm)
        assertEquals(Confirm.NONE, (run("undo") as Decision.Allow).confirm)
        assertEquals(Confirm.MODAL, (run("push") as Decision.Allow).confirm)
    }

    @Test fun tooManyArgumentsAreDenied() {
        denied(run("hide", *Array(65) { "a$it" }), "too many arguments")
    }

    // ---- RPCs ----

    private fun runBytes(verb: String, vararg args: String, rootField: String = "") =
        RunRequest.newBuilder().setVerb(verb).addAllArgs(args.toList()).setRoot(rootField).build().toByteArray()

    @Test fun runRpcStripsTheRootField() {
        val d = RelayChecks.checkRpc(al, "asc.v1.Bridge/Run", runBytes("up", "2", rootField = "/etc"), scope) as Decision.Allow
        assertEquals("", RunRequest.parseFrom(d.bytes).root)
    }

    @Test fun readsPassWritesAndNeversDoNot() {
        ok(RelayChecks.checkRpc(al, "asc.v1.Workspace/Status", ByteArray(0), scope))
        denied(RelayChecks.checkRpc(al, "asc.v1.Workspace/Shutdown", ByteArray(0), scope), "rpc not allowed")
        denied(RelayChecks.checkRpc(al, "asc.v1.Bridge/Token", ByteArray(0), scope), "rpc not allowed")
        denied(RelayChecks.checkRpc(al, "Anything/Else", ByteArray(0), scope), "rpc not allowed")
        denied(RelayChecks.checkRpc(al, "asc.v1.Bridge/Run", byteArrayOf(0x0a, 0x7f), scope), "undecodable request")
    }

    @Test fun stageLimitsAreEnforced() {
        val small = WsStageRequest.newBuilder().addAdd(ByteString.copyFromUtf8("a")).setPatch(ByteString.copyFromUtf8("p")).build()
        val d = RelayChecks.checkRpc(al, "asc.v1.Workspace/Stage", small.toByteArray(), scope) as Decision.Allow
        assertEquals(Confirm.NONE, d.confirm)
        val big = WsStageRequest.newBuilder().setPatch(ByteString.copyFrom(ByteArray(1_048_577))).build()
        denied(RelayChecks.checkRpc(al, "asc.v1.Workspace/Stage", big.toByteArray(), scope), "patch too large")
        val many = WsStageRequest.newBuilder().apply { repeat(10_001) { addAdd(ByteString.copyFromUtf8("f$it")) } }.build()
        denied(RelayChecks.checkRpc(al, "asc.v1.Workspace/Stage", many.toByteArray(), scope), "too many paths")
    }

    @Test fun requestMergeNeedsAModal() {
        val d = RelayChecks.checkRpc(al, "asc.v1.Changes/RequestMerge", ByteArray(0), scope) as Decision.Allow
        assertEquals(Confirm.MODAL, d.confirm)
    }

    // ---- Forgejo ----

    private val forge = ForgeScope(listOf("o/r"))

    private fun fj(method: String, path: String, body: String? = null) =
        RelayChecks.checkForgejo(al, method, path, body?.let { JsonParser.parseString(it) }, forge)

    @Test fun forgejoPairsMatch() {
        ok(fj("GET", "/user"))
        ok(fj("GET", "/repos/o/r/pulls/7/files?page=2"))
        ok(fj("PATCH", "/repos/o/r/pulls/7", """{"title":"t","draft":true}"""))
        val merge = fj("POST", "/repos/o/r/pulls/7/merge", """{"Do":"squash"}""") as Decision.Allow
        assertEquals(Confirm.MODAL, merge.confirm)
    }

    @Test fun forgejoDeniesTheRest() {
        denied(fj("DELETE", "/repos/o/r/pulls/7"), "forgejo operation not allowed")
        denied(fj("GET", "/admin/users"), "forgejo operation not allowed")
        denied(fj("GET", "/repos/o/other/pulls/7"), "repo not opened")
        denied(fj("GET", "/repos/o/r/pulls/x"), "forgejo operation not allowed")
        denied(fj("GET", "/repos/o/r/pulls/7/../8"), "bad path")
        denied(fj("GET", "/repos/o/r/pulls/7%2F"), "bad path")
        denied(fj("GET", "/user?a=<b>"), "bad query")
        denied(fj("GET", "/user", "{}"), "no body allowed")
        denied(fj("PATCH", "/repos/o/r/pulls/7", """{"state":"closed"}"""), "body field not allowed")
        denied(fj("POST", "/repos/o/r/pulls/7/reviews", """{"event":"APPROVE"}"""), "pending review only")
        denied(fj("POST", "/repos/o/r/pulls/7/merge", """{"Do":"delete-branch"}"""), "bad merge mode")
        denied(fj("PATCH", "/repos/o/r", """{"private":false}"""), "forgejo operation not allowed")
    }

    @Test fun confirmationTextComesFromTheParsedRequest() {
        val (title, targets) = RelayChecks.describe((fj("POST", "/repos/o/r/pulls/7/merge", """{"Do":"merge"}""") as Decision.Allow).parsed)
        assertEquals("Merge pull request", title)
        assertTrue(targets.contains("o=o"))
        assertEquals("Run \"asc hide\"", RelayChecks.describe(Parsed.Run("hide", listOf("abc"))).first)
    }

    // ---- the relay ----

    private class Rig(val al: Allowlist, root: String, confirm: Boolean? = null, upstream: RelayUpstream? = null) {
        val sent = CopyOnWriteArrayList<JsonObject>()
        val logs = CopyOnWriteArrayList<RelayLog>()
        val upstreamCalls = CopyOnWriteArrayList<Triple<String, ByteArray, String>>()
        val relay = AscRelay(
            CoroutineScope(Dispatchers.Unconfined),
            RelayDeps(
                upstream = upstream ?: RelayUpstream { m, r, rt ->
                    upstreamCalls.add(Triple(m, r, rt))
                    flow { emit("one".toByteArray()); emit("two".toByteArray()) }
                },
                allowlist = al,
                send = { sent.add(JsonParser.parseString(it).asJsonObject) },
                roots = { listOf(root) },
                forgeRepos = { listOf("o/r") },
                tmpDir = null,
                confirm = confirm?.let { answer ->
                    val f: suspend (String, List<String>) -> Boolean = { _, _ -> answer }
                    f
                },
                forgejo = { ForgejoResult(200, JsonParser.parseString("""{"ok":true}""")) },
                log = { logs.add(it) },
            ),
        )
    }

    private fun b64(b: ByteArray) = Base64.getEncoder().encodeToString(b)

    private fun msg(id: Int, method: String, bytes: ByteArray, root: String? = null): String {
        val o = JsonObject()
        o.addProperty("id", id)
        o.addProperty("method", method)
        o.addProperty("bytes", b64(bytes))
        if (root != null) o.addProperty("root", root)
        return o.toString()
    }

    @Test fun relayForwardsAllowedReadsAndEndsTheStream() {
        val rig = Rig(al, root)
        rig.relay.handle(msg(1, "asc.v1.Workspace/Status", ByteArray(0)))
        assertEquals(3, rig.sent.size)
        assertEquals("one", String(Base64.getDecoder().decode(rig.sent[0].get("reply").asString)))
        assertEquals(0, rig.sent[2].getAsJsonObject("end").get("code").asInt)
        assertEquals("asc.v1.Workspace/Status", rig.upstreamCalls[0].first)
    }

    @Test fun relayDeniesAndLogsWithoutPayload() {
        val rig = Rig(al, root)
        rig.relay.handle(msg(2, "asc.v1.Workspace/Shutdown", ByteArray(0)))
        assertEquals(AscRelay.PERMISSION_DENIED, rig.sent.single().getAsJsonObject("end").get("code").asInt)
        assertTrue(rig.upstreamCalls.isEmpty())
        assertEquals("denied", rig.logs.single().event)
        // A hostile method name is not logged.
        rig.relay.handle(msg(3, "evil\nname", ByteArray(0)))
        assertEquals("rpc", rig.logs.last().what)
    }

    @Test fun relayDropsGarbageAndBadIds() {
        val rig = Rig(al, root)
        for (m in listOf("not json", "[]", "{}", """{"id":0}""", """{"id":-1,"method":"x"}""", """{"id":1.5}""", """{"id":"1"}""")) rig.relay.handle(m)
        assertTrue(rig.sent.isEmpty())
        assertTrue(rig.upstreamCalls.isEmpty())
        assertEquals(7, rig.logs.size)
    }

    @Test fun relayRefusesAnUnopenedRoot() {
        val rig = Rig(al, root)
        rig.relay.handle(msg(1, "asc.v1.Workspace/Status", ByteArray(0), root = "/somewhere/else"))
        assertEquals(AscRelay.PERMISSION_DENIED, rig.sent.single().getAsJsonObject("end").get("code").asInt)
        rig.relay.handle(msg(2, "asc.v1.Workspace/Status", ByteArray(0), root = root))
        assertEquals(root, rig.upstreamCalls.single().third)
    }

    @Test fun relayWithoutAnAllowlistDeniesEverything() {
        val rig = Rig(al, root)
        val none = AscRelay(
            CoroutineScope(Dispatchers.Unconfined),
            RelayDeps(RelayUpstream { _, _, _ -> flow { } }, null, { rig.sent.add(JsonParser.parseString(it).asJsonObject) }, { listOf(root) }, { emptyList() }),
        )
        none.handle(msg(1, "asc.v1.Workspace/Status", ByteArray(0)))
        assertEquals(AscRelay.PERMISSION_DENIED, rig.sent.single().getAsJsonObject("end").get("code").asInt)
    }

    @Test fun modalCallsWaitForConfirmation() {
        val yes = Rig(al, root, confirm = true)
        yes.relay.handle(msg(1, "asc.v1.Bridge/Run", runBytes("hide", "abc")))
        assertEquals("asc.v1.Bridge/Run", yes.upstreamCalls.single().first)
        val no = Rig(al, root, confirm = false)
        no.relay.handle(msg(1, "asc.v1.Bridge/Run", runBytes("hide", "abc")))
        assertTrue(no.upstreamCalls.isEmpty())
        assertEquals("not confirmed", no.sent.single().getAsJsonObject("end").get("message").asString)
        val none = Rig(al, root, confirm = null)
        none.relay.handle(msg(1, "asc.v1.Bridge/Run", runBytes("hide", "abc")))
        assertTrue(none.upstreamCalls.isEmpty())
        assertEquals(AscRelay.PERMISSION_DENIED, none.sent.single().getAsJsonObject("end").get("code").asInt)
    }

    @Test fun bridgeErrorsKeepTheirCode() {
        val rig = Rig(al, root, upstream = RelayUpstream { _, _, _ -> flow { throw BridgeException(5, "nope", byteArrayOf(1, 2)) } })
        rig.relay.handle(msg(1, "asc.v1.Workspace/Status", ByteArray(0)))
        val end = rig.sent.single().getAsJsonObject("end")
        assertEquals(5, end.get("code").asInt)
        assertEquals("nope", end.get("message").asString)
        assertEquals(b64(byteArrayOf(1, 2)), end.get("detail").asString)
    }

    @Test fun unexpectedErrorsAreNotLeakedToThePage() {
        val rig = Rig(al, root, upstream = RelayUpstream { _, _, _ -> flow { throw IllegalStateException("secret path C:/x") } })
        rig.relay.handle(msg(1, "asc.v1.Workspace/Status", ByteArray(0)))
        val end = rig.sent.single().getAsJsonObject("end")
        assertEquals(AscRelay.INTERNAL, end.get("code").asInt)
        assertFalse(end.get("message").asString.contains("secret"))
    }

    @Test fun forgejoRunsThroughTheExecutor() {
        val rig = Rig(al, root)
        rig.relay.handle("""{"id":4,"forgejo":{"method":"GET","path":"/user"}}""")
        val f = rig.sent.single().getAsJsonObject("forgejo")
        assertEquals(200, f.get("status").asInt)
        rig.relay.handle("""{"id":5,"forgejo":{"method":"DELETE","path":"/repos/o/r/pulls/1"}}""")
        assertEquals(AscRelay.PERMISSION_DENIED, rig.sent.last().getAsJsonObject("end").get("code").asInt)
    }

    @Test fun forgejoClientOnlyTalksHttpsToTheForge() {
        assertNotNull(AscForgejoClient.url("https://forge.example/", "/user"))
        assertEquals("https://forge.example/api/v1/user", AscForgejoClient.url("https://forge.example/", "/user").toString())
        assertNull(AscForgejoClient.url("http://forge.example", "/user"))
        assertNull(AscForgejoClient.url("https://u:p@forge.example", "/user"))
    }

    // ---- avatars ----

    private fun ip(s: String) = InetAddress.getByName(s)

    @Test fun onlyPublicAddressesAreFetched() {
        for (a in listOf("127.0.0.1", "10.0.0.1", "172.16.0.1", "192.168.1.1", "169.254.1.1", "100.64.0.1", "0.0.0.0", "224.0.0.1", "::1", "fc00::1", "fe80::1", "2001:db8::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1")) {
            assertFalse(a, AscAvatar.isPublic(ip(a)))
        }
        for (a in listOf("8.8.8.8", "1.1.1.1", "2606:4700:4700::1111")) assertTrue(a, AscAvatar.isPublic(ip(a)))
    }

    private fun png(n: Int = 10) = ByteArray(n) { 1 }

    private fun avatar(
        statuses: Map<String, AscAvatar.AvatarResponse>,
        resolve: (String) -> List<InetAddress> = { listOf(ip("8.8.8.8")) },
        log: MutableList<String> = mutableListOf(),
    ) = AscAvatar(resolve, { uri, _ -> log.add(uri.toString()); statuses[uri.toString()] ?: AscAvatar.AvatarResponse(404, emptyMap(), ByteArrayInputStream(ByteArray(0))) })

    private fun img(type: String, body: ByteArray, status: Int = 200, extra: Map<String, String> = emptyMap()) =
        AscAvatar.AvatarResponse(status, mapOf("content-type" to type) + extra, ByteArrayInputStream(body))

    @Test fun avatarBecomesADataUri() {
        val a = avatar(mapOf("https://img.example/a.png" to img("image/png", png())))
        val uri = a.get("https://img.example/a.png")!!
        assertTrue(uri.startsWith("data:image/png;base64,"))
        assertEquals(1, a.size)
    }

    @Test fun avatarRefusals() {
        assertNull(avatar(emptyMap()).get("http://img.example/a.png"))
        assertNull(avatar(emptyMap()).get("https://user:pw@img.example/a.png"))
        assertNull(avatar(emptyMap()).get("https://127.0.0.1/a.png"))
        assertNull(avatar(emptyMap()).get("https://[::1]/a.png"))
        assertNull(avatar(emptyMap()).get("https://127.1/a.png"))
        assertNull(avatar(emptyMap(), resolve = { listOf(ip("8.8.8.8"), ip("10.0.0.1")) }).get("https://img.example/a.png"))
        assertNull(avatar(mapOf("https://img.example/a.svg" to img("image/svg+xml", png())), ).get("https://img.example/a.svg"))
        assertNull(avatar(mapOf("https://img.example/big" to img("image/png", png(AscAvatar.MAX_BYTES + 1)))).get("https://img.example/big"))
        assertNull(avatar(mapOf("https://img.example/big" to img("image/png", png(10), extra = mapOf("content-length" to "999999")))).get("https://img.example/big"))
        assertNull(avatar(mapOf("https://img.example/x" to img("image/png", png(), status = 500))).get("https://img.example/x"))
        assertNull(AscAvatar().get("not a url"))
    }

    @Test fun avatarRedirectsAreRecheckedAndCapped() {
        val ok = avatar(
            mapOf(
                "https://a.example/1" to img("text/plain", ByteArray(0), 302, mapOf("location" to "https://b.example/2")),
                "https://b.example/2" to img("image/gif", png()),
            ),
        )
        assertNotNull(ok.get("https://a.example/1"))
        val toPrivate = avatar(
            mapOf("https://a.example/1" to img("text/plain", ByteArray(0), 302, mapOf("location" to "https://127.0.0.1/x"))),
        )
        assertNull(toPrivate.get("https://a.example/1"))
        val toHttp = avatar(
            mapOf("https://a.example/1" to img("text/plain", ByteArray(0), 302, mapOf("location" to "http://b.example/x"))),
        )
        assertNull(toHttp.get("https://a.example/1"))
        val loop = avatar(
            (1..6).associate { "https://a.example/$it" to img("text/plain", ByteArray(0), 302, mapOf("location" to "https://a.example/${it + 1}")) },
        )
        assertNull(loop.get("https://a.example/1"))
    }

    @Test fun avatarCacheIsLruAndBounded() {
        val calls = mutableListOf<String>()
        val map = (0 until 205).associate { "https://img.example/$it" to img("image/png", png()) }
        // Each response is a one-shot stream, so use a fetcher that builds a fresh one.
        val a = AscAvatar({ listOf(ip("8.8.8.8")) }, { uri, _ -> calls.add(uri.toString()); img("image/png", png()) })
        for (k in map.keys) a.get(k)
        assertEquals(AscAvatar.CACHE_ENTRIES, a.size)
        assertTrue(a.cachedBytes <= AscAvatar.CACHE_BYTES)
        calls.clear()
        a.get("https://img.example/204")
        assertTrue(calls.isEmpty())
        a.get("https://img.example/0")
        assertEquals(1, calls.size)
    }

    // ---- theme ----

    private class Fake(val colors: Map<String, Color>, override val dark: Boolean = true, val family: String? = "Inter", val mono: String? = "JetBrains Mono") : ThemeSource {
        override fun color(key: String) = colors[key]
        override fun fontFamily(key: String) = if (key == "editor.font") mono else family
        override fun fontSizePx(key: String) = 13
    }

    @Test fun themeMapsOnlyDefinedColours() {
        val t = AscTheme.tokens(Fake(mapOf("Panel.background" to Color(0x1e, 0x1f, 0x22), "Label.foreground" to Color(0xbc, 0xbe, 0xc4, 128))))
        assertEquals("#1e1f22", t["--asc-bg"])
        assertEquals("rgba(188, 190, 196, 0.502)", t["--asc-fg"])
        assertFalse(t.containsKey("--asc-accent"))
        assertEquals("Inter, system-ui, sans-serif", t["--asc-font-family"])
        assertEquals("13px", t["--asc-font-size"])
    }

    @Test fun themeFirstDefinedKeyWinsAndUnsafeFontsAreDropped() {
        val t = AscTheme.tokens(Fake(mapOf("Tree.hoverBackground" to Color.RED), family = "Evil; } body{", mono = null))
        assertEquals("#ff0000", t["--asc-hover-bg"])
        assertFalse(t.containsKey("--asc-font-family"))
        assertFalse(t.containsKey("--asc-font-mono"))
    }

    @Test fun themeMessageShape() {
        val o = JsonParser.parseString(AscTheme.message(Fake(mapOf("Panel.background" to Color.BLACK)))).asJsonObject.getAsJsonObject("theme")
        assertTrue(o.get("dark").asBoolean)
        assertEquals("#000000", o.getAsJsonObject("tokens").get("--asc-bg").asString)
    }

    // ---- page and assets ----

    @Test fun pageHasAStrictCspAndEscapesTheRoot() {
        assertTrue(AscPage.CSP.contains("connect-src 'none'"))
        assertTrue(AscPage.CSP.contains("default-src 'none'"))
        assertFalse(AscPage.CSP.contains("unsafe"))
        val html = AscPage.html("graph", "C:/a\"><script>x</script>", true)
        assertFalse(html.contains("<script>x"))
        assertTrue(html.contains("data-asc-view=\"graph\""))
        assertTrue(AscPage.html("<bad>", "r", false).contains("data-asc-view=\"smartlog\""))
    }

    @Test fun assetPathsAreChecked() {
        assertEquals("index.js", AscAssets.resolve("/index.js"))
        assertEquals("fonts/codicon.woff2", AscAssets.resolve("fonts/codicon.woff2"))
        for (bad in listOf("../x.js", "a/../x.js", "/etc/passwd", "a\\b.js", "%2e%2e/x.js", ".hidden.js", "x.exe", "", "a//b.js", "x.js\u0000")) {
            assertNull(bad, AscAssets.resolve(bad))
        }
        assertEquals("text/javascript", AscAssets.mime("index.js"))
    }

    // ---- idle disposal ----

    @Test fun idleTimerFiresOnlyWhileHidden() {
        var fired = 0
        val tasks = mutableListOf<Pair<Runnable, BooleanArray>>()
        val t = IdleTimer(
            600_000,
            { ms, r ->
                assertEquals(600_000L, ms)
                val cancelled = BooleanArray(1)
                tasks.add(r to cancelled)
                AutoCloseable { cancelled[0] = true }
            },
            { fired++ },
        )
        t.hidden()
        t.hidden()
        assertEquals(1, tasks.size)
        t.shown()
        assertTrue(tasks[0].second[0])
        t.hidden()
        assertEquals(2, tasks.size)
        tasks[1].first.run()
        assertEquals(1, fired)
        t.hidden()
        assertEquals(3, tasks.size)
    }
}
