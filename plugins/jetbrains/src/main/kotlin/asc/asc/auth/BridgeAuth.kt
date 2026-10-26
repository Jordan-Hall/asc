package asc.asc.auth

import asc.v1.BridgeOuterClass.StoreTokenRequest
import asc.v1.BridgeOuterClass.TokenRequest
import asc.v1.BridgeOuterClass.TokenResponse
import asc.asc.bridge.BridgeException
import asc.asc.bridge.AscBridgeService

/** [AuthBridge] over `Bridge.Token` and `Bridge.StoreToken`. Error messages carry a stable code prefix. */
class BridgeAuth(private val bridge: AscBridgeService) : AuthBridge {
    override suspend fun token(host: String): String? = try {
        TokenResponse.parseFrom(
            bridge.unary("asc.v1.Bridge/Token", TokenRequest.newBuilder().setHost(host).build().toByteArray()),
        ).token.takeIf { it.isNotEmpty() }
    } catch (e: BridgeException) {
        if (e.message?.startsWith("NOT_FOUND") == true) null else throw e
    }

    override suspend fun storeToken(host: String, token: String) {
        try {
            bridge.unary(
                "asc.v1.Bridge/StoreToken",
                StoreTokenRequest.newBuilder().setHost(host).setToken(token).build().toByteArray(),
            )
        } catch (e: BridgeException) {
            if (e.message?.startsWith("NO_CREDENTIAL_HELPER") == true) throw NoCredentialHelper()
            throw e
        }
    }
}
