package com.amurcanov.tgwsproxy

import androidx.compose.runtime.Immutable

internal object LogPrivacy {
    private val proxyLink = Regex("""(?i)\b(?:https?://t\.me/|tg://)proxy(?:\?[^\s"'<>]*)?""")
    private val secret = Regex("""(?iu)\b(secret|secret_key|key|ключ)(\s*[=:]\s*)[^\s,;]+""")
    private val hexKey = Regex("""(?i)(?<![a-f0-9])[a-f0-9]{32,}(?![a-f0-9])""")
    private val ipv6 = Regex("""(?<![\w:])(?:[0-9a-fA-F]{0,4}:){2,}(?:(?:\d{1,3}\.){3}\d{1,3}|[0-9a-fA-F:]{0,39})(?:%[\w.-]+)?(?![\w:])""")
    private val ipv4 = Regex("""(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])""")

    fun redact(message: String): String {
        val linksHidden = proxyLink.replace(message, "[proxy link]")
        val keysHidden = hexKey.replace(secret.replace(linksHidden, "$1$2[redacted]"), "[redacted]")
        return ipv4.replace(ipv6.replace(keysHidden, "[IP]"), "[IP]")
    }
}

/**
 * Immutable data class for log entries — ensures Compose skips recomposition
 * when the reference hasn't changed.
 */
@Immutable
data class LogEntry(
    val key: String,
    val message: String,
    val count: Int,
    val isError: Boolean,
    val priority: Int, // 3=DEBUG, 4=INFO, 5=WARN, 6=ERROR
    val isEssential: Boolean = false
)
