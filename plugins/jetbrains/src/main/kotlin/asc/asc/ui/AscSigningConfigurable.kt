package asc.asc.ui

import asc.asc.bridge.AscBridgeService
import com.google.gson.JsonElement
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.execution.process.CapturingProcessHandler
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.components.service
import com.intellij.openapi.fileChooser.FileChooserDescriptorFactory
import com.intellij.openapi.options.Configurable
import com.intellij.openapi.project.ProjectManager
import com.intellij.openapi.ui.ComboBox
import com.intellij.openapi.ui.TextFieldWithBrowseButton
import com.intellij.ui.components.JBCheckBox
import com.intellij.ui.components.JBLabel
import com.intellij.util.ui.FormBuilder
import java.io.File
import javax.swing.DefaultComboBoxModel
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JPanel

/** Where a setting is written. */
enum class SigningScope(val flag: String, val label: String) {
    GLOBAL("--global", "Global (all repositories)"),
    LOCAL("--local", "This repository"),
    ;

    override fun toString(): String = label
}

/** The signing settings as the page holds them. The key is a path or id, never key content. */
data class SigningForm(
    val keyPath: String,
    val format: String,
    val commitSign: Boolean,
    val tagSign: Boolean,
)

/** One value of `asc signing status --json` with the layer it came from. */
data class Leveled<T>(val value: T, val level: String)

data class SigningKey(
    val source: String,
    val path: String?,
    val id: String?,
    val fingerprint: String?,
    val level: String?,
    val usable: Boolean,
    val problem: String?,
)

data class SigningStatus(
    val configured: Boolean,
    val key: SigningKey,
    val format: Leveled<String>,
    val commitSign: Leveled<Boolean>,
    val tagSign: Leveled<Boolean>,
    val allowedSigners: Leveled<String>,
) {
    /** The fields the page edits, as the CLI reports them; a key chosen by id or agent shows no path. */
    fun toForm() = SigningForm(key.path.orEmpty(), format.value, commitSign.value, tagSign.value)

    /** One line for the status label. */
    fun summary(): String {
        if (!configured) return "No signing key is configured."
        val what = when (key.source) {
            "file", "public-key-file" -> "key file ${key.path.orEmpty()}"
            "agent" -> "key from the SSH agent"
            "key-id" -> "key ${key.id.orEmpty()}"
            "literal" -> "a key set inline"
            else -> "no key"
        }
        val fp = key.fingerprint?.let { " ($it)" }.orEmpty()
        val bad = if (key.usable) "" else " - not usable: ${key.problem ?: "unknown problem"}"
        return "Signing with $what$fp, format ${format.value}$bad"
    }
}

/** The result of `asc signing test --json`. */
data class SigningTestResult(val ok: Boolean, val format: String?, val source: String?, val fingerprint: String?, val error: String?) {
    fun message(): String = if (ok) {
        "Test signature verified" + (fingerprint?.let { " ($it)" } ?: "") + "."
    } else {
        "Test signing failed: ${error ?: "unknown error"}"
    }
}

/** The pure parts of the settings page: argv, JSON and the diff between what is shown and what is set. */
object SigningCli {
    fun statusArgs(): List<String> = listOf("signing", "status", "--json")

    fun testArgs(): List<String> = listOf("signing", "test", "--json")

    /** `-C <root>` first when a repository is named, so a local read or write has a repository to act on. */
    fun withRoot(root: String?, args: List<String>): List<String> =
        if (root.isNullOrBlank()) args else listOf("-C", root) + args

    /** The scope to write: local needs a repository, so without one the write is global. */
    fun scopeFor(requested: SigningScope, hasRoot: Boolean): SigningScope =
        if (hasRoot) requested else SigningScope.GLOBAL

    /** `asc config <scope> <key> <value>`; the value is one argv element, so no quoting matters. */
    fun configArgs(scope: SigningScope, key: String, value: String): List<String> =
        listOf("config", scope.flag, key, value)

    /** One config call for each field that differs from `current`, in a fixed order. */
    fun changes(current: SigningForm, wanted: SigningForm, scope: SigningScope): List<List<String>> {
        val out = ArrayList<List<String>>()
        if (wanted.keyPath != current.keyPath && wanted.keyPath.isNotBlank()) {
            out += configArgs(scope, "user.signingkey", wanted.keyPath.trim())
        }
        if (wanted.format != current.format) out += configArgs(scope, "gpg.format", wanted.format)
        if (wanted.commitSign != current.commitSign) out += configArgs(scope, "commit.gpgsign", wanted.commitSign.toString())
        if (wanted.tagSign != current.tagSign) out += configArgs(scope, "tag.gpgsign", wanted.tagSign.toString())
        return out
    }

    fun parseStatus(text: String): SigningStatus? = try {
        JsonParser.parseString(text).takeIf { it.isJsonObject }?.asJsonObject?.let { status(it) }
    } catch (_: Exception) {
        null
    }

    fun parseTest(text: String): SigningTestResult? = try {
        JsonParser.parseString(text).takeIf { it.isJsonObject }?.asJsonObject?.let {
            SigningTestResult(it.bool("ok") ?: false, it.str("format"), it.str("source"), it.str("fingerprint"), it.str("error"))
        }
    } catch (_: Exception) {
        null
    }

    private fun JsonObject.str(k: String): String? = get(k)?.takeIf { it.isJsonPrimitive }?.asString
    private fun JsonObject.bool(k: String): Boolean? = get(k)?.takeIf { it.isJsonPrimitive && it.asJsonPrimitive.isBoolean }?.asBoolean
    private fun JsonObject.obj(k: String): JsonObject? = get(k)?.takeIf(JsonElement::isJsonObject)?.asJsonObject

    private fun status(o: JsonObject): SigningStatus {
        val k = o.obj("key")
        val key = SigningKey(
            source = k?.str("source") ?: "none",
            path = k?.str("path"),
            id = k?.str("id"),
            fingerprint = k?.str("fingerprint"),
            level = k?.str("level"),
            usable = k?.bool("usable") ?: false,
            problem = k?.str("problem"),
        )
        fun s(name: String, d: String) = o.obj(name).let { Leveled(it?.str("value") ?: d, it?.str("level") ?: "default") }
        fun b(name: String) = o.obj(name).let { Leveled(it?.bool("value") ?: false, it?.str("level") ?: "default") }
        return SigningStatus(o.bool("configured") ?: false, key, s("format", "openpgp"), b("commit_sign"), b("tag_sign"), s("allowed_signers", ""))
    }
}

/** Settings > Version Control > asc Signing: the key, format and sign-by-default switches, over the `asc` CLI. */
class AscSigningConfigurable : Configurable {
    private val formats = listOf("openpgp", "ssh", "x509")
    private val statusLabel = JBLabel("Reading the signing status...")
    private val resultLabel = JBLabel("")
    private val keyField = TextFieldWithBrowseButton()
    private val formatBox = ComboBox(DefaultComboBoxModel(formats.toTypedArray()))
    private val commitBox = JBCheckBox("Sign commits")
    private val tagBox = JBCheckBox("Sign tags")
    private val scopeBox = ComboBox(DefaultComboBoxModel(SigningScope.entries.toTypedArray()))
    private val testButton = JButton("Test signing")
    private var panel: JPanel? = null
    private var loaded: SigningForm? = null

    override fun getDisplayName(): String = "asc Signing"

    override fun createComponent(): JComponent {
        keyField.addBrowseFolderListener(
            null,
            FileChooserDescriptorFactory.createSingleFileDescriptor().withTitle("Signing Key File"),
        )
        testButton.addActionListener { runTest() }
        val p = FormBuilder.createFormBuilder()
            .addComponent(statusLabel)
            .addLabeledComponent("Key file (private or public):", keyField)
            .addLabeledComponent("Format:", formatBox)
            .addComponent(commitBox)
            .addComponent(tagBox)
            .addLabeledComponent("Apply to:", scopeBox)
            .addComponent(testButton)
            .addComponent(resultLabel)
            .addComponentFillVertically(JPanel(), 0)
            .panel
        panel = p
        if (root() == null) {
            scopeBox.selectedItem = SigningScope.GLOBAL
            scopeBox.isEnabled = false
            scopeBox.toolTipText = "Open an asc repository to write local settings."
        }
        refresh()
        return p
    }

    private fun form() = SigningForm(
        keyField.text.trim(),
        formatBox.selectedItem as? String ?: "openpgp",
        commitBox.isSelected,
        tagBox.isSelected,
    )

    private fun scope() = SigningCli.scopeFor(scopeBox.selectedItem as? SigningScope ?: SigningScope.GLOBAL, root() != null)

    override fun isModified(): Boolean = loaded?.let { SigningCli.changes(it, form(), scope()).isNotEmpty() } ?: false

    override fun apply() {
        val before = loaded ?: return
        val r = root()
        val calls = SigningCli.changes(before, form(), scope()).map { SigningCli.withRoot(r, it) }
        if (calls.isEmpty()) return
        val now = form()
        ApplicationManager.getApplication().executeOnPooledThread {
            val failed = calls.any { run(it) == null }
            ApplicationManager.getApplication().invokeLater({
                if (!failed) loaded = now
                resultLabel.text = if (failed) "Could not save: asc config failed." else "Saved."
                refresh()
            }, ModalityState.any())
        }
    }

    override fun reset() {
        loaded?.let { show(it) }
    }

    override fun disposeUIResources() {
        panel = null
    }

    private fun show(f: SigningForm) {
        keyField.text = f.keyPath
        formatBox.selectedItem = f.format
        commitBox.isSelected = f.commitSign
        tagBox.isSelected = f.tagSign
    }

    private fun refresh() {
        val r = root()
        ApplicationManager.getApplication().executeOnPooledThread {
            val st = run(SigningCli.withRoot(r, SigningCli.statusArgs()))?.let(SigningCli::parseStatus)
            ApplicationManager.getApplication().invokeLater({
                if (panel == null) return@invokeLater
                if (st == null) {
                    statusLabel.text = "asc was not found or did not answer; set the path to asc."
                } else {
                    statusLabel.text = st.summary()
                    loaded = st.toForm().also(::show)
                }
            }, ModalityState.any())
        }
    }

    private fun runTest() {
        val r = root()
        testButton.isEnabled = false
        resultLabel.text = "Testing..."
        ApplicationManager.getApplication().executeOnPooledThread {
            val res = runAny(SigningCli.withRoot(r, SigningCli.testArgs()))?.let(SigningCli::parseTest)
            ApplicationManager.getApplication().invokeLater({
                testButton.isEnabled = true
                resultLabel.text = res?.message() ?: "Test signing did not run: asc gave no answer."
            }, ModalityState.any())
        }
    }

    /** The first asc root of an open project, so a local scope has a repository to write to. */
    private fun root(): String? = ProjectManager.getInstance().openProjects.firstNotNullOfOrNull { p ->
        AscToolWindowFactory.ascRoots(p).firstOrNull()
    }

    /** Stdout when asc exits 0, else null. Never on the EDT. */
    private fun run(args: List<String>): String? = execute(args)?.takeIf { it.first == 0 }?.second

    /** Stdout whatever the exit code (`signing test` exits 1 with a JSON body). */
    private fun runAny(args: List<String>): String? = execute(args)?.second?.takeIf { it.isNotBlank() }

    private fun execute(args: List<String>): Pair<Int, String>? {
        val exe = service<AscBridgeService>().executablePath() ?: return null
        val cmd = GeneralCommandLine(listOf(exe) + args)
            .withWorkDirectory(File(exe).absoluteFile.parentFile)
            .withEnvironment("GIT_TERMINAL_PROMPT", "0")
            .withEnvironment("GCM_INTERACTIVE", "never")
        return try {
            val out = CapturingProcessHandler(cmd).runProcess(60_000)
            if (out.isTimeout) null else out.exitCode to out.stdout
        } catch (_: Exception) {
            null
        }
    }
}
