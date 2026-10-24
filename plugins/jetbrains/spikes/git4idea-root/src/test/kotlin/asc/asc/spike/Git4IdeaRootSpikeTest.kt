package asc.asc.spike

import com.intellij.openapi.vcs.ProjectLevelVcsManager
import com.intellij.openapi.vcs.VcsConfiguration
import com.intellij.openapi.vcs.VcsDirectoryMapping
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import java.nio.file.Files
import java.nio.file.Path

/**
 * Observes, with Git4Idea enabled, what the IDE does with a synthetic asc root. Each test records one
 * observation; RESULTS.md holds the outcome and the chosen fix. Paths are temp dirs only.
 */
class Git4IdeaRootSpikeTest : BasePlatformTestCase() {
    private fun syntheticRoot(): Path {
        val root = Files.createTempDirectory("p52-root")
        val gitdir = Files.createTempDirectory("p52-gitdir")
        Files.writeString(gitdir.resolve("asc-workspace"), "ws-1")
        Files.writeString(root.resolve(".git"), "gitdir: ${gitdir.toString().replace('\', '/')}\n")
        return root
    }

    fun testGitMappingIsNotAutoCreatedWithoutScan() {
        val root = syntheticRoot().toString().replace('\', '/')
        val vcs = ProjectLevelVcsManager.getInstance(project)
        assertFalse(vcs.directoryMappings.any { it.directory == root && it.vcs == "Git" })
    }

    fun testIgnoredUnregisteredRootIsRemembered() {
        val root = syntheticRoot().toString().replace('\', '/')
        val config = VcsConfiguration.getInstance(project)
        config.addIgnoredUnregisteredRoots(listOf(root))
        assertTrue(config.ignoredUnregisteredRoots.map { it.replace('\', '/') }.contains(root))
    }

    fun testExplicitAscMappingOverridesGit() {
        val root = syntheticRoot().toString().replace('\', '/')
        val vcs = ProjectLevelVcsManager.getInstance(project)
        vcs.setDirectoryMappings(listOf(VcsDirectoryMapping(root, "Git")))
        vcs.setDirectoryMappings(listOf(VcsDirectoryMapping(root, "asc")))
        assertEquals("asc", vcs.directoryMappings.single { it.directory == root }.vcs)
    }
}
