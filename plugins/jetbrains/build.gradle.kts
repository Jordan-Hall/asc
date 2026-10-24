import com.google.protobuf.gradle.id
import org.jetbrains.intellij.platform.gradle.IntelliJPlatformType
import org.jetbrains.intellij.platform.gradle.TestFrameworkType

plugins {
    kotlin("jvm") version "2.1.20"
    id("org.jetbrains.intellij.platform") version "2.5.0"
    id("com.google.protobuf") version "0.9.4"
}

group = "asc.asc"
version = providers.gradleProperty("pluginVersion").get()

repositories {
    mavenCentral()
    intellijPlatform { defaultRepositories() }
}

kotlin { jvmToolchain(21) }

dependencies {
    intellijPlatform {
        intellijIdea("2026.1")
        pluginVerifier()
        zipSigner()
        testFramework(TestFrameworkType.Platform)
    }
    implementation("com.google.protobuf:protobuf-java:4.31.1")
    testImplementation("junit:junit:4.13.2")
}

// protobuf-java is generated from the shared proto/ tree (the same files drive the TypeScript client).
sourceSets { main { proto { srcDir("../../proto") } } }

protobuf {
    protoc { artifact = "com.google.protobuf:protoc:4.31.1" }
    generateProtoTasks { all().configureEach { builtins { id("java") } } }
}

intellijPlatform {
    pluginConfiguration {
        name = "ASC"
        version = project.version.toString()
        ideaVersion {
            sinceBuild = "261"
            untilBuild = provider { null } // no untilBuild
        }
    }
    // Signing and publishing read only environment variables, so no secret reaches argv or a log (SPEC 24.9).
    // CERTIFICATE_CHAIN and PRIVATE_KEY hold PEM text; PRIVATE_KEY_PASSWORD is optional; PUBLISH_TOKEN is the Marketplace token.
    signing {
        certificateChain = providers.environmentVariable("CERTIFICATE_CHAIN")
        privateKey = providers.environmentVariable("PRIVATE_KEY")
        password = providers.environmentVariable("PRIVATE_KEY_PASSWORD")
    }
    publishing {
        token = providers.environmentVariable("PUBLISH_TOKEN")
        channels = providers.gradleProperty("pluginChannel").map { listOf(it) }.orElse(listOf("default"))
    }
    // IU, RD, RR, PY and WS on 2026.1 and 2026.2 (SPEC 24.7.1).
    pluginVerification {
        ides {
            for (v in listOf("2026.1", "2026.2")) {
                create(IntelliJPlatformType.IntellijIdeaUltimate, v)
                create(IntelliJPlatformType.Rider, v)
                create(IntelliJPlatformType.RustRover, v)
                create(IntelliJPlatformType.PyCharmProfessional, v)
                create(IntelliJPlatformType.WebStorm, v)
            }
        }
    }
}

tasks.test { useJUnit() }

// IDE Starter + Driver UI tests (SPEC 24.7.1, nightly): a separate source set, so `test` never downloads an IDE.
val uiTest: SourceSet by sourceSets.creating {
    compileClasspath += sourceSets.main.get().output
    runtimeClasspath += sourceSets.main.get().output
}
configurations.named("uiTestImplementation") { extendsFrom(configurations.testImplementation.get()) }
configurations.named("uiTestRuntimeOnly") { extendsFrom(configurations.testRuntimeOnly.get()) }

repositories {
    maven("https://www.jetbrains.com/intellij-repository/releases")
    maven("https://cache-redirector.jetbrains.com/intellij-dependencies")
}

val starterVersion = "253.28294.169"
dependencies {
    "uiTestImplementation"(kotlin("test"))
    "uiTestImplementation"("org.junit.jupiter:junit-jupiter:5.11.4")
    "uiTestImplementation"("com.jetbrains.intellij.tools:ide-starter-squashed:$starterVersion")
    "uiTestImplementation"("com.jetbrains.intellij.tools:ide-starter-junit5:$starterVersion")
    "uiTestImplementation"("com.jetbrains.intellij.driver:driver-client:$starterVersion")
    "uiTestImplementation"("com.jetbrains.intellij.driver:driver-sdk:$starterVersion")
    "uiTestImplementation"("com.jetbrains.intellij.driver:driver-model:$starterVersion")
}

// `./gradlew uiTest` installs the built plugin zip into a fresh IDE through IDE Starter and drives it.
// ASC_BIN names the asc binary used to build the fixture; ASC_UITEST_DIR moves IDE downloads and logs off the system drive.
tasks.register<Test>("uiTest") {
    description = "IDE Starter + Driver UI tests (nightly)."
    group = "verification"
    testClassesDirs = uiTest.output.classesDirs
    classpath = uiTest.runtimeClasspath
    useJUnitPlatform()
    dependsOn("buildPlugin")
    systemProperty("path.to.build.plugin", tasks.named<org.gradle.api.tasks.bundling.Zip>("buildPlugin").get().archiveFile.get().asFile.absolutePath)
    providers.environmentVariable("ASC_BIN").orNull?.let { systemProperty("asc.bin", it) }
    providers.environmentVariable("ASC_UITEST_DIR").orNull?.let { systemProperty("ide.starter.dir", it) }
    jvmArgs("--add-opens=java.base/java.lang=ALL-UNNAMED", "--add-opens=java.base/java.util=ALL-UNNAMED")
    outputs.upToDateWhen { false }
}
