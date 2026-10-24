package asc.asc.auth

import com.intellij.credentialStore.CredentialAttributes
import com.intellij.credentialStore.Credentials
import com.intellij.credentialStore.generateServiceName
import com.intellij.ide.passwordSafe.PasswordSafe

/** The JetBrains `PasswordSafe` fallback (SPEC 24.6). Tokens never reach settings or logs. */
class PasswordSafeVault : TokenVault {
    private fun attrs(host: String) =
        CredentialAttributes(generateServiceName("ASC", AuthConstants.passwordSafeService(host)))

    override fun get(host: String): String? = PasswordSafe.instance.getPassword(attrs(host))?.takeIf { it.isNotEmpty() }

    override fun set(host: String, token: String) = PasswordSafe.instance.set(attrs(host), Credentials("asc", token))

    override fun delete(host: String) = PasswordSafe.instance.set(attrs(host), null)
}
