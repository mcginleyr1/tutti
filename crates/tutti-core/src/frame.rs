use crate::PaneId;

/// Wire format (little-endian): `[u32 len][u8 kind][payload]` where `len`
/// counts the kind byte plus payload. Pane frames widen the doc's u32 pane id
/// to u64 to match `PaneId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Exactly one JSON object (a `Request`, `Response`, or `Event`).
    Control(Vec<u8>),
    PaneSnapshot(PaneData),
    PaneDelta(PaneData),
    Input {
        pane: PaneId,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneData {
    pub pane: PaneId,
    pub rows: u16,
    pub cols: u16,
    pub seq: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooLarge(usize),
    UnknownKind(u8),
    TruncatedPayload,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(len) => write!(f, "frame length {len} exceeds {MAX_FRAME_LEN}"),
            FrameError::UnknownKind(kind) => write!(f, "unknown frame kind {kind:#x}"),
            FrameError::TruncatedPayload => write!(f, "frame payload shorter than its header"),
        }
    }
}

impl std::error::Error for FrameError {}

pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

const KIND_CONTROL: u8 = 0x01;
const KIND_SNAPSHOT: u8 = 0x02;
const KIND_DELTA: u8 = 0x03;
const KIND_INPUT: u8 = 0x04;

const PANE_HEADER: usize = 8 + 2 + 2 + 4;

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload_len) = match self {
            Frame::Control(json) => (KIND_CONTROL, json.len()),
            Frame::PaneSnapshot(d) => (KIND_SNAPSHOT, PANE_HEADER + d.bytes.len()),
            Frame::PaneDelta(d) => (KIND_DELTA, PANE_HEADER + d.bytes.len()),
            Frame::Input { bytes, .. } => (KIND_INPUT, 8 + bytes.len()),
        };
        assert!(
            payload_len < MAX_FRAME_LEN,
            "frame payload {payload_len} exceeds MAX_FRAME_LEN"
        );
        let mut out = Vec::with_capacity(4 + 1 + payload_len);
        out.extend_from_slice(&(1 + payload_len as u32).to_le_bytes());
        out.push(kind);
        match self {
            Frame::Control(json) => out.extend_from_slice(json),
            Frame::PaneSnapshot(d) | Frame::PaneDelta(d) => {
                out.extend_from_slice(&d.pane.0.to_le_bytes());
                out.extend_from_slice(&d.rows.to_le_bytes());
                out.extend_from_slice(&d.cols.to_le_bytes());
                out.extend_from_slice(&d.seq.to_le_bytes());
                out.extend_from_slice(&d.bytes);
            }
            Frame::Input { pane, bytes } => {
                out.extend_from_slice(&pane.0.to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
        out
    }

    /// Decode one frame from the front of `buf`. Returns `Ok(None)` when the
    /// buffer does not yet hold a complete frame; on success also returns the
    /// number of bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>, FrameError> {
        let Some(len_bytes) = buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        if len == 0 {
            return Err(FrameError::TruncatedPayload);
        }
        let Some(body) = buf.get(4..4 + len) else {
            return Ok(None);
        };
        let payload = &body[1..];
        let frame = match body[0] {
            KIND_CONTROL => Frame::Control(payload.to_vec()),
            KIND_SNAPSHOT => Frame::PaneSnapshot(decode_pane_data(payload)?),
            KIND_DELTA => Frame::PaneDelta(decode_pane_data(payload)?),
            KIND_INPUT => {
                if payload.len() < 8 {
                    return Err(FrameError::TruncatedPayload);
                }
                Frame::Input {
                    pane: PaneId(u64::from_le_bytes(payload[..8].try_into().unwrap())),
                    bytes: payload[8..].to_vec(),
                }
            }
            kind => return Err(FrameError::UnknownKind(kind)),
        };
        Ok(Some((frame, 4 + len)))
    }
}

fn decode_pane_data(payload: &[u8]) -> Result<PaneData, FrameError> {
    if payload.len() < PANE_HEADER {
        return Err(FrameError::TruncatedPayload);
    }
    Ok(PaneData {
        pane: PaneId(u64::from_le_bytes(payload[..8].try_into().unwrap())),
        rows: u16::from_le_bytes(payload[8..10].try_into().unwrap()),
        cols: u16::from_le_bytes(payload[10..12].try_into().unwrap()),
        seq: u32::from_le_bytes(payload[12..16].try_into().unwrap()),
        bytes: payload[16..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) {
        let encoded = frame.encode();
        let (decoded, consumed) = Frame::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn roundtrips_all_kinds() {
        roundtrip(Frame::Control(br#"{"type":"pane_list"}"#.to_vec()));
        roundtrip(Frame::PaneSnapshot(PaneData {
            pane: PaneId(7),
            rows: 24,
            cols: 80,
            seq: 0,
            bytes: b"\x1b[2Jhello".to_vec(),
        }));
        roundtrip(Frame::PaneDelta(PaneData {
            pane: PaneId(u64::MAX),
            rows: 50,
            cols: 200,
            seq: 42,
            bytes: vec![],
        }));
        roundtrip(Frame::Input {
            pane: PaneId(3),
            bytes: b"ls -la\r".to_vec(),
        });
    }

    #[test]
    fn incomplete_buffer_returns_none() {
        let encoded = Frame::Control(b"{}".to_vec()).encode();
        for cut in 0..encoded.len() {
            assert_eq!(Frame::decode(&encoded[..cut]).unwrap(), None, "cut {cut}");
        }
    }

    #[test]
    fn decodes_back_to_back_frames() {
        let a = Frame::Control(b"{\"type\":\"ok\"}".to_vec());
        let b = Frame::Input {
            pane: PaneId(1),
            bytes: b"x".to_vec(),
        };
        let mut buf = a.encode();
        buf.extend_from_slice(&b.encode());
        let (first, consumed) = Frame::decode(&buf).unwrap().unwrap();
        assert_eq!(first, a);
        let (second, rest) = Frame::decode(&buf[consumed..]).unwrap().unwrap();
        assert_eq!(second, b);
        assert_eq!(consumed + rest, buf.len());
    }

    #[test]
    fn rejects_unknown_kind_and_truncated_payloads() {
        let mut bad_kind = Frame::Control(b"{}".to_vec()).encode();
        bad_kind[4] = 0x7f;
        assert_eq!(Frame::decode(&bad_kind), Err(FrameError::UnknownKind(0x7f)));

        let truncated = [8u32.to_le_bytes().as_slice(), &[KIND_SNAPSHOT], &[0; 7]].concat();
        assert_eq!(Frame::decode(&truncated), Err(FrameError::TruncatedPayload));

        let huge = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes();
        assert_eq!(
            Frame::decode(&huge),
            Err(FrameError::TooLarge(MAX_FRAME_LEN + 1))
        );
    }
}
