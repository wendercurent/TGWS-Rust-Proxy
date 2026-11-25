package com.amurcanov.tgwsproxy

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NetworkSnapshotPolicyTest {

    private val wifiLinks = listOf("192.168.1.10/24") to listOf("192.168.1.1")
    private val lteLinks = listOf("10.0.0.2/32") to listOf("8.8.8.8")

    @Test
    fun validatedRecoveryOnSameWifiTriggersNonHandoverEpochBump() {
        val unvalidated = NetworkSnapshot(
            networkHandle = 100L,
            transport = 1,
            validated = false,
            hasInternet = true,
            links = wifiLinks,
        )
        val validated = unvalidated.copy(validated = true)

        assertEquals(
            NetworkTransition.Notify(transport = 1, replaced = false),
            NetworkSnapshotPolicy.evaluate(unvalidated, validated),
        )
    }

    @Test
    fun losingValidationOnSameNetworkDoesNotChurnUntilRecovery() {
        val validated = NetworkSnapshot(
            networkHandle = 100L,
            transport = 1,
            validated = true,
            hasInternet = true,
            links = wifiLinks,
        )
        val unvalidated = validated.copy(validated = false)

        assertEquals(
            NetworkTransition.Unchanged,
            NetworkSnapshotPolicy.evaluate(validated, unvalidated),
        )
        assertEquals(
            NetworkTransition.Notify(transport = 1, replaced = false),
            NetworkSnapshotPolicy.evaluate(unvalidated, validated),
        )
    }

    @Test
    fun wifiToLteHandoverSetsReplacedTrue() {
        val wifi = NetworkSnapshot(100L, 1, validated = true, hasInternet = true, links = wifiLinks)
        val lte = NetworkSnapshot(200L, 2, validated = true, hasInternet = true, links = lteLinks)

        assertEquals(
            NetworkTransition.Notify(transport = 2, replaced = true),
            NetworkSnapshotPolicy.evaluate(wifi, lte),
        )
    }

    @Test
    fun duplicateCallbackIsIgnored() {
        val wifi = NetworkSnapshot(100L, 1, validated = true, hasInternet = true, links = wifiLinks)
        assertEquals(
            NetworkTransition.Unchanged,
            NetworkSnapshotPolicy.evaluate(wifi, wifi.copy()),
        )
    }

    @Test
    fun offlineSnapshotMarksNetworkUnavailable() {
        val offline = NetworkSnapshot(null, 0, validated = false, hasInternet = false, links = null)
        assertFalse(NetworkSnapshotPolicy.isNetworkAvailable(offline))

        val online = NetworkSnapshot(100L, 1, validated = true, hasInternet = true, links = wifiLinks)
        assertTrue(NetworkSnapshotPolicy.isNetworkAvailable(online))
    }
}
