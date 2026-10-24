package asc.asc.ui

import com.intellij.ide.starter.driver.engine.BackgroundRun
import com.intellij.ide.starter.driver.engine.runIdeWithDriver
import com.intellij.ide.starter.ide.IdeProductProfile
import com.intellij.ide.starter.models.TestCase
import com.intellij.ide.starter.plugins.PluginConfigurator
import com.intellij.ide.starter.project.LocalProjectInfo
import com.intellij.ide.starter.runner.Starter
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit
import org.junit.jupiter.api.Assumptions

/**
 * Shared set-up for the IDE Starter + Driver tests (SPEC 24.7.1, nightly). Each test builds a fresh asc
 * fixture with the `asc` CLI, installs the built plugin zip into a fresh IDE and drives it.
 *
 * No secret reaches argv or a log: the fixture needs no credentials and the child process gets a fixed
 * identity and no token.
 */
object UiHarness {
    /** The plugin zip built by `buildPlugin`, set by the `uiTest` Gradle task; the test is skipped without it. */
    fun pluginZip(): Path {
        val p = System.getProperty("path.to.build.plugin")
        Assumptions.assumeTrue(p != null && Files.exists(Path.of(p)), "path.to.build.plugin is not set or missing")
        return Path.of(p)
    }

    /** The asc binary (`ASC_BIN`); the test is skipped without it. */
    fun ascBin(): String {
        val bin = System.getProperty("asc.bin")
        Assumptions.assumeTrue(bin != null && Files.exists(Path.of(bin)), "ASC_BIN is not set to an existing binary")
        return bin
    }

    fun asc(bin: String, dir: Path, vararg args: String): String {
        val pb = ProcessBuilder(listOf(bin) + args).directory(dir.toFile()).redirectErrorStream(true)
        val env = pb.environment()
        env["GIT_AUTHOR_NAME"] = "Fixture"
        env["GIT_AUTHOR_EMAIL"] = "fixture@example.invalid"
        env["GIT_COMMITTER_NAME"] = "Fixture"
        env["GIT_COMMITTER_EMAIL"] = "fixture@example.invalid"
        env["GIT_AUTHOR_DATE"] = "2026-01-01T00:00:00Z"
        env["GIT_COMMITTER_DATE"] = "2026-01-01T00:00:00Z"
        val proc = pb.start()
        val out = proc.inputStream.readBytes().toString(Charsets.UTF_8)
        check(proc.waitFor(60, TimeUnit.SECONDS) && proc.exitValue() == 0) { "asc ${args.joinToString(" ")} failed: $out" }
        return out
    }

    private fun tempDir(): Path {
        val base = System.getProperty("ide.starter.dir")?.let { Path.of(it) }
        return if (base != null) {
            Files.createDirectories(base)
            Files.createTempDirectory(base, "asc-ui-fixture")
        } else {
            Files.createTempDirectory("asc-ui-fixture")
        }
    }

    /**
     * An asc workspace with one committed `a.txt`, a modified `a.txt` and an untracked `new.txt`.
     * TODO(P12): replace with the `dirty`, `stacks` and `conflict` scenarios of `@asc/asc-fixtures`.
     */
    fun dirtyFixture(bin: String): Path {
        val dir = tempDir()
        asc(bin, dir, "init")
        Files.writeString(dir.resolve("a.txt"), "one\ntwo\nthree\n")
        asc(bin, dir, "commit", "-m", "base", "--end-of-options")
        Files.writeString(dir.resolve("a.txt"), "one\ntwo changed\nthree\n")
        Files.writeString(dir.resolve("new.txt"), "untracked\n")
        return dir
    }

    fun start(testName: String, project: Path): BackgroundRun {
        val ctx = Starter.newContext(testName, TestCase(IdeProductProfile.IU, LocalProjectInfo(project)).withVersion("2026.1"))
        PluginConfigurator(ctx).installPluginFromPath(pluginZip())
        return ctx.runIdeWithDriver()
    }
}

/** TODO(P12): returns the `conflict` scenario once `@asc/asc-fixtures` is in HEAD; null until then. */
@Suppress("UNUSED_PARAMETER")
fun UiHarness.conflictFixture(bin: String): Path? = null
