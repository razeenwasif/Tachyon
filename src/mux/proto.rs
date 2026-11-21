//! Tachyon Mux v2 binary framing codec.
//!
//! Every message on a multiplexed transport (local Unix socket, SSH stdio
//! bridge, Windows named pipe, or future Tailscale TCP) uses a fixed 9-byte
//! little-endian header:
//!
//! ```text
//!  0       4      5         9                    9 + len
//! ┌───────┬──────┬─────────┬────────────────────────────┐
//! │  len  │ kind │ channel │          payload           │
//! │  u32  │  u8  │   u32   │         [u8; len]          │
//! └───────┴──────┴─────────┴────────────────────────────┘
//! ```
//!
//! Control RPCs and events carry UTF-8 JSON; PTY streams carry raw bytes (or
//! LZ4-compressed `Term::to_vt()` restore snapshots) with zero JSON/base64
//! overhead.

use std::io::{self, Read, Write};

use serde_json::Value;

/// Wire protocol version negotiated in `hello`.
pub const PROTOCOL_VERSION: u32 = 2;

/// Maximum payload accepted in a single frame (64 MiB). Protects against
/// corrupt streams allocating unbounded memory while still leaving generous
/// room for large screen snapshots or inline images.
pub const MAX_FRAME_LEN: usize = 64 << 20;

/// Snapshot payload flags.
pub const SNAPSHOT_RAW: u8 = 0;
pub const SNAPSHOT_LZ4: u8 = 1;

/// Minimum snapshot length worth LZ4-compressing.
const LZ4_THRESHOLD: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    RpcReq = 0x01,
    RpcRes = 0x02,
    Event = 0x03,
    PtyOut = 0x04,
    PtyIn = 0x05,
    PtySnapshot = 0x06,
    PtyResize = 0x07,
    Ping = 0x08,
    Pong = 0x09,
}

impl FrameKind {
    pub fn from_u8(v: u8) -> io::Result<Self> {
        match v {
            0x01 => Ok(Self::RpcReq),
            0x02 => Ok(Self::RpcRes),
            0x03 => Ok(Self::Event),
            0x04 => Ok(Self::PtyOut),
            0x05 => Ok(Self::PtyIn),
            0x06 => Ok(Self::PtySnapshot),
            0x07 => Ok(Self::PtyResize),
            0x08 => Ok(Self::Ping),
            0x09 => Ok(Self::Pong),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown mux frame kind: 0x{other:02x}"),
            )),
        }
    }

    /// Control frames bypass PTY output backpressure queues so interactive
    /// RPCs, pings, snapshots, and events never starve behind a noisy shell.
    pub fn is_priority(self) -> bool {
        !matches!(self, Self::PtyOut)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub channel: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn rpc_req(req_id: u32, body: &Value) -> io::Result<Self> {
        let payload = serde_json::to_vec(body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(Self { kind: FrameKind::RpcReq, channel: req_id, payload })
    }

    pub fn rpc_res(req_id: u32, body: &Value) -> io::Result<Self> {
        let payload = serde_json::to_vec(body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(Self { kind: FrameKind::RpcRes, channel: req_id, payload })
    }

    pub fn event(body: &Value) -> io::Result<Self> {
        let payload = serde_json::to_vec(body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(Self { kind: FrameKind::Event, channel: 0, payload })
    }

    pub fn pty_out(stream_id: u32, bytes: Vec<u8>) -> Self {
        Self { kind: FrameKind::PtyOut, channel: stream_id, payload: bytes }
    }

    pub fn pty_in(stream_id: u32, bytes: Vec<u8>) -> Self {
        Self { kind: FrameKind::PtyIn, channel: stream_id, payload: bytes }
    }

    pub fn pty_snapshot(stream_id: u32, vt: &[u8]) -> Self {
        let payload = encode_snapshot(vt);
        Self { kind: FrameKind::PtySnapshot, channel: stream_id, payload }
    }

    pub fn pty_resize(stream_id: u32, cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> Self {
        Self {
            kind: FrameKind::PtyResize,
            channel: stream_id,
            payload: encode_resize(cols, rows, cell_w, cell_h).to_vec(),
        }
    }

    pub fn ping(seq: u32, sent_ns: u64) -> Self {
        Self { kind: FrameKind::Ping, channel: seq, payload: sent_ns.to_le_bytes().to_vec() }
    }

    pub fn pong(seq: u32, sent_ns: u64) -> Self {
        Self { kind: FrameKind::Pong, channel: seq, payload: sent_ns.to_le_bytes().to_vec() }
    }

    pub fn parse_json(&self) -> io::Result<Value> {
        serde_json::from_slice(&self.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid json frame: {e}")))
    }
}

/// Write a single framed message to `w`.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> io::Result<()> {
    if frame.payload.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mux frame too large: {} bytes", frame.payload.len()),
        ));
    }
    let mut hdr = [0u8; 9];
    hdr[0..4].copy_from_slice(&(frame.payload.len() as u32).to_le_bytes());
    hdr[4] = frame.kind as u8;
    hdr[5..9].copy_from_slice(&frame.channel.to_le_bytes());
    w.write_all(&hdr)?;
    if !frame.payload.is_empty() {
        w.write_all(&frame.payload)?;
    }
    w.flush()
}

/// Read the next framed message from `r`. Returns `UnexpectedEof` on clean close.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr)?;
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("mux frame exceeds limit: {len} bytes"),
        ));
    }
    let kind = FrameKind::from_u8(hdr[4])?;
    let channel = u32::from_le_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]);
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok(Frame { kind, channel, payload })
}

/// Encode VT snapshot bytes with optional LZ4 compression.
pub fn encode_snapshot(vt: &[u8]) -> Vec<u8> {
    if vt.len() >= LZ4_THRESHOLD {
        let compressed = lz4_flex::compress_prepend_size(vt);
        if compressed.len() < vt.len() {
            let mut out = Vec::with_capacity(1 + compressed.len());
            out.push(SNAPSHOT_LZ4);
            out.extend_from_slice(&compressed);
            return out;
        }
    }
    let mut out = Vec::with_capacity(1 + vt.len());
    out.push(SNAPSHOT_RAW);
    out.extend_from_slice(vt);
    out
}

/// Decode a `PTY_SNAPSHOT` payload back to raw VT bytes.
pub fn decode_snapshot(payload: &[u8]) -> io::Result<Vec<u8>> {
    let Some((&flags, rest)) = payload.split_first() else {
        return Ok(Vec::new());
    };
    match flags {
        SNAPSHOT_RAW => Ok(rest.to_vec()),
        SNAPSHOT_LZ4 => lz4_flex::decompress_size_prepended(rest)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad lz4 snapshot: {e}"))),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown snapshot flags: {other}"),
        )),
    }
}

pub fn encode_resize(cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&cols.to_le_bytes());
    out[2..4].copy_from_slice(&rows.to_le_bytes());
    out[4..6].copy_from_slice(&cell_w.to_le_bytes());
    out[6..8].copy_from_slice(&cell_h.to_le_bytes());
    out
}

pub fn decode_resize(payload: &[u8]) -> io::Result<(u16, u16, u16, u16)> {
    if payload.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("resize payload must be 8 bytes, got {}", payload.len()),
        ));
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    let cell_w = u16::from_le_bytes([payload[4], payload[5]]);
    let cell_h = u16::from_le_bytes([payload[6], payload[7]]);
    Ok((cols, rows, cell_w, cell_h))
}

pub fn decode_timestamp(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("timestamp payload must be 8 bytes, got {}", payload.len()),
        ));
    }
    Ok(u64::from_le_bytes(payload.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frame_roundtrip_all_kinds() {
        let mut buf = Vec::new();
        let frames = vec![
            Frame::rpc_req(1, &json!({"op": "hello", "protocol": PROTOCOL_VERSION})).unwrap(),
            Frame::rpc_res(1, &json!({"ok": true})).unwrap(),
            Frame::event(&json!({"event": "session.created", "id": "s_1"})).unwrap(),
            Frame::pty_out(7, b"hello world\r\n".to_vec()),
            Frame::pty_in(7, b"ls\n".to_vec()),
            Frame::pty_snapshot(7, b"\x1b[2Jshort"),
            Frame::pty_snapshot(7, &"repetitive line of terminal output\r\n".repeat(40).into_bytes()),
            Frame::pty_resize(7, 120, 40, 16, 32),
            Frame::ping(99, 123_456_789),
            Frame::pong(99, 123_456_789),
        ];
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut cursor = io::Cursor::new(buf);
        for expected in &frames {
            let got = read_frame(&mut cursor).unwrap();
            assert_eq!(&got, expected);
        }
        assert_eq!(read_frame(&mut cursor).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn snapshot_uses_lz4_for_compressible_payloads() {
        let big = "loss=0.412 acc=88.3% step=100\r\n".repeat(100);
        let enc = encode_snapshot(big.as_bytes());
        assert_eq!(enc[0], SNAPSHOT_LZ4);
        assert!(enc.len() < big.len() / 2);
        assert_eq!(decode_snapshot(&enc).unwrap(), big.as_bytes());

        let tiny = b"hi";
        let enc_tiny = encode_snapshot(tiny);
        assert_eq!(enc_tiny[0], SNAPSHOT_RAW);
        assert_eq!(decode_snapshot(&enc_tiny).unwrap(), tiny);
    }

    #[test]
    fn resize_and_timestamp_codecs_validate_length() {
        let r = encode_resize(132, 43, 14, 28);
        assert_eq!(decode_resize(&r).unwrap(), (132, 43, 14, 28));
        assert!(decode_resize(&[0, 1, 2]).is_err());

        let f = Frame::ping(5, 42_000);
        assert_eq!(decode_timestamp(&f.payload).unwrap(), 42_000);
        assert!(decode_timestamp(&[1, 2]).is_err());
    }
}
