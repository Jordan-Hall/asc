package asc.asc.spikes

import java.io.File
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The spike table in RESULTS-vcslog.md assumes the plugin has no VcsLogProvider. This fails if one is added without re-running the spike. */
class VcsLogPreconditionTest {
    private fun pluginXml(): File {
        var dir: File? = File("").absoluteFile
        while (dir != null) {
            val f = File(dir, "src/main/resources/META-INF/plugin.xml")
            if (f.isFile) return f
            dir = dir.parentFile
        }
        error("plugin.xml not found above ${File("").absolutePath}")
    }

    @Test
    fun pluginRegistersNoVcsLogProvider() {
        val xml = pluginXml().readText()
        assertFalse("a VcsLogProvider is registered; re-run spike P51", xml.contains("logProvider", ignoreCase = true))
    }

    @Test
    fun pluginRegistersTheAscVcs() {
        assertTrue(pluginXml().readText().contains("""<vcs name="asc""""))
    }

    @Test
    fun resultsFileCoversEveryHypothesisRow() {
        var dir: File? = File("").absoluteFile
        var results: File? = null
        while (dir != null && results == null) {
            File(dir, "spikes/RESULTS-vcslog.md").takeIf { it.isFile }?.let { results = it }
            dir = dir.parentFile
        }
        val text = results?.readText() ?: error("RESULTS-vcslog.md not found")
        for (row in listOf("Log", "Compare with", "history", "Cherry-pick", "Annotate", "Branches popup", "comments API")) {
            assertTrue("missing row: $row", text.contains(row, ignoreCase = true))
        }
    }
}
