package asc.asc.bridge

import asc.v1.BridgeOuterClass.BridgeFrame
import java.io.EOFException
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/** `u32 little-endian length || BridgeFrame`, at most 16 MiB per frame (SPEC 24.2). */
const val MAX_FRAME: Int = 16 * 1024 * 1024

class FrameException(message: String) : Exception(message)

fun encodeFrame(frame: BridgeFrame): ByteArray {
    val body = frame.toByteArray()
    if (body.size > MAX_FRAME) throw FrameException("frame is larger than 16 MiB")
    return ByteBuffer.allocate(4 + body.size).order(ByteOrder.LITTLE_ENDIAN).putInt(body.size).put(body).array()
}

fun writeFrame(out: OutputStream, frame: BridgeFrame) {
    out.write(encodeFrame(frame))
    out.flush()
}

/** Blocking read of one frame; null at a clean end of stream. */
fun readFrame(input: InputStream): BridgeFrame? {
    val head = ByteArray(4)
    var got = 0
    while (got < 4) {
        val n = input.read(head, got, 4 - got)
        if (n < 0) {
            if (got == 0) return null
            throw EOFException("stream ended inside a frame header")
        }
        got += n
    }
    val len = ByteBuffer.wrap(head).order(ByteOrder.LITTLE_ENDIAN).int
    if (len < 0 || len > MAX_FRAME) throw FrameException("frame is larger than 16 MiB")
    val body = ByteArray(len)
    var at = 0
    while (at < len) {
        val n = input.read(body, at, len - at)
        if (n < 0) throw EOFException("stream ended inside a frame")
        at += n
    }
    return try {
        BridgeFrame.parseFrom(body)
    } catch (_: com.google.protobuf.InvalidProtocolBufferException) {
        throw FrameException("frame does not decode")
    }
}
