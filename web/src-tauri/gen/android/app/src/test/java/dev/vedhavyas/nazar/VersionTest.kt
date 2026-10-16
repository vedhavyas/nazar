package dev.vedhavyas.nazar

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The offer decision and the release name the install check compares: one
 * parse serves both, and a version it cannot read must never be treated as
 * one it can.
 */
class VersionTest {
  @Test
  fun `offers only a strictly newer release`() {
    assertTrue(isNewerVersion("1.0.116", "1.0.115"))
    assertFalse(isNewerVersion("1.0.115", "1.0.115"))
    assertFalse(isNewerVersion("1.0.114", "1.0.115"))
    assertFalse(isNewerVersion("1.1.0", "2.0.0"))
  }

  @Test
  fun `reads a v prefix on either side`() {
    assertTrue(isNewerVersion("v1.0.116", "1.0.115"))
    assertTrue(isNewerVersion("1.0.116", "v1.0.115"))
    assertEquals("1.0.116", normalizeVersion("v1.0.116"))
    assertEquals("1.0.116", normalizeVersion(" 1.0.116 "))
  }

  @Test
  fun `refuses anything that is not three integers`() {
    val odd = listOf("1.0.116-rc1", "1.0.116.1", "1.0.116-beta", "x.1.0.116", "1.0", "", "v")
    for (version in odd) {
      assertFalse(version, isNewerVersion(version, "1.0.115"))
      assertNull(version, normalizeVersion(version))
    }
  }
}
