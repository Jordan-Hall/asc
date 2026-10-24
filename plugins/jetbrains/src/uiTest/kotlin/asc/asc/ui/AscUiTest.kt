package asc.asc.ui

import com.intellij.driver.sdk.invokeAction
import com.intellij.driver.sdk.ui.components.common.ideFrame
import com.intellij.driver.sdk.waitForIndicators
import java.nio.file.Files
import java.util.concurrent.TimeUnit
import kotlin.time.Duration.Companion.minutes
import kotlin.time.Duration.Companion.seconds
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assumptions
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.Timeout

/**
 * SPEC 24.7.1, JetBrains UI row: the Commit window commit, Annotate, the merge dialog and the `asc` tool
 * window loading `asc-ui`. Nightly only, through `./gradlew uiTest`; each test skips when the plugin zip or
 * `ASC_BIN` is missing.
 */
@Timeout(value = 20, unit = TimeUnit.MINUTES)
class AscUiTest {
    @Test fun commitWindowCommitsTheModifiedFile() {
        val bin = UiHarness.ascBin()
        val project = UiHarness.dirtyFixture(bin)
        UiHarness.start("commitWindowCommit", project).useDriverAndCloseIde {
            waitForIndicators(5.minutes)
            ideFrame {
                invokeAction("CheckinProject")
                x("//div[@class='CommitMessage']").waitFound(30.seconds)
                x("//div[@class='CommitMessage']//div[@class='EditorComponentImpl']").click()
                keyboard { typeText("ui test commit") }
                x("//div[@class='JBOptionButton' and @accessiblename='Commit']").click()
            }
            waitForIndicators(1.minutes)
        }
        val log = UiHarness.asc(bin, project, "log", "--json")
        assertTrue(log.contains("ui test commit"), "the commit made in the Commit window is in the log: $log")
        assertTrue(Files.exists(project.resolve("new.txt")), "the untracked file is left alone")
    }

    @Test fun annotateOpensTheGutterColumn() {
        val bin = UiHarness.ascBin()
        val project = UiHarness.dirtyFixture(bin)
        UiHarness.start("annotate", project).useDriverAndCloseIde {
            waitForIndicators(5.minutes)
            ideFrame {
                x("//div[@accessiblename='a.txt']").doubleClick()
                x("//div[@class='EditorGutterComponentImpl']").rightClick()
                x("//div[@text='Annotate with Git Blame' or @text='Annotate']").click()
                x("//div[@class='EditorGutterComponentImpl' and contains(@visible_text,'Fixture')]").waitFound(60.seconds)
            }
        }
    }

    @Test fun mergeDialogOpensOnAConflictedFile() {
        val bin = UiHarness.ascBin()
        // TODO(P12): needs the `conflict` scenario of @asc/asc-fixtures (a restack paused on two conflicted
        // files); the CLI-built dirty fixture has no conflict, so the test is skipped until that package lands.
        val project = UiHarness.conflictFixture(bin)
        Assumptions.assumeTrue(project != null, "the conflict fixture is not available (P12)")
        UiHarness.start("mergeDialog", project!!).useDriverAndCloseIde {
            waitForIndicators(5.minutes)
            ideFrame {
                invokeAction("Vcs.ResolveConflicts")
                x("//div[@class='MyDialog' and @title='Conflicts']").waitFound(30.seconds)
                x("//div[@accessiblename='Merge...']").click()
                x("//div[@class='MyDialog' and contains(@title,'Merge')]").waitFound(30.seconds)
            }
        }
    }

    @Test fun toolWindowLoadsAscUiAndShowsTheStack() {
        val bin = UiHarness.ascBin()
        val project = UiHarness.dirtyFixture(bin)
        UiHarness.start("toolWindow", project).useDriverAndCloseIde {
            waitForIndicators(5.minutes)
            ideFrame {
                x("//div[@accessiblename='asc' and @class='StripeButton']").click()
                x("//div[@class='JBCefOsrComponent' or @class='JBCefBrowser']").waitFound(60.seconds)
                x("//div[contains(@visible_text,'Stack')]").waitFound(60.seconds)
            }
        }
    }
}
