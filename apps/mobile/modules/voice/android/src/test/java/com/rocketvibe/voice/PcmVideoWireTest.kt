package com.rocketvibe.voice

import java.io.ByteArrayInputStream
import java.io.DataInputStream
import java.io.IOException
import org.junit.Assert.*
import org.junit.Test

class PcmVideoWireTest {
  private fun bytes(vararg values: Int) = ByteArray(values.size) { values[it].toByte() }
  private fun stream(bytes: ByteArray) = DataInputStream(ByteArrayInputStream(bytes))

  @Test fun aPictureGoesOutAsTheEngineReadsIt() {
    val sent = PcmVideoWire.picture(0x01020304, bytes(0, 0, 0, 1, 0x65))
    assertArrayEquals(bytes(1, 9, 0, 0, 0, 4, 3, 2, 1, 0, 0, 0, 1, 0x65), sent)
    val (kind, payload) = PcmVideoWire.read(stream(sent))!!
    assertEquals(PcmVideoWire.UP_PICTURE, kind)
    assertArrayEquals(sent.copyOfRange(5, sent.size), payload)
  }

  @Test fun sendingAndAKeyframeRequestAreOneByteAndNothing() {
    assertArrayEquals(bytes(2, 1, 0, 0, 0, 1), PcmVideoWire.sending(true))
    assertArrayEquals(bytes(2, 1, 0, 0, 0, 0), PcmVideoWire.sending(false))
    assertArrayEquals(bytes(3, 0, 0, 0, 0), PcmVideoWire.message(PcmVideoWire.UP_KEYFRAME, ByteArray(0)))
  }

  @Test fun theEnginesMessagesAreReadInTurnThenTheEnd() {
    val picture = bytes(1, 10, 0, 0, 0, 1, 3, 0x10, 0, 0, 0, 0, 0, 1, 0x65)
    val input = stream(picture + bytes(2, 1, 0, 0, 0, 0) + bytes(4, 0, 0, 0, 0))
    val (kind, payload) = PcmVideoWire.read(input)!!
    assertEquals(PcmVideoWire.DOWN_PICTURE, kind)
    val peer = PcmVideoWire.peerPicture(payload)!!
    assertTrue(peer.keyframe)
    assertEquals(3, peer.turns)
    assertEquals(0x10, peer.timestamp)
    assertArrayEquals(bytes(0, 0, 1, 0x65), peer.unit)
    assertEquals(PcmVideoWire.DOWN_PEER to 0, PcmVideoWire.read(input)!!.let { it.first to it.second[0].toInt() })
    assertEquals(PcmVideoWire.DOWN_REFUSED, PcmVideoWire.read(input)!!.first)
    assertNull(PcmVideoWire.read(input))
  }

  @Test fun aPictureTooShortOrAMessageTooLargeIsRefused() {
    assertNull(PcmVideoWire.peerPicture(bytes(1, 0, 0, 0, 0, 0)))
    val huge = bytes(1) + bytes(1, 0, 0x40, 0)
    try { PcmVideoWire.read(stream(huge)); fail("over 4 MiB") } catch (_: IOException) {}
  }

  @Test fun aKeyframeIsLedByTheParameterSetsOnce() {
    val sets = bytes(0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce)
    val idr = bytes(0, 0, 0, 1, 0x65, 0x88)
    val delta = bytes(0, 0, 1, 0x41, 0x9a)
    assertArrayEquals(sets + idr, PcmVideoWire.accessUnit(idr, true, sets))
    assertArrayEquals(delta, PcmVideoWire.accessUnit(delta, false, sets))
    assertArrayEquals(sets + idr, PcmVideoWire.accessUnit(sets + idr, true, sets))
    assertArrayEquals(idr, PcmVideoWire.accessUnit(idr, true, null))
    assertTrue(PcmVideoWire.carriesParameterSets(bytes(0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x67)))
    assertFalse(PcmVideoWire.carriesParameterSets(idr))
  }

  @Test fun picturesFitWithin720pInWholeMacroblocks() {
    assertEquals(1280 to 720, PcmVideoWire.fitted(1920, 1080))
    assertEquals(720 to 1280, PcmVideoWire.fitted(1080, 1920))
    assertEquals(640 to 480, PcmVideoWire.fitted(640, 480))
    assertEquals(1280 to 576, PcmVideoWire.fitted(2400, 1080))
    assertEquals(16 to 16, PcmVideoWire.fitted(2, 2))
  }

  @Test fun theClockIs90kHzAndWraps() {
    assertEquals(90_000, PcmVideoWire.clock(1_000_000))
    assertEquals(((1L shl 32) / 90_000 * 90_000 + 90_000 - (1L shl 32)).toInt(), PcmVideoWire.clock(((1L shl 32) / 90_000 + 1) * 1_000_000))
  }
}
