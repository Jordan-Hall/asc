package asc.asc.spike

import com.intellij.ui.jcef.JBCefApp

/** Loaded only when the optional jcef module is present; the runtime check mirrors SPEC 24.5.2. */
class JcefProbe {
    fun supported(): Boolean = JBCefApp.isSupported()
}
