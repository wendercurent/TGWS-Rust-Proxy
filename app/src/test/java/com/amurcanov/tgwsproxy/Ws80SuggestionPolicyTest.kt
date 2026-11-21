package com.amurcanov.tgwsproxy

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class Ws80SuggestionPolicyTest {

    private fun cellularFailingSnapshot(
        networkKind: Int = Ws80SuggestionSnapshot.NETWORK_CELLULAR,
        generation: Long = 3L,
        secureFailures: Int = 3,
        workingRoutes: Int = 0,
        hasRecentPayload: Boolean = false,
        cfEnabled: Boolean = true,
        cfPlainWs80: Boolean = false,
    ) = Ws80SuggestionSnapshot(
        networkKind = networkKind,
        generation = generation,
        secureFailures = secureFailures,
        workingRoutes = workingRoutes,
        hasRecentPayload = hasRecentPayload,
        cfEnabled = cfEnabled,
        cfPlainWs80 = cfPlainWs80,
    )

    @Test
    fun suggestsOnlyWhenAllCellularFailureConditionsHold() {
        assertTrue(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = 1_000_000L,
            )
        )
    }

    @Test
    fun doesNotSuggestOnWifiOrWhenStopped() {
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(networkKind = Ws80SuggestionSnapshot.NETWORK_WIFI),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = 1_000_000L,
            )
        )
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = false,
                snapshot = cellularFailingSnapshot(),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = 1_000_000L,
            )
        )
    }

    @Test
    fun doesNotSuggestWhenIdleOrHealthyRouteExists() {
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(secureFailures = 0),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = 1_000_000L,
            )
        )
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(workingRoutes = 1),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = 1_000_000L,
            )
        )
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(hasRecentPayload = true),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = 1_000_000L,
            )
        )
    }

    @Test
    fun respectsWs80ConsentDismissalCooldownAndAcknowledgedGeneration() {
        val now = 1_000_000L
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(cfPlainWs80 = true),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = now,
            )
        )
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(),
                savedCfPlainWs80 = true,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = now,
            )
        )
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(),
                savedCfPlainWs80 = false,
                dismissedUntilMs = now + 60_000L,
                neverSuggest = false,
                acknowledgedGeneration = 0L,
                nowMs = now,
            )
        )
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = true,
                acknowledgedGeneration = 0L,
                nowMs = now,
            )
        )
        // Acknowledged for generation 3 -> does not spam on generation 3, but can suggest again on a new generation 4.
        assertFalse(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(generation = 3L),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 3L,
                nowMs = now,
            )
        )
        assertTrue(
            Ws80SuggestionPolicy.shouldSuggest(
                isRunning = true,
                snapshot = cellularFailingSnapshot(generation = 4L),
                savedCfPlainWs80 = false,
                dismissedUntilMs = 0L,
                neverSuggest = false,
                acknowledgedGeneration = 3L,
                nowMs = now,
            )
        )
    }
}
