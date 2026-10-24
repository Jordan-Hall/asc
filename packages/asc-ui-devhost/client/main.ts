// Smoke-page entry: applies the asc-ui theme tokens and reads the user from the mock Forgejo
// through the guarded client, proving asc-ui, asc-client and the host fit together.
import { createForgejoClient } from "@asc/asc-client/forgejo";
import { applyTokens, resolveTokens } from "@asc/asc-ui/theme";

applyTokens(document.documentElement, resolveTokens());
const forge = createForgejoClient({ baseUrl: location.origin, allowLoopbackHttp: true });
const root = document.getElementById("root");
const { data } = await forge.api.GET("/user");
if (root) root.textContent = `asc-ui ready: ${data?.login ?? "unknown"}`;
