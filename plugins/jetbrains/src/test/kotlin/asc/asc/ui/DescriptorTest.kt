package asc.asc.ui

import javax.xml.parsers.DocumentBuilderFactory
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element

/**
 * plugin.xml against the code: every class it names exists, every icon it names ships, the tool window is
 * registered, and the "asc Views" menu has one action per tool window tab, in tab order.
 */
class DescriptorTest {
    private val loader = javaClass.classLoader

    private val root: Element by lazy {
        val stream = loader.getResourceAsStream("META-INF/plugin.xml") ?: error("plugin.xml is not on the classpath")
        val f = DocumentBuilderFactory.newInstance()
        f.setFeature("http://apache.org/xml/features/disallow-doctype-decl", true)
        stream.use { f.newDocumentBuilder().parse(it).documentElement }
    }

    private fun elements(): List<Element> {
        val all = root.getElementsByTagName("*")
        return (0 until all.length).map { all.item(it) as Element }
    }

    @Test fun everyNamedClassExists() {
        val attrs = listOf("implementation", "serviceImplementation", "vcsClass", "factoryClass", "class")
        val names = elements().flatMap { e -> attrs.mapNotNull { a -> e.getAttribute(a).takeIf { it.startsWith("asc.") } } }
        assertTrue("plugin.xml names no asc classes", names.size > 20)
        for (n in names) Class.forName(n, false, loader)
    }

    @Test fun everyIconShips() {
        for (icon in elements().map { it.getAttribute("icon") }.filter { it.isNotEmpty() }) {
            assertNotNull("missing icon $icon", loader.getResource(icon.removePrefix("/")))
            assertNotNull("missing dark icon for $icon", loader.getResource(icon.removePrefix("/").replace(".svg", "_dark.svg")))
        }
    }

    @Test fun theToolWindowIsRegistered() {
        val tw = elements().single { it.tagName == "toolWindow" }
        assertEquals("asc", tw.getAttribute("id"))
        assertEquals(AscToolWindowFactory::class.java.name, tw.getAttribute("factoryClass"))
    }

    @Test fun theViewsMenuMatchesTheTabs() {
        val group = elements().single { it.tagName == "group" && it.getAttribute("id") == "asc.asc.OpenView" }
        val actions = group.getElementsByTagName("action")
        val texts = (0 until actions.length).map { (actions.item(it) as Element).getAttribute("text") }
        assertEquals(AscPage.VIEWS.keys.toList(), texts)
    }
}
