package dev.tablink.display.video

import dev.tablink.display.protocol.Protocol
import org.junit.Assert.assertEquals
import org.junit.Test

class DisplayInfoTest {
    @Test
    fun landscapeSwapsPortraitDimensions() {
        val portrait = DisplayInfo(1752, 2800, listOf(60, 120), 266)
        assertEquals(DisplayInfo(2800, 1752, listOf(60, 120), 266), landscape(portrait))
    }

    @Test
    fun landscapeKeepsLandscapeDimensions() {
        val info = DisplayInfo(2800, 1752, listOf(60), 266)
        assertEquals(info, landscape(info))
    }

    @Test
    fun helloCarriesDisplayInfo() {
        val hello = buildHello(DisplayInfo(2800, 1752, listOf(60, 120), 266), 3)
        assertEquals(2800, hello.width)
        assertEquals(1752, hello.height)
        assertEquals(listOf(60, 120), hello.refreshHz)
        assertEquals(3, hello.caps)
        assertEquals(266, hello.dpi)
    }

    @Test
    fun helloUsesProtocolVersion() {
        assertEquals(Protocol.VERSION, buildHello(DisplayInfo(2800, 1752, listOf(60), 266), 1).version)
    }

    @Test
    fun displayInfoUsesNativeModeAndItsRates() {
        val modes = listOf(
            PanelMode(1752, 2800, 60.000004f),
            PanelMode(1752, 2800, 119.99999f),
            PanelMode(1095, 1750, 60f),
            PanelMode(1752, 2800, 60f),
        )
        assertEquals(DisplayInfo(2800, 1752, listOf(60, 120), 266), displayInfoOf(modes, 266))
    }

    @Test
    fun displayInfoOfNoModesIsEmpty() {
        assertEquals(DisplayInfo(0, 0, emptyList(), 266), displayInfoOf(emptyList(), 266))
    }
}
