package com.amurcanov.tgwsproxy

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class LogPrivacyTest {
    @Test
    fun removesProxyLinksAndTheirSecrets() {
        val key = "0123456789abcdef0123456789abcdef"
        val line = "https://t.me/proxy?server=192.0.2.1&port=1443&secret=dd$key tg://proxy?secret=ee$key"
        assertEquals("[proxy link] [proxy link]", LogPrivacy.redact(line))
    }

    @Test
    fun masksAddressesAndKeepsUsefulRouteDetails() {
        val line = "DC2 192.0.2.1:443 [2001:db8::1]:443 ::ffff:192.0.2.2 Ключ: private timeout"
        val safe = LogPrivacy.redact(line)
        assertEquals("DC2 [IP]:443 [[IP]]:443 [IP] Ключ: [redacted] timeout", safe)
        assertFalse(safe.contains("private"))
        assertEquals(safe, LogPrivacy.redact(safe))
    }
}
