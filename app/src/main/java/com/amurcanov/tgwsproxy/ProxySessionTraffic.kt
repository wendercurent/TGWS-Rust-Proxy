package com.amurcanov.tgwsproxy

internal data class ProxySessionTraffic(val sent: Long, val received: Long) {
    val total: Long get() = sent + received

    companion object {
        private val exactBytes = Regex("""(?:^|\s)(up|down)_bytes=([0-9]+)(?=\s|$)""")
        private val byteCount = Regex("""(?:^|\s)(up|down)=([0-9]+(?:[.,][0-9]+)?)(B|KB|MB|GB|TB)(?=\s|$)""")
        private val units = listOf("B", "KB", "MB", "GB", "TB")

        fun parse(stats: String?): ProxySessionTraffic? {
            val summary = stats ?: return null
            val exact = exactBytes.findAll(summary).mapNotNull { match ->
                val (direction, count) = match.destructured
                count.toLongOrNull()?.let { direction to it }
            }.toMap()
            if (exact.size == 2) return fromCounters(exact)

            // Builds with an older native library still return rounded units.
            val counters = byteCount.findAll(summary).mapNotNull { match ->
                val (direction, quantity, unit) = match.destructured
                val multiplier = 1L shl (10 * units.indexOf(unit))
                val amount = (quantity.replace(',', '.').toDoubleOrNull() ?: return@mapNotNull null) * multiplier
                if (amount < Long.MAX_VALUE.toDouble()) direction to amount.toLong() else null
            }.toMap()
            return fromCounters(counters)
        }

        private fun fromCounters(counters: Map<String, Long>): ProxySessionTraffic? {
            val sent = counters["up"] ?: return null
            val received = counters["down"] ?: return null
            if (sent > Long.MAX_VALUE - received) return null
            return ProxySessionTraffic(sent, received)
        }
    }
}
