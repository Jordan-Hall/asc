// Spike P51 (SPEC 24.5.4). Checks the precondition of the degraded-feature table: the asc plugin registers no VcsLogProvider.
// The per-feature behaviour is recorded by hand in ../RESULTS-vcslog.md after a run on 2026.1 (runIde); a unit test cannot observe the IDE UI.
// Needs `include(":spikes:vcslog")` in the root settings.gradle.kts (the root file is owned by P36).
plugins {
    kotlin("jvm")
}

kotlin { jvmToolchain(21) }

repositories { mavenCentral() }

dependencies {
    testImplementation("junit:junit:4.13.2")
}

tasks.test { useJUnit() }
