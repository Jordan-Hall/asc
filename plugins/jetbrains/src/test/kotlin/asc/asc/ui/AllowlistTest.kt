package asc.asc.ui

import java.nio.file.Files
import java.nio.file.Path
import java.security.MessageDigest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Proves the generated Kotlin copy of allowlist.json equals the checked-in source (SPEC 24.4.5.1, P48). */
class AllowlistTest {
    private val source = Path.of("..", "..", "packages", "asc-client-ts", "src", "transport", "allowlist.json")
    private val bytes = Files.readAllBytes(source)

    private fun sha(b: ByteArray) = MessageDigest.getInstance("SHA-256").digest(b).joinToString("") { "%02x".format(it) }

    @Test fun generatedTextEqualsSourceByteForByte() {
        assertTrue(GeneratedAllowlist.JSON.toByteArray(Charsets.UTF_8).contentEquals(bytes))
    }

    @Test fun generatedHashEqualsSourceHash() {
        assertEquals(sha(bytes), GeneratedAllowlist.SHA256)
    }

    @Test fun driftIsDetected() {
        // A one-byte change to the source changes the hash, so a stale Allowlist.kt cannot pass.
        val tampered = bytes.copyOf()
        tampered[tampered.size - 1] = (tampered[tampered.size - 1].toInt() xor 1).toByte()
        assertTrue(sha(tampered) != GeneratedAllowlist.SHA256)
    }

    @Test fun generatedAllowlistParsesToTheSourceTable() {
        val gen = GeneratedAllowlist.parse()
        val src = Allowlist.parse(String(bytes, Charsets.UTF_8))
        assertEquals(src.read, gen.read)
        assertEquals(src.write.keys, gen.write.keys)
        assertEquals(src.reject, gen.reject)
        assertEquals(src.maxArgs, gen.maxArgs)
        assertEquals(src.maxArgBytes, gen.maxArgBytes)
        assertEquals(src.verbs.keys, gen.verbs.keys)
        for ((k, v) in src.verbs) {
            assertEquals(v.flags, gen.verbs[k]!!.flags)
            assertEquals(v.pos, gen.verbs[k]!!.pos)
            assertEquals(v.confirm, gen.verbs[k]!!.confirm)
        }
        assertEquals(src.providers.keys, gen.providers.keys)
        for ((id, pairs) in src.providers) {
            assertEquals(pairs.map { Triple(it.method, it.path, it.confirm) }, gen.providers.getValue(id).map { Triple(it.method, it.path, it.confirm) })
            assertEquals(pairs.map { it.body }, gen.providers.getValue(id).map { it.body })
        }
        for ((k, w) in src.write) {
            assertEquals(w.confirm, gen.write[k]!!.confirm)
            assertEquals(w.patchBytes, gen.write[k]!!.patchBytes)
            assertEquals(w.paths, gen.write[k]!!.paths)
        }
    }

    @Test fun packagedResourceEqualsSource() {
        val res = Allowlist::class.java.getResourceAsStream(Allowlist.RESOURCE)
        assertNotNull(res)
        assertTrue(res!!.use { it.readBytes() }.contentEquals(bytes))
        assertNotNull(Allowlist.load())
    }

    @Test fun denyEntriesStayDenied() {
        val gen = GeneratedAllowlist.parse()
        assertTrue(gen.reject.containsAll(listOf("-c", "--exec", "--force", "--git-dir")))
        assertNull(gen.verbs["push"]?.flags?.get("--force"))
        assertTrue(gen.providers.values.all { ps -> ps.none { it.method == "DELETE" } })
    }
}
