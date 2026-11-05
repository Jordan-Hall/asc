package asc.asc.tsrx

import com.intellij.openapi.application.PathManager
import com.intellij.openapi.diagnostic.logger
import org.jetbrains.plugins.textmate.api.TextMateBundleProvider
import org.jetbrains.plugins.textmate.api.TextMateBundleProvider.PluginBundle
import java.io.IOException
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption

/**
 * TSRX colouring through the TextMate plugin: the grammar of the VS Code extension (copied into the jar under
 * /tsrx-bundle/ at build time) as a VS Code-style bundle. The TextMate plugin reads bundles from disk, so the
 * files are written once under the IDE's system directory and rewritten only when the jar's copy differs. The
 * `source.ts` and `source.css` scopes the grammar embeds come from the IDE's own bundled TextMate bundles.
 */
class TsrxBundleProvider : TextMateBundleProvider {
    override fun getBundles(): List<PluginBundle> {
        val dir = PathManager.getSystemDir().resolve("asc").resolve("tsrx-bundle")
        return try {
            TsrxBundle.extract(dir) { TsrxBundleProvider::class.java.getResourceAsStream(it)?.use { s -> s.readBytes() } }
            listOf(PluginBundle("TSRX", dir))
        } catch (e: IOException) {
            logger<TsrxBundleProvider>().warn("TSRX bundle not written to $dir", e)
            emptyList()
        }
    }
}

/** The bundle's files and how they reach disk; free of the IDE so tests run without one. */
object TsrxBundle {
    /** Paths inside the bundle, also the resource names under /tsrx-bundle/. */
    val FILES = listOf("package.json", "language-configuration.json", "syntaxes/tsrx.tmLanguage.json")

    /** Writes each file of [read] (a resource name to its bytes) under [dir] when it is missing or differs. */
    fun extract(dir: Path, read: (String) -> ByteArray?) {
        for (name in FILES) {
            val bytes = read("/tsrx-bundle/$name") ?: throw IOException("resource /tsrx-bundle/$name is missing")
            val target = dir.resolve(name)
            if (Files.isRegularFile(target) && Files.readAllBytes(target).contentEquals(bytes)) continue
            Files.createDirectories(target.parent)
            val tmp = Files.createTempFile(target.parent, ".tsrx", ".tmp")
            try {
                Files.write(tmp, bytes)
                Files.move(tmp, target, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE)
            } finally {
                Files.deleteIfExists(tmp)
            }
        }
    }
}
