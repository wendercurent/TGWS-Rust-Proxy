package com.amurcanov.tgwsproxy

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LogPrivacyTest {
    @Test
    fun redactsSecretsTokensEmailsIdsAndIps() {
        val raw = listOf(
            "Link https://t.me/proxy?server=127.0.0.1&port=1443&secret=dd0123456789abcdef0123456789abcdef",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig",
            "token=ghp_1234567890abcdef password=mySecretPass user_id=99887766",
            "contact user.name+tag@example.org from 149.154.167.220 or 2001:67c:4e8:f002::a",
            "rawHex=0123456789abcdef0123456789abcdef"
        ).joinToString("\n")

        val redacted = LogPrivacy.redact(raw)

        assertFalse(redacted.contains("0123456789abcdef"))
        assertFalse(redacted.contains("eyJhbGci"))
        assertFalse(redacted.contains("ghp_1234567890"))
        assertFalse(redacted.contains("mySecretPass"))
        assertFalse(redacted.contains("99887766"))
        assertFalse(redacted.contains("user.name+tag@example.org"))
        assertFalse(redacted.contains("149.154.167.220"))
        assertFalse(redacted.contains("2001:67c:4e8:f002::a"))
        assertTrue(redacted.contains("[proxy link]"))
        assertTrue(redacted.contains("[redacted]"))
        assertTrue(redacted.contains("[email]"))
        assertTrue(redacted.contains("[IP]"))
    }

    @Test
    fun preservesNonSensitiveRouteDiagnostics() {
        val diag = """{"network":"cellular","generation":3,"directDegraded":true,"workingRoutes":1}"""
        val redacted = LogPrivacy.redact(diag)
        assertTrue(redacted.contains("\"network\":\"cellular\""))
        assertTrue(redacted.contains("\"workingRoutes\":1"))
    }
}
