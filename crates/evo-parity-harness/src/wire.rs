//! Wire client for the steward's admin Unix socket.
//!
//! The steward listens on `/run/evo/evo.sock` and speaks
//! framed JSON: 4-byte big-endian length prefix + JSON body.
//! Every operator op (`request`, `subscribe_subject`, ...) is
//! a JSON object the steward dispatches per its op id.
//!
//! This module is the typed dispatch surface the harness's
//! per-shelf verb helpers build on. It is deliberately
//! minimal — no auto-reconnect, no connection pool — because
//! the harness runs short, deterministic scripts against a
//! steady-state steward.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Hard cap on a single frame's body length. Matches the
/// steward's own `MAX_FRAME_SIZE` so the harness refuses
/// outsized requests at construction time rather than the
/// steward dropping them at parse time.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// A connected wire client. Holds the socket between calls.
pub struct Wire {
    stream: UnixStream,
}

impl Wire {
    /// Connect to the steward's admin Unix socket.
    pub async fn connect(socket_path: &str) -> Result<Self> {
        let stream =
            UnixStream::connect(socket_path).await.with_context(|| {
                format!("connecting to steward socket {socket_path}")
            })?;
        Ok(Self { stream })
    }

    /// Issue a `request` op against a plugin shelf and return
    /// the deserialised response payload (the per-shelf wire
    /// envelope the plugin returned).
    pub async fn request(
        &mut self,
        shelf: &str,
        request_type: &str,
        payload: Value,
    ) -> Result<Value> {
        let payload_bytes = serde_json::to_vec(&payload)
            .context("serialising request payload to JSON")?;
        let payload_b64 = base64_encode(&payload_bytes);
        let op = serde_json::json!({
            "op":            "request",
            "shelf":         shelf,
            "request_type":  request_type,
            "payload_b64":   payload_b64,
        });
        self.write_frame(&op).await?;
        let response = self.read_frame().await?;
        // Steward returns ClientResponse: either `{ "payload_b64":
        // ... }` (success) or `{ "error": { ... } }`. The
        // harness's per-shelf helpers translate the structured
        // ApiError into a typed Result; here we decode the
        // success payload back into a Value.
        if let Some(err) = response.get("error") {
            anyhow::bail!(
                "wire dispatch refused: shelf={shelf} verb={request_type} \
                 error={err}"
            );
        }
        let payload_b64 = response
            .get("payload_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| {
            anyhow::anyhow!("steward response missing payload_b64: {response}")
        })?;
        let bytes = base64_decode(payload_b64)
            .context("decoding response payload_b64")?;
        let v: Value = serde_json::from_slice(&bytes)
            .context("response payload is not valid JSON")?;
        Ok(v)
    }

    /// Issue a `request` op and return the structured error
    /// envelope. Used by negative-path assertions where the
    /// harness EXPECTS the verb to refuse.
    pub async fn request_expect_error(
        &mut self,
        shelf: &str,
        request_type: &str,
        payload: Value,
    ) -> Result<ApiError> {
        let payload_bytes = serde_json::to_vec(&payload)?;
        let payload_b64 = base64_encode(&payload_bytes);
        let op = serde_json::json!({
            "op":           "request",
            "shelf":        shelf,
            "request_type": request_type,
            "payload_b64":  payload_b64,
        });
        self.write_frame(&op).await?;
        let response = self.read_frame().await?;
        if let Some(err) = response.get("error") {
            let parsed: ApiError = serde_json::from_value(err.clone())
                .context("response error envelope did not parse as ApiError")?;
            return Ok(parsed);
        }
        anyhow::bail!(
            "expected error but got success: shelf={shelf} verb={request_type} \
             response={response}"
        );
    }

    async fn write_frame(&mut self, body: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(body)?;
        if bytes.len() > MAX_FRAME_BYTES {
            anyhow::bail!(
                "request frame too large: {} bytes > {MAX_FRAME_BYTES}",
                bytes.len()
            );
        }
        let len = (bytes.len() as u32).to_be_bytes();
        self.stream
            .write_all(&len)
            .await
            .context("writing frame length prefix")?;
        self.stream
            .write_all(&bytes)
            .await
            .context("writing frame body")?;
        self.stream.flush().await.context("flushing frame")?;
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<Value> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("reading frame length prefix")?;
        let n = u32::from_be_bytes(len_buf) as usize;
        if n > MAX_FRAME_BYTES {
            anyhow::bail!(
                "response frame too large: {n} bytes > {MAX_FRAME_BYTES}"
            );
        }
        let mut buf = vec![0u8; n];
        self.stream
            .read_exact(&mut buf)
            .await
            .context("reading frame body")?;
        let v: Value = serde_json::from_slice(&buf)
            .context("response frame is not valid JSON")?;
        Ok(v)
    }
}

/// Structured error envelope the steward emits in
/// `ClientResponse::Error`. Mirror of evo's `ApiError` wire
/// shape; deserialised here so harness assertions can match on
/// `class` / `subclass` rather than string-grep messages.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ApiError {
    pub class: String,
    pub message: String,
    #[serde(default)]
    pub subclass: Option<String>,
}

// ----- minimal base64 codec -----
//
// The harness embeds these helpers rather than pulling the
// `base64` crate as a workspace dep, since the wire protocol
// uses base64 only on the request/response payload field and
// nothing else.

const B64_ALPHA: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64_ALPHA[(b0 >> 2) as usize] as char);
        out.push(B64_ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(
                B64_ALPHA[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char,
            );
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHA[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let trimmed = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in trimmed.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => anyhow::bail!("invalid base64 character: {c:#x}"),
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_short_inputs() {
        for input in &[
            b"".to_vec(),
            b"f".to_vec(),
            b"fo".to_vec(),
            b"foo".to_vec(),
            b"foob".to_vec(),
            b"fooba".to_vec(),
            b"foobar".to_vec(),
        ] {
            let encoded = base64_encode(input);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(&decoded, input);
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 §10 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_decode_refuses_invalid_chars() {
        let err = base64_decode("Zm9vYmFy!!").unwrap_err();
        assert!(err.to_string().contains("invalid base64 character"));
    }
}
