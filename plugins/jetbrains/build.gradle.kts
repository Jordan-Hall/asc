import com.google.protobuf.gradle.id
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
    pluginVerification {
        ides { recommended() }
    }
}

tasks.test { useJUnit() }
