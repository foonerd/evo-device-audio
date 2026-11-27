// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A synthetic AirPlay 2 buffered-audio **sender**, for tests and for the
//! `airplay_sender` example.
//!
//! It speaks the sender side of the protocol this library implements:
//! transient pairing, the encrypted control channel, the `fp-setup` exchange,
//! both `SETUP` phases, the control verbs, and the buffered data channel with
//! per-packet encryption. It is test tooling, not a product surface: it proves
//! the receiver works against a sender built from the same reading of the
//! protocol, which is not the same as proving it works against an Apple
//! device.

use std::io;
use std::net::SocketAddr;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use plist::{Dictionary, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::cipher::{sender_control_channel, Decryptor, Encryptor};
use crate::srp::SrpClient;
use crate::tlv::{ty, Tlv};

const BINARY_PLIST: &str = "Content-Type: application/x-apple-binary-plist\r\n";
const OCTET_STREAM: &str = "Content-Type: application/octet-stream\r\n";

/// A receiver's answer to one request.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The status line, for example `RTSP/1.0 200 OK`.
    pub status: String,
    /// The response body.
    pub body: Vec<u8>,
}

impl Reply {
    /// The numeric status code, or `0` if the status line is unreadable.
    pub fn code(&self) -> u16 {
        self.status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    /// Whether the answer was `200`.
    pub fn is_ok(&self) -> bool {
        self.code() == 200
    }
}

/// The ports a receiver opened for a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPorts {
    /// The TCP port the buffered audio is sent to.
    pub data: u16,
    /// The UDP control port.
    pub control: u16,
}

/// A paired sender holding the encrypted control connection.
pub struct Sender {
    stream: TcpStream,
    enc: Encryptor,
    dec: Decryptor,
    cseq: u32,
    addr: SocketAddr,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_head(head: &[u8]) -> (String, usize) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or_default().to_string();
    let length = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("Content-Length")
                .then(|| v.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    (status, length)
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed")
}

async fn plain_request(
    stream: &mut TcpStream,
    line: &str,
    body: &[u8],
) -> io::Result<Reply> {
    let mut request =
        format!("{line}\r\nContent-Length: {}\r\n\r\n", body.len())
            .into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request).await?;
    let mut buf = Vec::new();
    loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let (status, length) = parse_head(&buf[..pos]);
            if buf.len() >= pos + 4 + length {
                return Ok(Reply {
                    status,
                    body: buf[pos + 4..pos + 4 + length].to_vec(),
                });
            }
        }
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(eof());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn plist_bytes(dict: Dictionary) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    Value::Dictionary(dict)
        .to_writer_binary(&mut out)
        .map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, e.to_string())
        })?;
    Ok(out)
}

fn dmap(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
    let mut out = tag.to_vec();
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
}

impl Sender {
    /// Connect to `addr` and complete transient pairing with `password`
    /// (`3939` when the receiver has none). A refused password is an error
    /// of kind `PermissionDenied`.
    pub async fn connect(
        addr: SocketAddr,
        password: &str,
    ) -> io::Result<Sender> {
        let mut stream = TcpStream::connect(addr).await?;
        let mut m1 = Tlv::new();
        m1.put_u8(ty::STATE, 1)
            .put_u8(ty::METHOD, 0)
            .put_u8(ty::FLAGS, 0x10);
        let m2 = plain_request(
            &mut stream,
            "POST /pair-setup RTSP/1.0\r\nCSeq: 0",
            &m1.encode(),
        )
        .await?;
        let m2 = Tlv::decode(&m2.body)
            .ok_or_else(|| io::Error::other("unreadable pair-setup M2"))?;
        let (Some(salt), Some(server_key)) =
            (m2.get(ty::SALT), m2.get(ty::PUBLIC_KEY))
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "pairing refused (error {:?}, retry in {:?} s)",
                    m2.get_u8(ty::ERROR),
                    m2.get(ty::RETRY_DELAY)
                        .filter(|d| d.len() == 2)
                        .map(|d| u16::from_le_bytes([d[0], d[1]]))
                ),
            ));
        };
        let mut client = SrpClient::new(password);
        let proof = client.process(salt, server_key);
        let mut m3 = Tlv::new();
        m3.put_u8(ty::STATE, 3)
            .put(ty::PUBLIC_KEY, client.public_a())
            .put(ty::PROOF, proof.to_vec());
        let m4 = plain_request(
            &mut stream,
            "POST /pair-setup RTSP/1.0\r\nCSeq: 1",
            &m3.encode(),
        )
        .await?;
        let m4 = Tlv::decode(&m4.body)
            .ok_or_else(|| io::Error::other("unreadable pair-setup M4"))?;
        let verified = m4.get_u8(ty::STATE) == Some(4)
            && m4
                .get(ty::PROOF)
                .is_some_and(|p| client.verify_hamk(p, &proof));
        if !verified {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("pairing refused (error {:?})", m4.get_u8(ty::ERROR)),
            ));
        }
        let secret = client
            .session_key()
            .ok_or_else(|| io::Error::other("no pairing session key"))?;
        let (enc, dec) = sender_control_channel(secret);
        Ok(Sender {
            stream,
            enc,
            dec,
            cseq: 1,
            addr,
        })
    }

    /// Send one request on the encrypted channel and read its answer.
    pub async fn request(
        &mut self,
        start: &str,
        extra: &str,
        body: &[u8],
    ) -> io::Result<Reply> {
        self.cseq += 1;
        let mut request = format!(
            "{start}\r\nCSeq: {}\r\n{extra}Content-Length: {}\r\n\r\n",
            self.cseq,
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        self.stream.write_all(&self.enc.encrypt(&request)).await?;
        let mut cipher = Vec::new();
        let mut plain = Vec::new();
        loop {
            if let Some(pos) = find(&plain, b"\r\n\r\n") {
                let (status, length) = parse_head(&plain[..pos]);
                if plain.len() >= pos + 4 + length {
                    return Ok(Reply {
                        status,
                        body: plain[pos + 4..pos + 4 + length].to_vec(),
                    });
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(eof());
            }
            cipher.extend_from_slice(&chunk[..n]);
            let (decrypted, used) =
                self.dec.decrypt_available(&cipher).ok_or_else(|| {
                    io::Error::other("control channel failed to authenticate")
                })?;
            cipher.drain(0..used);
            plain.extend_from_slice(&decrypted);
        }
    }

    /// `GET /info`.
    pub async fn info(&mut self) -> io::Result<Reply> {
        self.request("GET /info HTTP/1.1", "", b"").await
    }

    /// The two-phase `fp-setup` exchange.
    pub async fn fp_setup(&mut self) -> io::Result<()> {
        let phase1: [u8; 16] = [
            0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x02, 0x00, 0x01, 0xbb,
        ];
        let reply = self
            .request("POST /fp-setup HTTP/1.1", OCTET_STREAM, &phase1)
            .await?;
        if !reply.is_ok() {
            return Err(io::Error::other(format!(
                "fp-setup 1: {}",
                reply.status
            )));
        }
        let mut phase2 = vec![0u8; 164];
        phase2[0..4].copy_from_slice(b"FPLY");
        phase2[4] = 3;
        phase2[5] = 1;
        phase2[6] = 3;
        let reply = self
            .request("POST /fp-setup HTTP/1.1", OCTET_STREAM, &phase2)
            .await?;
        if reply.is_ok() {
            Ok(())
        } else {
            Err(io::Error::other(format!("fp-setup 2: {}", reply.status)))
        }
    }

    /// `SETUP` phase 1 (the event and timing channels).
    pub async fn setup_event(&mut self) -> io::Result<Reply> {
        let mut dict = Dictionary::new();
        dict.insert("timingProtocol".into(), Value::String("PTP".into()));
        let body = plist_bytes(dict)?;
        self.request("SETUP rtsp://x/1 RTSP/1.0", BINARY_PLIST, &body)
            .await
    }

    /// `SETUP` phase 2 for one stream of `stream_type` (103 is buffered
    /// audio) keyed with `key`. The ports are present when the answer is `200`.
    pub async fn setup_stream(
        &mut self,
        stream_type: u64,
        key: &[u8],
    ) -> io::Result<(Reply, Option<StreamPorts>)> {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(stream_type.into()));
        stream
            .insert("audioFormat".into(), Value::Integer(0x40_0000u64.into()));
        stream.insert("spf".into(), Value::Integer(1024u64.into()));
        stream.insert("ct".into(), Value::Integer(4u64.into()));
        stream.insert("shk".into(), Value::Data(key.to_vec()));
        let mut dict = Dictionary::new();
        dict.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let body = plist_bytes(dict)?;
        let reply = self
            .request("SETUP rtsp://x/1 RTSP/1.0", BINARY_PLIST, &body)
            .await?;
        let ports = reply.is_ok().then(|| parse_ports(&reply.body)).flatten();
        Ok((reply, ports))
    }

    /// `RECORD`.
    pub async fn record(&mut self) -> io::Result<Reply> {
        self.request("RECORD rtsp://x/1 RTSP/1.0", "", b"").await
    }

    /// `SET_PARAMETER volume` in AirPlay dB.
    pub async fn set_volume(&mut self, db: f32) -> io::Result<Reply> {
        self.request(
            "SET_PARAMETER rtsp://x/1 RTSP/1.0",
            "Content-Type: text/parameters\r\n",
            format!("volume: {db:.6}\r\n").as_bytes(),
        )
        .await
    }

    /// `SET_PARAMETER` with DMAP track metadata.
    pub async fn set_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
    ) -> io::Result<Reply> {
        let mut item = dmap(b"minm", title.as_bytes());
        item.extend(dmap(b"asar", artist.as_bytes()));
        item.extend(dmap(b"asal", album.as_bytes()));
        self.request(
            "SET_PARAMETER rtsp://x/1 RTSP/1.0",
            "Content-Type: application/x-dmap-tagged\r\n",
            &dmap(b"mlit", &item),
        )
        .await
    }

    /// `SETRATEANCHORTIME`: `1` plays, `0` pauses.
    pub async fn set_rate(
        &mut self,
        rate: u64,
        rtp_time: u64,
    ) -> io::Result<Reply> {
        let mut dict = Dictionary::new();
        dict.insert("rate".into(), Value::Integer(rate.into()));
        dict.insert("rtpTime".into(), Value::Integer(rtp_time.into()));
        let body = plist_bytes(dict)?;
        self.request(
            "SETRATEANCHORTIME rtsp://x/1 RTSP/1.0",
            BINARY_PLIST,
            &body,
        )
        .await
    }

    /// `FLUSHBUFFERED`: discard buffered audio below `until_seq`.
    pub async fn flush_until(&mut self, until_seq: u64) -> io::Result<Reply> {
        let mut dict = Dictionary::new();
        dict.insert("flushUntilSeq".into(), Value::Integer(until_seq.into()));
        dict.insert("flushUntilTS".into(), Value::Integer(0u64.into()));
        let body = plist_bytes(dict)?;
        self.request("FLUSHBUFFERED rtsp://x/1 RTSP/1.0", BINARY_PLIST, &body)
            .await
    }

    /// `TEARDOWN`.
    pub async fn teardown(&mut self) -> io::Result<Reply> {
        self.request("TEARDOWN rtsp://x/1 RTSP/1.0", BINARY_PLIST, b"")
            .await
    }

    /// Open the buffered data channel the receiver announced, keyed with the
    /// `key` given to [`setup_stream`](Self::setup_stream).
    pub async fn open_data(
        &self,
        ports: StreamPorts,
        key: &[u8; 32],
    ) -> io::Result<DataChannel> {
        let stream =
            TcpStream::connect(SocketAddr::new(self.addr.ip(), ports.data))
                .await?;
        Ok(DataChannel {
            stream,
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            seq: 0,
        })
    }

    /// The receiver's address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Whether the receiver has closed the control connection. Reads at most
    /// what is already waiting; an idle open connection answers `false`.
    pub async fn is_closed(&mut self) -> bool {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(
            std::time::Duration::from_millis(50),
            self.stream.read(&mut byte),
        )
        .await
        {
            Ok(Ok(0) | Err(_)) => true,
            Ok(Ok(_)) | Err(_) => false,
        }
    }
}

fn parse_ports(body: &[u8]) -> Option<StreamPorts> {
    let value = Value::from_reader(io::Cursor::new(body)).ok()?;
    let streams = value.as_dictionary()?.get("streams")?.as_array()?;
    let first = streams.first()?.as_dictionary()?;
    let port = |name: &str| {
        first
            .get(name)
            .and_then(Value::as_unsigned_integer)
            .and_then(|p| u16::try_from(p).ok())
    };
    Some(StreamPorts {
        data: port("dataPort")?,
        control: port("controlPort")?,
    })
}

/// The buffered audio data channel of one stream.
pub struct DataChannel {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    seq: u32,
}

impl DataChannel {
    /// Encrypt and send one raw AAC-LC frame as the next packet.
    pub async fn send_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let seq = self.seq;
        self.seq += 1;
        let mut nonce8 = [0u8; 8];
        nonce8.copy_from_slice(&(u64::from(seq) + 1).to_le_bytes());
        let timestamp = 1_000_000u32.wrapping_add(seq.wrapping_mul(1024));
        let mut header = [0u8; 12];
        header[0..4].copy_from_slice(&seq.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&nonce8);
        let sealed = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: frame,
                    aad: &header[4..12],
                },
            )
            .map_err(|_| io::Error::other("audio packet encryption failed"))?;
        let mut packet = header.to_vec();
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&nonce8);
        let mut block = u16::try_from(packet.len() + 2)
            .map_err(|_| io::Error::other("audio packet too large"))?
            .to_be_bytes()
            .to_vec();
        block.extend_from_slice(&packet);
        self.stream.write_all(&block).await
    }

    /// The sequence number the next packet will carry.
    pub fn next_seq(&self) -> u32 {
        self.seq
    }
}

/// Split a file of length-prefixed raw AAC frames (each a big-endian `u32`
/// length then the frame) into its frames; a truncated tail is ignored.
pub fn parse_frames(raw: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at + 4 <= raw.len() {
        let len = u32::from_be_bytes([
            raw[at],
            raw[at + 1],
            raw[at + 2],
            raw[at + 3],
        ]) as usize;
        let Some(frame) = raw.get(at + 4..at + 4 + len) else {
            break;
        };
        frames.push(frame);
        at += 4 + len;
    }
    frames
}

/// The frequency of a tone in one channel of interleaved stereo PCM, from
/// zero crossings, counted from the first audible sample so the encoder's
/// leading silence does not matter.
pub fn channel_hz(pcm: &[i16], channel: usize, rate: u32) -> f64 {
    let samples: Vec<i16> =
        pcm.iter().skip(channel).step_by(2).copied().collect();
    let Some(start) = samples.iter().position(|s| s.unsigned_abs() > 200)
    else {
        return 0.0;
    };
    let tail = &samples[start..];
    let crossings = tail.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
    crossings as f64 / (tail.len() as f64 / f64::from(rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_on_their_length_prefix() {
        let mut raw = Vec::new();
        for frame in [&b"abc"[..], &b"de"[..]] {
            raw.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            raw.extend_from_slice(frame);
        }
        raw.extend_from_slice(&9u32.to_be_bytes());
        raw.extend_from_slice(b"cut");
        assert_eq!(parse_frames(&raw), vec![&b"abc"[..], &b"de"[..]]);
    }

    #[test]
    fn a_tone_is_measured_per_channel() {
        let rate = 44_100u32;
        let mut pcm = Vec::new();
        for n in 0..rate {
            let t = f64::from(n) / f64::from(rate);
            pcm.push(
                ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16,
            );
            pcm.push(
                ((t * 880.0 * std::f64::consts::TAU).sin() * 8000.0) as i16,
            );
        }
        assert!((channel_hz(&pcm, 0, rate) - 440.0).abs() < 2.0);
        assert!((channel_hz(&pcm, 1, rate) - 880.0).abs() < 2.0);
        assert_eq!(channel_hz(&[0; 64], 0, rate), 0.0);
    }

    #[test]
    fn a_reply_reads_its_status_code() {
        let reply = Reply {
            status: "RTSP/1.0 453 Not Enough Bandwidth".into(),
            body: Vec::new(),
        };
        assert_eq!(reply.code(), 453);
        assert!(!reply.is_ok());
    }
}
