package com.amurcanov.tgwsproxy.ui

internal object DomainList {
    private val separators = Regex("[,;\\s]+")

    fun normalize(raw: String): List<String> {
        val seen = LinkedHashSet<String>()
        for (token in raw.split(separators)) {
            val item = token.trim().lowercase()
            if (isValid(item)) seen.add(item)
        }
        return seen.toList()
    }

    private fun isValid(domain: String): Boolean {
        if (domain.isEmpty() || domain.length > 253 || domain.startsWith('.') || domain.endsWith('.')) {
            return false
        }
        val labels = domain.split('.')
        if (labels.size < 2) return false
        for (label in labels) {
            if (label.isEmpty() || label.length > 63 || label.startsWith('-') || label.endsWith('-')) {
                return false
            }
            if (label.any { it !in 'a'..'z' && it !in '0'..'9' && it != '-' }) return false
        }
        val tld = labels.last()
        return tld.length >= 2 && tld.any { it in 'a'..'z' }
    }
}
