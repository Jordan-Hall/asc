package asc.asc.ui

import com.google.gson.JsonObject
import java.awt.Color

/** Where theme values come from. Production reads `UIManager`/`JBColor`; tests pass a map. */
interface ThemeSource {
    fun color(key: String): Color?

    fun fontFamily(key: String): String?

    fun fontSizePx(key: String): Int?

    val dark: Boolean
}

/**
 * Computes the `--asc-*` tokens (the names of packages/asc-ui/src/theme/tokens.ts) from the IDE look and feel
 * on open and on `LafManagerListener`, and builds the `theme` message pushed to the page (SPEC 24.5.2).
 * A value the IDE does not define is left out so the asc-ui default applies. Values are checked before they
 * reach CSS: anything outside a small safe alphabet is dropped.
 */
object AscTheme {
    /** token -> UIManager colour keys, first defined wins. */
    private val COLORS: List<Pair<String, List<String>>> = listOf(
        "--asc-bg" to listOf("Panel.background"),
        "--asc-fg" to listOf("Label.foreground", "Panel.foreground"),
        "--asc-fg-muted" to listOf("Component.infoForeground", "Label.disabledForeground"),
        "--asc-border" to listOf("Borders.color", "Component.borderColor"),
        "--asc-accent" to listOf("Link.activeForeground", "Component.focusColor"),
        "--asc-accent-fg" to listOf("Button.default.foreground", "Button.foreground"),
        "--asc-hover-bg" to listOf("List.hoverBackground", "Tree.hoverBackground"),
        "--asc-selection-bg" to listOf("List.selectionBackground", "Tree.selectionBackground"),
        "--asc-focus-ring" to listOf("Component.focusColor"),
        "--asc-added" to listOf("FileStatus.ADDED"),
        "--asc-removed" to listOf("FileStatus.DELETED"),
        "--asc-modified" to listOf("FileStatus.MODIFIED"),
        "--asc-conflict" to listOf("FileStatus.MERGED_WITH_CONFLICTS"),
        "--asc-warning" to listOf("Component.warningFocusColor", "Label.warningForeground"),
        "--asc-error" to listOf("Component.errorFocusColor", "Label.errorForeground"),
        "--asc-success" to listOf("Component.validOutlineColor", "Label.successForeground"),
        "--asc-badge-bg" to listOf("Counter.background", "Badge.background"),
        "--asc-badge-fg" to listOf("Counter.foreground", "Badge.foreground"),
    )

    private val SAFE_FONT = Regex("^[A-Za-z0-9 ._-]{1,64}$")

    fun css(c: Color): String =
        if (c.alpha == 255) {
            "#%02x%02x%02x".format(c.red, c.green, c.blue)
        } else {
            "rgba(${c.red}, ${c.green}, ${c.blue}, ${"%.3f".format(java.util.Locale.ROOT, c.alpha / 255.0)})"
        }

    fun tokens(src: ThemeSource): Map<String, String> {
        val out = LinkedHashMap<String, String>()
        for ((token, keys) in COLORS) {
            val c = keys.firstNotNullOfOrNull { src.color(it) } ?: continue
            out[token] = css(c)
        }
        src.fontFamily("Label.font")?.takeIf { SAFE_FONT.matches(it) }?.let { out["--asc-font-family"] = "$it, system-ui, sans-serif" }
        src.fontFamily("editor.font")?.takeIf { SAFE_FONT.matches(it) }?.let { out["--asc-font-mono"] = "$it, ui-monospace, monospace" }
        src.fontSizePx("Label.font")?.takeIf { it in 6..48 }?.let { out["--asc-font-size"] = "${it}px" }
        return out
    }

    /** The message pushed to the page: `{theme: {dark, tokens: {...}}}`. */
    fun message(src: ThemeSource): String {
        val tokens = JsonObject()
        for ((k, v) in tokens(src)) tokens.addProperty(k, v)
        val theme = JsonObject()
        theme.addProperty("dark", src.dark)
        theme.add("tokens", tokens)
        val o = JsonObject()
        o.add("theme", theme)
        return o.toString()
    }
}
