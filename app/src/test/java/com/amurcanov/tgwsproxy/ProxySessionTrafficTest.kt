package com.amurcanov.tgwsproxy

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ProxySessionTrafficTest {
    @Test
    fun exactCountersTakePriorityOverRoundedUnits() {
        val sample = ProxySessionTraffic.parse("up=2.0MB down=0.0B up_bytes=2097181 down_bytes=23")
        assertEquals(ProxySessionTraffic(2_097_181, 23), sample)
        assertEquals(2_097_204L, sample?.total)
    }

    @Test
    fun readsBothDirectionsAndConvertsNativeUnits() {
        val traffic = ProxySessionTraffic.parse("total=5 active=2 up=1.5KB down=2.0MB")
        assertEquals(ProxySessionTraffic(1_536, 2_097_152), traffic)
        assertEquals(2_098_688L, traffic?.total)
    }

    @Test
    fun zeroTrafficIsASampleButMissingCountersAreNot() {
        assertEquals(ProxySessionTraffic(0, 0), ProxySessionTraffic.parse("down=0.0B up=0.0B"))
        assertNull(ProxySessionTraffic.parse(null))
        assertNull(ProxySessionTraffic.parse("active=2 up=3KB"))
    }
}
