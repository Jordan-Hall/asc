package asc.asc.auth

import asc.asc.auth.HostRules.Root
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class HostRulesTest {
    @Test fun normalizeAcceptsHostsAndHttpsUrls() {
        assertEquals("forge.example.org", HostRules.normalize("  Forge.Example.org "))
        assertEquals("forge.example.org", HostRules.normalize("https://forge.example.org/"))
        assertEquals("forge.example.org:8443", HostRules.normalize("forge.example.org:8443"))
        assertEquals("localhost", HostRules.normalize("localhost"))
    }

    @Test fun normalizeRefusesEverythingElse() {
        for (bad in listOf(
            "", "http://forge.example.org", "ssh://forge.example.org", "user@forge.example.org",
            "forge.example.org/path", "forge.example.org?x", "forge.example.org#x", "forge example.org",
            "forge.example.org:0", "forge.example.org:65536", "forge.example.org:08", "-forge.example.org",
            "forge..example.org", "10.0.0.1", "forge\\example.org", "a".repeat(64) + ".org",
        )) assertNull(bad, HostRules.normalize(bad))
    }

    @Test fun ofRemoteTakesOnlyHttpsWithoutUserInformation() {
        assertEquals("forge.example.org", HostRules.ofRemote("https://forge.example.org/acme/app"))
        assertEquals("forge.example.org", HostRules.ofRemote("https://forge.example.org:443/acme/app"))
        assertEquals("forge.example.org:8443", HostRules.ofRemote("https://forge.example.org:8443/acme/app"))
        for (bad in listOf(
            "http://forge.example.org/a", "ssh://git@forge.example.org/a", "git@forge.example.org:a/b",
            "https://user@forge.example.org/a", "https:///forge.example.org/a", "/srv/repos/a",
            "https://forge.example.org/a\nb", "",
        )) assertNull(bad, HostRules.ofRemote(bad))
    }

    @Test fun aTokenHostNeedsEveryRootOnTheSameHost() {
        val a = Root("w1", "https://forge.example.org/acme/a")
        val b = Root("w2", "https://forge.example.org/acme/b")
        val other = Root("w3", "https://code.example.net/x/y")
        assertEquals("forge.example.org", HostRules.commonTokenHost(listOf(a, b)))
        assertNull(HostRules.commonTokenHost(listOf(a, other)))
        assertNull(HostRules.commonTokenHost(listOf(a, Root("w4", "ssh://git@forge.example.org/acme/c"))))
        assertNull(HostRules.commonTokenHost(listOf(Root("", "https://forge.example.org/acme/a"))))
        assertNull(HostRules.commonTokenHost(emptyList()))
    }

    @Test fun candidatesAreTheHttpsHostsOnceInOrder() {
        val roots = listOf(
            Root("w1", "https://code.example.net/x"), Root("w2", "ssh://git@forge.example.org/a"),
            Root("w3", "https://forge.example.org/a"), Root("w4", "https://code.example.net/y"),
        )
        assertEquals(listOf("code.example.net", "forge.example.org"), HostRules.candidates(roots))
    }

    @Test fun cleanTokenIsOneBoundedLine() {
        assertEquals("abc", HostRules.cleanToken("  abc \n"))
        assertNull(HostRules.cleanToken("   "))
        assertNull(HostRules.cleanToken("ab\ncd"))
        assertNull(HostRules.cleanToken("x".repeat(HostRules.MAX_TOKEN + 1)))
    }
}
