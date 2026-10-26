# P52 results: Git4Idea and the synthetic `.git` root

Status: UNVERIFIED. The machine that implemented this spike has no Gradle, no Gradle wrapper and only
JDK 17 (2026.1 needs 21), so `Git4IdeaRootSpikeTest` has not been run. Run it on a JDK 21 host:
`gradle -p plugins/jetbrains/spikes/git4idea-root test`, then confirm the manual checks below.

## Question (SPEC 24.5.4)
With Git4Idea enabled, does the synthetic `.git` gitfile make the IDE report an unregistered Git root,
offer to add it, or map it to `Git`?

## Hypothesis from the platform design
Git4Idea's root detection (`GitRootChecker`, `GitUtil.isGitRoot`) accepts a `.git` gitfile and
resolves it, so the IDE may scan the synthetic root as a candidate Git root and show
"Unregistered VCS root detected". The asc gitdir has no `HEAD`/`objects`, so a `Git` mapping would
fail on first command.

## Options, least invasive first
1. `vcs.xml` ignored unregistered roots (`VcsConfiguration.addIgnoredUnregisteredRoots`): silences the
   notice only. No effect on mapping.
2. A `VcsRootChecker` that claims the root (`AscRootChecker`, already registered by P37a) plus an explicit
   `asc` directory mapping (`AscMappingActivity`): the root is registered, so the unregistered-root scan skips it.
3. Disabling `Git4Idea` for the project: not possible per project; rejected as too invasive.

## Decision (pending verification)
Use 2 with 1 as belt and braces, which is what `AscMappingActivity` already does (the note in that
file about this spike is replaced by the result once the manual checks below pass). If a notice still appears with both in place,
document it in the plugin README as a known cosmetic issue (SPEC decision rule "otherwise document it").

## Manual checks to record here
- [ ] Open an asc workspace with Git4Idea enabled and the plugin absent: notice appears? Y/N
- [ ] Same with the plugin: notice appears? Mapping shown in Settings > Version Control? 
- [ ] Notice reappears after IDE restart or `vcs.xml` deletion?
