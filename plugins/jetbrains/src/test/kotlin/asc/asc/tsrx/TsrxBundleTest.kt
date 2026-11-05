package asc.asc.tsrx

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.nio.file.Files
import java.nio.file.attribute.FileTime

class TsrxBundleTest {
    private fun resource(name: String): ByteArray? = TsrxBundleTest::class.java.getResourceAsStream(name)?.use { it.readBytes() }

    @Test fun theJarCarriesTheGrammarTheConfigurationAndTheManifest() {
        for (f in TsrxBundle.FILES) assertTrue("missing /tsrx-bundle/$f", resource("/tsrx-bundle/$f") != null)
        val manifest = String(resource("/tsrx-bundle/package.json")!!)
        assertTrue(manifest.contains("\"source.tsrx\""))
        assertTrue(manifest.contains("\"./syntaxes/tsrx.tmLanguage.json\""))
        assertTrue(manifest.contains("\".tsrx\""))
        val grammar = String(resource("/tsrx-bundle/syntaxes/tsrx.tmLanguage.json")!!)
        assertTrue(grammar.contains("\"scopeName\": \"source.tsrx\""))
    }

    @Test fun extractWritesEachFileOnceAndRewritesOnlyAChangedOne() {
        val dir = Files.createTempDirectory("tsrx-bundle")
        try {
            TsrxBundle.extract(dir, ::resource)
            for (f in TsrxBundle.FILES) assertArrayEquals(resource("/tsrx-bundle/$f"), Files.readAllBytes(dir.resolve(f)))
            val old = FileTime.fromMillis(1_000_000L)
            for (f in TsrxBundle.FILES) Files.setLastModifiedTime(dir.resolve(f), old)
            Files.writeString(dir.resolve("package.json"), "{}")
            Files.setLastModifiedTime(dir.resolve("package.json"), old)
            TsrxBundle.extract(dir, ::resource)
            assertArrayEquals(resource("/tsrx-bundle/package.json"), Files.readAllBytes(dir.resolve("package.json")))
            assertTrue(Files.getLastModifiedTime(dir.resolve("package.json")) != old)
            assertEquals(old, Files.getLastModifiedTime(dir.resolve("syntaxes/tsrx.tmLanguage.json")))
            Files.list(dir).use { s -> assertTrue(s.noneMatch { it.fileName.toString().endsWith(".tmp") }) }
        } finally {
            dir.toFile().deleteRecursively()
        }
    }

    @Test(expected = IOException::class) fun aMissingResourceFails() {
        val dir = Files.createTempDirectory("tsrx-bundle")
        try {
            TsrxBundle.extract(dir) { null }
        } finally {
            dir.toFile().deleteRecursively()
        }
    }
}
