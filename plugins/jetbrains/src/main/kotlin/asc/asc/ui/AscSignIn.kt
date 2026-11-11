package asc.asc.ui

import com.intellij.ide.BrowserUtil
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.ProgressIndicator
import com.intellij.openapi.progress.ProgressManager
import com.intellij.openapi.progress.Task
import com.intellij.openapi.progress.runBlockingCancellable
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.ProjectActivity
import com.intellij.openapi.ui.InputValidator
import com.intellij.openapi.ui.Messages
import asc.asc.auth.AscAuth
import asc.asc.auth.AuthException
import asc.asc.auth.BridgeAuth
import asc.asc.auth.HostRules
import asc.asc.auth.JdkHttpPost
import asc.asc.auth.PasswordSafeVault
import asc.asc.bridge.AscBridgeService
import asc.asc.bridge.BridgeException

/** Where a host's credential comes from; never the credential itself. */
enum class SignInState(val text: String) {
    PASSWORD_SAFE("signed in (token in the IDE password store)"),
    HELPER("signed in (token held by your credential helper)"),
    SIGNED_OUT("signed out"),
}

/**
 * Sign-in for the IDE (SPEC 24.6): OAuth in the browser or a pasted access token, stored by the credential helper
 * through the bridge or, without one, in the IDE password store. A password-store token reaches `asc` only when
 * every root's remote is on its host ([HostRules.commonTokenHost]).
 */
@Service(Service.Level.APP)
class AscAuthService {
    private val bridge: AscBridgeService get() = service()
    private val vault = PasswordSafeVault()
    private val helper by lazy { BridgeAuth(bridge) }

    // The bridge environment is set by [reconcile], which applies the common-host rule, not by the sign-in itself.
    val auth: AscAuth by lazy { AscAuth(helper, vault, JdkHttpPost(), { BrowserUtil.browse(it) }) }

    private suspend fun roots(paths: List<String>): List<HostRules.Root> =
        bridge.hello(paths).rootsList.map { HostRules.Root(it.wsId, it.remoteUrl) }

    /** The hosts of the project's roots, offered first in the prompts. */
    suspend fun candidates(paths: List<String>): List<String> = HostRules.candidates(roots(paths))

    /** Gives the bridge the password-store token of the project's one host, or none; restarts it only on a change. */
    suspend fun reconcile(paths: List<String>) {
        val host = HostRules.commonTokenHost(roots(paths))
        val token = host?.let { vault.get(it) }
        bridge.setToken(token, if (token == null) null else host)
    }

    suspend fun state(host: String): SignInState = when {
        vault.get(host) != null -> SignInState.PASSWORD_SAFE
        helper.token(host) != null -> SignInState.HELPER
        else -> SignInState.SIGNED_OUT
    }
}

/** Applies the stored token of the project's host when the project opens, so a sign-in outlives a restart. */
class AscAuthStartup : ProjectActivity {
    override suspend fun execute(project: Project) {
        val roots = AscToolWindowFactory.ascRoots(project)
        if (roots.isEmpty()) return
        try {
            service<AscAuthService>().reconcile(roots)
        } catch (_: BridgeException) {
            // No asc found, or it does not start: the next action that needs the bridge reports it.
        }
    }
}

/** Shared visibility and the background plumbing of the sign-in actions. */
abstract class AscAuthAction(text: String) : AnAction(text), DumbAware {
    override fun getActionUpdateThread(): ActionUpdateThread = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        val p = e.project
        e.presentation.isEnabledAndVisible = p != null && AscActions.firstRoot(p) != null
    }

    protected fun background(project: Project, title: String, work: suspend () -> Unit) {
        ProgressManager.getInstance().run(object : Task.Backgroundable(project, title, true) {
            override fun run(indicator: ProgressIndicator) {
                try {
                    runBlockingCancellable { work() }
                } catch (e: AuthException) {
                    AscActions.notify(project, "Sign In", e.message ?: "Sign-in failed.", false)
                } catch (e: BridgeException) {
                    AscActions.notify(project, "Sign In", e.message ?: "The asc bridge is unavailable", false)
                }
            }
        })
    }

    /** Finds the candidate hosts in the background, then asks for one on the EDT. */
    protected fun withHost(project: Project, prompt: String, then: (String) -> Unit) {
        val roots = AscToolWindowFactory.ascRoots(project)
        ProgressManager.getInstance().run(object : Task.Backgroundable(project, "asc: finding hosts", true) {
            private var hosts: List<String> = emptyList()

            override fun run(indicator: ProgressIndicator) {
                hosts = try {
                    runBlockingCancellable { service<AscAuthService>().candidates(roots) }
                } catch (_: BridgeException) {
                    emptyList()
                }
            }

            override fun onSuccess() {
                val typed = if (hosts.isEmpty()) {
                    Messages.showInputDialog(project, prompt, title, null, "", HostValidator)
                } else {
                    Messages.showEditableChooseDialog(prompt, title, null, hosts.toTypedArray(), hosts[0], HostValidator)
                }
                HostRules.normalize(typed ?: return)?.let(then)
            }
        })
    }
}

/** Accepts only what [HostRules.normalize] accepts: an https host name, with an optional port. */
object HostValidator : InputValidator {
    override fun checkInput(inputString: String?): Boolean = HostRules.normalize(inputString.orEmpty()) != null

    override fun canClose(inputString: String?): Boolean = checkInput(inputString)
}

class AscSignInAction : AscAuthAction("Sign In to a Host...") {
    override fun actionPerformed(e: AnActionEvent) {
        val p = e.project ?: return
        val roots = AscToolWindowFactory.ascRoots(p)
        withHost(p, "Forge host (for example forge.example.org):") { host ->
            val how = Messages.showDialog(
                p, "How do you want to sign in to $host?", "Sign In",
                arrayOf("Sign In with Browser", "Use an Access Token", "Cancel"), 0, Messages.getQuestionIcon(),
            )
            if (how != 0 && how != 1) return@withHost
            var token: String? = null
            if (how == 1) {
                val typed = Messages.showPasswordDialog(p, "Access token for $host:", "Sign In", null) ?: return@withHost
                token = HostRules.cleanToken(typed)
                if (token == null) {
                    AscActions.notify(p, "Sign In", "That is not a usable token.", false)
                    return@withHost
                }
            }
            val pat = token
            background(p, "asc: signing in to $host") {
                val svc = service<AscAuthService>()
                if (pat == null) svc.auth.signInOAuth(host) else svc.auth.signInPat(host, pat)
                svc.reconcile(roots)
                AscActions.notify(p, "Sign In", "Signed in to $host.", true)
            }
        }
    }
}

class AscSignOutAction : AscAuthAction("Sign Out of a Host...") {
    override fun actionPerformed(e: AnActionEvent) {
        val p = e.project ?: return
        val roots = AscToolWindowFactory.ascRoots(p)
        withHost(p, "Host to sign out of:") { host ->
            val ok = Messages.showYesNoDialog(
                p, "The token stored in the IDE for $host is removed. Other hosts are not affected.",
                "Sign Out of $host?", "Sign Out", "Cancel", Messages.getQuestionIcon(),
            ) == Messages.YES
            if (!ok) return@withHost
            background(p, "asc: signing out of $host") {
                val svc = service<AscAuthService>()
                svc.auth.signOut(host)
                svc.reconcile(roots)
                val text = if (svc.state(host) == SignInState.HELPER) {
                    "Removed the IDE's token for $host. Your credential helper still holds one; remove it there."
                } else {
                    "Signed out of $host."
                }
                AscActions.notify(p, "Sign Out", text, true)
            }
        }
    }
}

class AscSignInStatusAction : AscAuthAction("Show Sign-in Status") {
    override fun actionPerformed(e: AnActionEvent) {
        val p = e.project ?: return
        val roots = AscToolWindowFactory.ascRoots(p)
        background(p, "asc: sign-in status") {
            val svc = service<AscAuthService>()
            val hosts = svc.candidates(roots)
            val text = if (hosts.isEmpty()) {
                "No root has an https remote, so there is no host to sign in to."
            } else {
                hosts.joinToString("\n") { "$it: ${svc.state(it).text}" }
            }
            AscActions.notify(p, "Sign-in Status", text, true)
        }
    }
}
