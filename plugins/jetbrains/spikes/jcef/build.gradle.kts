import org.jetbrains.intellij.platform.gradle.IntelliJPlatformType

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
        pluginVerifier()
    }
}

// Variant selected with -Pjcef=optional (default) or -Pjcef=required; the verifier runs on 2026.1 and 2026.2.
val variant = providers.gradleProperty("jcef").orElse("optional")

intellijPlatform {
    pluginConfiguration {
        id = "asc.asc.spike.jcef"
        name = "asc jcef spike"
        version = "0.0.0"
        ideaVersion { sinceBuild = "261"; untilBuild = provider { null } }
    }
    pluginVerification {
        ides {
            for (v in listOf("2026.1", "2026.2")) create(IntelliJPlatformType.IntellijIdeaUltimate, v)
        }
    }
}

tasks.processResources {
    from("src/variants/${variant.get()}") { into("META-INF") }
}
