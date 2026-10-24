// Precompiled script plugin `asc-allowlist` (P48). Apply with `id("asc-allowlist")` in build.gradle.kts.
// Registers `generateAllowlist` and feeds its output into the main source set, so compileKotlin and
// processResources run it first. It applies no Kotlin plugin itself; the project's own plugin set wins.
val generateAllowlist = tasks.register<GenerateAllowlistTask>("generateAllowlist") {
    description = "Generates Allowlist.kt and the allowlist.json resource from packages/asc-client-ts allowlist.json."
    group = "build"
    source.set(layout.projectDirectory.file("../../packages/asc-client-ts/src/transport/allowlist.json"))
    kotlinDir.set(layout.buildDirectory.dir("generated/allowlist/kotlin"))
    resourceDir.set(layout.buildDirectory.dir("generated/allowlist/resources"))
}

pluginManager.withPlugin("java") {
    extensions.getByType<SourceSetContainer>().named("main") {
        java.srcDir(generateAllowlist.map { it.kotlinDir })
        resources.srcDir(generateAllowlist.map { it.resourceDir })
    }
}
