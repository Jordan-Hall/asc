// Spike P55 (SPEC 24.6): pure Kotlin/JDK, no IntelliJ Platform dependency, so it runs without downloading an IDE.
// Standalone: `gradle test` here. Through the plugin build: `./gradlew :spikes:loopback:test` once the root settings include it.
plugins {
    kotlin("jvm") version "2.1.20"
}

repositories { mavenCentral() }

kotlin { jvmToolchain(21) }

dependencies {
    testImplementation("junit:junit:4.13.2")
}

tasks.test { useJUnit() }
