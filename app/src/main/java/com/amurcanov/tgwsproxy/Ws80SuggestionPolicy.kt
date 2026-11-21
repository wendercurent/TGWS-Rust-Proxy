package com.amurcanov.tgwsproxy

import org.json.JSONObject

internal data class Ws80SuggestionSnapshot(
    val networkKind: Int,
    val generation: Long,
    val secureFailures: Int,
    val workingRoutes: Int,
    val hasRecentPayload: Boolean,
    val cfEnabled: Boolean,
    val cfPlainWs80: Boolean,
) {
    companion object {
        const val NETWORK_WIFI = 1
        const val NETWORK_CELLULAR = 2

        fun parse(rawJson: String?): Ws80SuggestionSnapshot? {
            if (rawJson.isNullOrBlank()) return null
            return runCatching {
                val obj = JSONObject(rawJson)
                Ws80SuggestionSnapshot(
                    networkKind = obj.optInt("network", 0),
                    generation = obj.optLong("generation", 0L),
                    secureFailures = obj.optInt("secureFailures", 0),
                    workingRoutes = obj.optInt("workingRoutes", 0),
                    hasRecentPayload = !obj.isNull("lastPayloadSeconds"),
                    cfEnabled = obj.optBoolean("cfEnabled", false),
                    cfPlainWs80 = obj.optBoolean("cfPlainWs80", false),
                )
            }.getOrNull()
        }
    }
}

internal object Ws80SuggestionPolicy {
    const val MIN_SECURE_FAILURES = 3
    const val DISMISS_COOLDOWN_MS = 6L * 60L * 60L * 1000L

    fun shouldSuggest(
        isRunning: Boolean,
        snapshot: Ws80SuggestionSnapshot?,
        savedCfPlainWs80: Boolean,
        dismissedUntilMs: Long,
        neverSuggest: Boolean,
        acknowledgedGeneration: Long,
        nowMs: Long,
    ): Boolean {
        if (!isRunning || snapshot == null) return false
        if (snapshot.networkKind != Ws80SuggestionSnapshot.NETWORK_CELLULAR) return false
        if (!snapshot.cfEnabled || savedCfPlainWs80 || snapshot.cfPlainWs80) return false
        if (neverSuggest || nowMs < dismissedUntilMs) return false
        if (snapshot.generation > 0L && snapshot.generation == acknowledgedGeneration) return false
        if (snapshot.workingRoutes > 0 || snapshot.hasRecentPayload) return false
        return snapshot.secureFailures >= MIN_SECURE_FAILURES
    }
}
