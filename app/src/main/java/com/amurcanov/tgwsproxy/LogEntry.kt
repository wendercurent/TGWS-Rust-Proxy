package com.amurcanov.tgwsproxy

import androidx.compose.runtime.Immutable

internal object LogPrivacy {
    private val proxyLink = Regex("""(?i)\b(?:https?://t\.me/|tg://)proxy(?:\?[^\s"'<>]*)?""")
    private val bearerToken = Regex("""(?i)\bBearer\s+[A-Za-z0-9._\-~+/]+=*""")
    private val secret = Regex(
        """(?iu)\b(secret|secret_key|key|ключ|token|access_token|refresh_token|api_key|authorization|password|passwd|cookie|session|user_id|chat_id|uid|phone)(\s*[=:]\s*)[^\s,;]+"""
    )
    private val email = Regex("""(?i)\b[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}\b""")
    private val hexKey = Regex("""(?i)(?<![a-f0-9])[a-f0-9]{32,}(?![a-f0-9])""")
    private val ipv6 = Regex("""(?<![\w:])(?:[0-9a-fA-F]{0,4}:){2,}(?:(?:\d{1,3}\.){3}\d{1,3}|[0-9a-fA-F:]{0,39})(?:%[\w.-]+)?(?![\w:])""")
    private val ipv4 = Regex("""(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])""")

    fun redact(message: String): String {
        val linksHidden = proxyLink.replace(message, "[proxy link]")
        val bearerHidden = bearerToken.replace(linksHidden, "Bearer [redacted]")
        val secretsHidden = secret.replace(bearerHidden, "$1$2[redacted]")
        val emailsHidden = email.replace(secretsHidden, "[email]")
        val keysHidden = hexKey.replace(emailsHidden, "[redacted]")
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
