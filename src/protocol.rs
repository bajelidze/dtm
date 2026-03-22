use std::io;
use std::os::fd::BorrowedFd;
use nix::unistd;

// Client-to-server message tags.
const TAG_INPUT: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_DETACH: u8 = 3;

// Server-to-client message tags.
const TAG_OUTPUT: u8 = 1;
const TAG_EXIT: u8 = 2;

pub enum ClientMsg {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Detach,
}

pub enum ServerMsg {
    Output(Vec<u8>),
    Exit(i32),
}

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClientMsg::Input(data) => {
                let payload_len = 1 + data.len();
                let mut buf = Vec::with_capacity(4 + payload_len);
                buf.extend_from_slice(&(payload_len as u32).to_be_bytes());
                buf.push(TAG_INPUT);
                buf.extend_from_slice(data);
                buf
            }
            ClientMsg::Resize { rows, cols } => {
                let payload_len: usize = 1 + 4;
                let mut buf = Vec::with_capacity(4 + payload_len);
                buf.extend_from_slice(&(payload_len as u32).to_be_bytes());
                buf.push(TAG_RESIZE);
                buf.extend_from_slice(&rows.to_be_bytes());
                buf.extend_from_slice(&cols.to_be_bytes());
                buf
            }
            ClientMsg::Detach => {
                let mut buf = Vec::with_capacity(5);
                buf.extend_from_slice(&1u32.to_be_bytes());
                buf.push(TAG_DETACH);
                buf
            }
        }
    }

    pub fn decode(frame: &[u8]) -> Option<Self> {
        let tag = *frame.first()?;
        let payload = &frame[1..];
        match tag {
            TAG_INPUT => Some(ClientMsg::Input(payload.to_vec())),
            TAG_RESIZE => {
                if payload.len() < 4 {
                    return None;
                }
                let rows = u16::from_be_bytes([payload[0], payload[1]]);
                let cols = u16::from_be_bytes([payload[2], payload[3]]);
                Some(ClientMsg::Resize { rows, cols })
            }
            TAG_DETACH => Some(ClientMsg::Detach),
            _ => None,
        }
    }
}

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ServerMsg::Output(data) => {
                let payload_len = 1 + data.len();
                let mut buf = Vec::with_capacity(4 + payload_len);
                buf.extend_from_slice(&(payload_len as u32).to_be_bytes());
                buf.push(TAG_OUTPUT);
                buf.extend_from_slice(data);
                buf
            }
            ServerMsg::Exit(code) => {
                let payload_len: usize = 1 + 4;
                let mut buf = Vec::with_capacity(4 + payload_len);
                buf.extend_from_slice(&(payload_len as u32).to_be_bytes());
                buf.push(TAG_EXIT);
                buf.extend_from_slice(&code.to_be_bytes());
                buf
            }
        }
    }

    pub fn decode(frame: &[u8]) -> Option<Self> {
        let tag = *frame.first()?;
        let payload = &frame[1..];
        match tag {
            TAG_OUTPUT => Some(ServerMsg::Output(payload.to_vec())),
            TAG_EXIT => {
                if payload.len() < 4 {
                    return None;
                }
                let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Some(ServerMsg::Exit(code))
            }
            _ => None,
        }
    }
}

/// Accumulates bytes from a stream socket and yields complete frames.
///
/// Frame format: [u32 BE length][payload of `length` bytes]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to extract the next complete frame.
    /// Returns the frame payload (without the length prefix).
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if self.buf.len() < 4 + len {
            return None;
        }
        let frame = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Some(frame)
    }
}

/// Write a fully encoded message (with length prefix) to an fd.
pub fn write_msg(fd: BorrowedFd, encoded: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < encoded.len() {
        match unistd::write(fd, &encoded[offset..]) {
            Ok(n) => offset += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
