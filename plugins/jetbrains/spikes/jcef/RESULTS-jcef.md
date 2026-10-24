# P54 results: the jcef module (SPEC 24.5.4)

Status: scaffold only. NOT YET RUN. This machine has JDK 17 only and no Gradle wrapper, so
`verifyPlugin` could not run against 2026.1 and 2026.2. Do not treat the decision below as a finding.

Run: `./gradlew verifyPlugin` and `./gradlew verifyPlugin -Pjcef=required` in this folder
(or `:spikes:jcef:verifyPlugin` once the root settings include it; TODO(P54)).

| Question | 2026.1 | 2026.2 |
|---|---|---|
| `com.intellij.modules.jcef` is a known module id | unrecorded | unrecorded |
| `verifyPlugin` passes with the dependency optional | unrecorded | unrecorded |

Decision rule: keep `<depends optional="true" config-file="asc-jcef.xml">` if the verifier passes on both;
otherwise make it required from the `sinceBuild` that needs it.
