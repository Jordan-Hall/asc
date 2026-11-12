package asc.asc.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class NotifyTest {
    @Test fun notificationTextIsEscapedAndKeepsLineBreaks() {
        assertEquals("a &lt;b&gt; &amp; &quot;c&quot; &#39;d&#39;", AscActions.notificationHtml("a <b> & \"c\" 'd'"))
        assertEquals("one<br>two", AscActions.notificationHtml("one\ntwo"))
    }

    @Test fun aTagSubjectOrStashMessageCannotInjectMarkup() {
        val row = StashRow(0, "ab".repeat(20), "<a href=\"https://evil.example\">click</a><img src=x>")
        val html = AscActions.notificationHtml(StashPlan.text(listOf(row)))
        assertFalse(html.contains("<a "))
        assertFalse(html.contains("<img"))
        val tag = TagRow("v1", "ab".repeat(20), true, "<script>x</script>")
        assertFalse(AscActions.notificationHtml(TagPlan.label(tag)).contains("<script>"))
    }
}
