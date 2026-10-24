import org.jetbrains.intellij.platform.gradle.TestFrameworkType

// Spike P52 (SPEC 24.5.4): runs with Git4Idea enabled against a synthetic asc root (a `.git` gitfile
// whose gitdir holds the `asc-workspace` marker). Standalone build: `gradle -p spikes/git4idea-root test`
// until the parent settings.gradle.kts includes it (that file is owned by another unit).
plugins {
    kotlin("jvm") version "2.1.20"
    id("org.jetbrains.intellij.platform") version "2.5.0"
}

repositories {
    mavenCentral()
    intellijPlatform { defaultRepositories() }
}

kotlin { jvmToolchain(21) }

dependencies {
    intellijPlatform {
        intellijIdea("2026.1")
        bundledPlugin("Git4Idea")
        testFramework(TestFrameworkType.Platform)
    }
    testImplementation("junit:junit:4.13.2")
}

tasks.test { useJUnit() }
