package asc.asc.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AscSigningConfigurableTest {
    private val base = SigningForm("", "openpgp", false, false)

    @Test fun noChangeNoCall() {
        assertTrue(SigningCli.changes(base, base, SigningScope.GLOBAL).isEmpty())
    }

    @Test fun oneArgvPerChangedField() {
        val key = "/home/u/.ssh/id_ed25519"
        assertEquals(
            listOf(listOf("config", "--global", "--", "user.signingkey", key)),
            SigningCli.changes(base, base.copy(keyPath = key), SigningScope.GLOBAL),
        )
        assertEquals(
            listOf(listOf("config", "--local", "--", "gpg.format", "ssh")),
            SigningCli.changes(base, base.copy(format = "ssh"), SigningScope.LOCAL),
        )
        assertEquals(
            listOf(listOf("config", "--global", "--", "commit.gpgsign", "true")),
            SigningCli.changes(base, base.copy(commitSign = true), SigningScope.GLOBAL),
        )
        assertEquals(
            listOf(listOf("config", "--global", "--", "tag.gpgsign", "true")),
            SigningCli.changes(base, base.copy(tagSign = true), SigningScope.GLOBAL),
        )
        assertEquals(
            listOf(listOf("config", "--global", "--", "tag.gpgsign", "false")),
            SigningCli.changes(base.copy(tagSign = true), base, SigningScope.GLOBAL),
        )
    }

    @Test fun aValueWithSpacesAndMetacharactersStaysOneElement() {
        val key = "/tmp/my keys/a; rm -rf \$HOME"
        val call = SigningCli.changes(base, base.copy(keyPath = key), SigningScope.GLOBAL).single()
        assertEquals(5, call.size)
        assertEquals(key, call.last())
    }

    @Test fun keyPathMustBeAnAbsolutePathNeverAnOption() {
        assertNull(SigningCli.keyPathProblem("/home/u/.ssh/id"))
        assertNull(SigningCli.keyPathProblem("C:\\keys\\id"))
        assertNull(SigningCli.keyPathProblem("\\\\host\\share\\id"))
        assertNotNull(SigningCli.keyPathProblem("--file=/x"))
        assertNotNull(SigningCli.keyPathProblem("-/x"))
        assertNotNull(SigningCli.keyPathProblem("relative/key"))
        assertNotNull(SigningCli.keyPathProblem("/a\nb"))
        assertNotNull(SigningCli.keyPathProblem("/" + "a".repeat(1024)))
        assertNotNull(SigningCli.keyPathProblem(""))
    }

    @Test fun localScopeNeedsARepository() {
        assertEquals(SigningScope.GLOBAL, SigningCli.scopeFor(SigningScope.LOCAL, false))
        assertEquals(SigningScope.LOCAL, SigningCli.scopeFor(SigningScope.LOCAL, true))
        assertEquals(SigningScope.GLOBAL, SigningCli.scopeFor(SigningScope.GLOBAL, true))
    }

    @Test fun rootGoesThroughDashC() {
        assertEquals(listOf("-C", "/r", "signing", "status", "--json"), SigningCli.withRoot("/r", SigningCli.statusArgs()))
        assertEquals(SigningCli.testArgs(), SigningCli.withRoot(null, SigningCli.testArgs()))
    }

    private val none = """{"configured":false,"key":{"source":"none","path":null,"id":null,"fingerprint":null,"level":null,"usable":false,"problem":null},
        "format":{"value":"openpgp","level":"default"},"commit_sign":{"value":false,"level":"default"},
        "tag_sign":{"value":false,"level":"default"},"allowed_signers":{"value":"","level":"default"}}"""

    @Test fun parsesNone() {
        val s = SigningCli.parseStatus(none)!!
        assertFalse(s.configured)
        assertEquals("none", s.key.source)
        assertEquals(SigningForm("", "openpgp", false, false), s.toForm())
        assertEquals("No signing key is configured.", s.summary())
    }

    @Test fun parsesFile() {
        val s = SigningCli.parseStatus(
            """{"configured":true,"key":{"source":"file","path":"/k/id","id":null,"fingerprint":"SHA256:abc","level":"global","usable":true,"problem":null},
            "format":{"value":"ssh","level":"global"},"commit_sign":{"value":true,"level":"global"},
            "tag_sign":{"value":false,"level":"default"},"allowed_signers":{"value":"/k/allowed","level":"global"}}""",
        )!!
        assertTrue(s.configured && s.key.usable)
        assertEquals(SigningForm("/k/id", "ssh", true, false), s.toForm())
        assertEquals("/k/allowed", s.allowedSigners.value)
        assertTrue(s.summary().contains("SHA256:abc"))
    }

    @Test fun parsesUnusable() {
        val s = SigningCli.parseStatus(
            """{"configured":true,"key":{"source":"file","path":"/k/gone","usable":false,"problem":"file not found"},
            "format":{"value":"ssh","level":"local"},"commit_sign":{"value":false,"level":"default"},
            "tag_sign":{"value":false,"level":"default"},"allowed_signers":{"value":"","level":"default"}}""",
        )!!
        assertFalse(s.key.usable)
        assertEquals("file not found", s.key.problem)
        assertTrue(s.summary().contains("not usable: file not found"))
    }

    @Test fun badStatusIsNull() {
        assertNull(SigningCli.parseStatus("not json"))
        assertNull(SigningCli.parseStatus("[1]"))
    }

    @Test fun parsesTestResults() {
        val ok = SigningCli.parseTest("""{"ok":true,"format":"ssh","source":"file","fingerprint":"SHA256:x","error":null}""")!!
        assertTrue(ok.ok)
        assertEquals("SHA256:x", ok.fingerprint)
        assertTrue(ok.message().startsWith("Test signature verified"))
        val bad = SigningCli.parseTest("""{"ok":false,"format":"ssh","source":"none","fingerprint":null,"error":"no key"}""")!!
        assertFalse(bad.ok)
        assertEquals("Test signing failed: no key", bad.message())
        assertNotNull(bad.error)
        assertNull(SigningCli.parseTest("oops"))
    }
}
