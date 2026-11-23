package com.amurcanov.tgwsproxy

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ProxyControllerSecretTest {

    @Test
    fun acceptsValid32HexSecrets() {
        assertTrue(ProxyController.isValidSecret("0123456789abcdef0123456789abcdef"))
        assertTrue(ProxyController.isValidSecret("ABCDEF0123456789abcdef0123456789"))
    }

    @Test
    fun rejectsEmptyShortLongAndNonHexSecrets() {
        assertFalse(ProxyController.isValidSecret(""))
        assertFalse(ProxyController.isValidSecret("0123456789abcdef"))
        assertFalse(ProxyController.isValidSecret("0123456789abcdef0123456789abcdef00"))
        assertFalse(ProxyController.isValidSecret("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"))
    }
}
