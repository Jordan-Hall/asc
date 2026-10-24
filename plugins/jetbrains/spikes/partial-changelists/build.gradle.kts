// Spike P53 (SPEC 24.5.4): pure Kotlin with no IntelliJ Platform dependency, so it runs without downloading an IDE.
// Standalone: `gradle test` in this directory. Through the plugin build: include it from plugins/jetbrains/settings.gradle.kts
// as `:spikes:partial-changelists` (that file belongs to P36) and run `./gradlew :spikes:partial-changelists:test`.
plugins {
    kotlin("jvm") version "2.1.20"
}

repositories { mavenCentral() }

kotlin { jvmToolchain(21) }

dependencies {
    testImplementation("junit:junit:4.13.2")
}

tasks.test { useJUnit() }
