package com.amurcanov.tgwsproxy

internal data class NetworkSnapshot(
    val networkHandle: Long?,
    val transport: Int,
    val validated: Boolean,
    val hasInternet: Boolean,
    val links: Pair<List<String>, List<String>>?,
)

internal sealed class NetworkTransition {
    data object Unchanged : NetworkTransition()
    data class Notify(val transport: Int, val replaced: Boolean) : NetworkTransition()
}

internal object NetworkSnapshotPolicy {
    fun isNetworkAvailable(snapshot: NetworkSnapshot): Boolean {
        return snapshot.networkHandle != null && (snapshot.hasInternet || snapshot.validated)
    }

    fun evaluate(previous: NetworkSnapshot?, current: NetworkSnapshot): NetworkTransition {
        if (previous == null) {
            return NetworkTransition.Notify(
                transport = current.transport,
                replaced = current.networkHandle != null,
            )
        }
        if (previous.networkHandle != current.networkHandle) {
            return NetworkTransition.Notify(
                transport = current.transport,
                replaced = true,
            )
        }
        if (previous.transport != current.transport || previous.links != current.links) {
            return NetworkTransition.Notify(
                transport = current.transport,
                replaced = false,
            )
        }
        // WAN recovered on the same Network association: advance native epoch to clear
        // stale cooldowns and idle sockets without forcing handover on healthy relays.
        if (!previous.validated && current.validated && current.networkHandle != null) {
            return NetworkTransition.Notify(
                transport = current.transport,
                replaced = false,
            )
        }
        return NetworkTransition.Unchanged
    }
}
