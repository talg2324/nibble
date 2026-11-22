//! A websocket client for one job: talking to the Firefox nib started, on this machine's
//! loopback. Text messages only, no TLS, no extensions. Written here instead of taking a crate
//! because the whole of it is this file (D-071).
//!
//! The peer is a process we started on a port it chose, so the handshake's accept key is not
//! checked and the mask (required of clients by the protocol) need not be unpredictable.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bigger than any message Firefox sends us (a full-page screenshot is a few megabytes).
const MAX_MESSAGE: u64 = 512 << 20;

/// The sending half. Clones share the connection.
#[derive(Clone)]
pub struct Sender(Arc<Mutex<TcpStream>>);

/// The receiving half: owned by the thread that reads.
pub struct Receiver {
    stream: TcpStream,
    pong: Sender,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Connect to `ws://127.0.0.1:PORT/PATH`.
pub fn connect(port: u16, path: &str) -> io::Result<(Sender, Receiver)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: bmliIHRhbGtzIHRvIGZveA==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    // The answer's headers, a byte at a time so nothing after them is swallowed.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 << 10 {
            return Err(bad("the handshake answer is too long"));
        }
        stream.read_exact(&mut b)?;
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or("");
    if status.split(' ').nth(1) != Some("101") {
        return Err(bad(&format!("not a websocket: {status}")));
    }
    stream.set_read_timeout(None)?;
    let send = Sender(Arc::new(Mutex::new(stream.try_clone()?)));
    Ok((send.clone(), Receiver { stream, pong: send }))
}

impl Sender {
    fn frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        // Any four bytes will do as the mask (see the top of the file).
        let mask = (payload.len() as u32).wrapping_mul(0x9E37_79B9).wrapping_add(0x5F0F_3C2D).to_be_bytes();
        let mut out = Vec::with_capacity(payload.len() + 14);
        out.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => out.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                out.push(0x80 | 126);
                out.extend((n as u16).to_be_bytes());
            }
            n => {
                out.push(0x80 | 127);
                out.extend((n as u64).to_be_bytes());
            }
        }
        out.extend(mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.0.lock().unwrap().write_all(&out)
    }

    pub fn text(&self, s: &str) -> io::Result<()> {
        self.frame(0x1, s.as_bytes())
    }

    /// Say goodbye and close both halves (the reader's `next` then ends).
    pub fn close(&self) {
        let _ = self.frame(0x8, &[]);
        let _ = self.0.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

impl Receiver {
    /// The next text message; an error when the connection is over.
    pub fn next(&mut self) -> io::Result<String> {
        let mut message = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.stream.read_exact(&mut h)?;
            let (fin, opcode) = (h[0] & 0x80 != 0, h[0] & 0x0F);
            let masked = h[1] & 0x80 != 0;
            let mut len = (h[1] & 0x7F) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                self.stream.read_exact(&mut b)?;
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.stream.read_exact(&mut b)?;
                len = u64::from_be_bytes(b);
            }
            if len + message.len() as u64 > MAX_MESSAGE {
                return Err(bad("a message is too long"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.stream.read_exact(&mut payload)?;
            if masked {
                payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
            }
            match opcode {
                // Text, or more of it.
                0x0 | 0x1 => {
                    message.extend(payload);
                    if fin {
                        return String::from_utf8(message).map_err(|_| bad("a message is not UTF-8"));
                    }
                }
                0x8 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "closed by the browser")),
                0x9 => self.pong.frame(0xA, &payload)?,
                // A pong, or binary we never asked for: nothing to do.
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A server that answers the handshake, then sends the frames it is given and returns what
    /// the client sent it (unmasked).
    fn serve(frames: Vec<u8>, read: usize) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            assert!(String::from_utf8_lossy(&head).contains("Upgrade: websocket"));
            s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n").unwrap();
            s.write_all(&frames).unwrap();
            let mut got = vec![0u8; read];
            s.read_exact(&mut got).unwrap();
            got
        });
        (port, t)
    }

    fn unmask(frame: &[u8]) -> (u8, Vec<u8>) {
        let (len, at) = match frame[1] & 0x7F {
            126 => (u16::from_be_bytes([frame[2], frame[3]]) as usize, 4),
            n => (n as usize, 2),
        };
        let mask = &frame[at..at + 4];
        (frame[0], frame[at + 4..at + 4 + len].iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect())
    }

    #[test]
    fn messages_come_whole_whatever_their_framing() {
        let long = "x".repeat(70_000);
        let mut frames = vec![0x81, 2, b'h', b'i'];
        // One message in two frames, a ping in between.
        frames.extend([0x01, 3, b'a', b'b', b'c']);
        frames.extend([0x89, 1, b'p']);
        frames.extend([0x80, 2, b'd', b'e']);
        // A 16-bit length, then a 64-bit one.
        frames.extend([0x81, 126, 0, 200]);
        frames.extend(vec![b'y'; 200]);
        frames.extend([0x81, 127]);
        frames.extend((long.len() as u64).to_be_bytes());
        frames.extend(long.as_bytes());
        frames.extend([0x88, 0]);
        // The server reads back our pong: 2 header bytes, 4 of mask, 1 of payload.
        let (port, server) = serve(frames, 7);
        let (_tx, mut rx) = connect(port, "/session").unwrap();
        assert_eq!(rx.next().unwrap(), "hi");
        assert_eq!(rx.next().unwrap(), "abcde");
        assert_eq!(rx.next().unwrap(), "y".repeat(200));
        assert_eq!(rx.next().unwrap(), long);
        assert!(rx.next().is_err());
        assert_eq!(unmask(&server.join().unwrap()), (0x8A, b"p".to_vec()));
    }

    #[test]
    fn what_we_send_is_masked_text() {
        let text = "z".repeat(300);
        let (port, server) = serve(Vec::new(), 2 + 2 + 4 + 300);
        let (tx, _rx) = connect(port, "/session").unwrap();
        tx.text(&text).unwrap();
        let got = server.join().unwrap();
        assert_eq!(got[1] & 0x80, 0x80, "a client's frames are masked");
        assert_eq!(unmask(&got), (0x81, text.into_bytes()));
    }

    #[test]
    fn a_plain_http_answer_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut b = [0u8; 256];
            let _ = s.read(&mut b);
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n");
        });
        assert!(connect(port, "/session").is_err());
    }
}
