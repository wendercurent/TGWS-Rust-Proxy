package com.amurcanov.tgwsproxy.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class DomainListTest {
    @Test
    fun keepsDistinctValidNamesAndDropsTheRest() {
        assertEquals(
            listOf("a.example.com", "second.example"),
            DomainList.normalize("A.Example.com, bad, A.Example.com; second.example")
        )
        assertEquals(emptyList<String>(), DomainList.normalize("not a domain"))
    }
}
