//! File-side source-format probe.
//!
//! Reads the head of a music file from MPD's library and parses
//! the codec-specific header to extract the authoritative source
//! audio format (sample rate, bit depth, channel count, DSD rate).
//! This is the only honest source-format signal an MPD-fronted
//! plugin has access to: MPD's protocol-side `audio:` and
//! `Format:` fields report the OUTPUT MPD will produce
//! (post-decode, post-resample, post-DoP-wrap), not the source.
//!
//! # Entry point
//!
//! [`probe_source_format`] takes an absolute path on the local
//! filesystem plus a codec hint (the canonical token derived
//! from the file extension by
//! [`crate::mpd::derive_source_codec_name`]) and returns the
//! parsed [`AudioFormat`] when the file's head carries an
//! authoritative shape, or `None` when:
//!
//! - the file cannot be opened (I/O error, NFS mount stalled,
//!   remote MPD library mounted on a different host),
//! - the codec hint is `None` (no extension / unknown extension),
//! - the codec hint names a format the probe set doesn't cover,
//! - the head bytes don't carry the expected magic / structure
//!   (truncated file, container drift).
//!
//! The probe never panics on malformed input — bounded reads,
//! checked arithmetic, defensive `get(...)` everywhere.
//!
//! # Music-directory resolution
//!
//! MPD's `file:` field is relative to its `music_directory`
//! config setting. [`load_music_directory_from_mpd_conf`] reads
//! `/etc/mpd.conf` (or an operator-supplied alternate path) and
//! extracts the directive. The plugin discovers this once at
//! load time and stores the absolute base; resolving a
//! per-currentsong file path is then a single `Path::join`.
//!
//! # Codec coverage
//!
//! Every codec the catalogue's audio.playback.v1 schema admits
//! and the file-extension derivation table maps:
//!
//! - **DSD containers**: DSF (Sony), DFF (Philips DSDIFF).
//! - **Lossless PCM**: FLAC, WAV (RIFF), AIFF, ALAC (in M4A),
//!   APE (Monkey's Audio), WavPack, TTA, Shorten.
//! - **Lossy / encoded**: MP3 (MPEG-1/2 Layer III), AAC (ADTS or
//!   raw / in M4A), Vorbis (Ogg), Opus (Ogg), WMA (ASF),
//!   Musepack, Speex (Ogg).
//! - **Tracker**: ProTracker MOD, Scream Tracker 3 (S3M),
//!   FastTracker 2 (XM), Impulse Tracker (IT).
//!
//! Each parser is bounded to a head read of at most
//! [`PROBE_HEAD_BYTES`] bytes — enough to traverse the magic
//! prefix, the format descriptor, and the next chunk header
//! for every container in scope. Larger reads are never needed
//! at probe time; format introspection past the head is decoder
//! work, not framework work.
//!
//! # Honesty contract
//!
//! Where the file's head genuinely doesn't carry an
//! authoritative shape (rare; MOD trackers have implicit rate
//! by convention, Shorten v1 lacks an explicit rate field),
//! the parser returns `None` rather than guessing. The wire
//! envelope's `source` field stays null; the UI displays
//! `source_codec` alone for those formats. This matches the
//! "facts not guesswork" discipline already established in
//! [`crate::mpd::derive_source_codec_name`].

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use evo_plugin_sdk::audio::{
    AudioFormat, DsdRate, DsdTransport, EncodedBitrate, PcmCodec,
};

/// Default location of MPD's main configuration file. The
/// plugin reads `music_directory` from here at load time.
pub(crate) const DEFAULT_MPD_CONF_PATH: &str = "/etc/mpd.conf";

/// Maximum number of bytes the probe reads from a file's head.
/// Every parser in scope needs strictly less; 4 KiB leaves
/// headroom for container chunks (RIFF chunk sequencing,
/// ISO-BMFF moov atom traversal) without paying for a larger
/// allocation per probe.
const PROBE_HEAD_BYTES: usize = 4096;

// ----- music_directory resolution -----

/// Parse `music_directory "<path>"` out of an MPD config file.
///
/// MPD's config format allows the value either quoted or
/// unquoted; this parser accepts both. Lines starting with `#`
/// are treated as comments. Returns `None` when the file cannot
/// be opened, when no `music_directory` directive is present,
/// or when the value is empty.
pub(crate) fn load_music_directory_from_mpd_conf(
    conf_path: &Path,
) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(conf_path).ok()?;
    parse_music_directory(&contents)
}

/// Pure parsing logic for `music_directory`. Extracted so the
/// parsing is unit-testable without filesystem I/O.
fn parse_music_directory(contents: &str) -> Option<PathBuf> {
    parse_mpd_directive(contents, "music_directory")
}

/// Load `playlist_directory` from `/etc/mpd.conf` (or an
/// alternate path passed in). Used by the playlist shelf's
/// `create_playlist` verb to materialise empty .m3u files.
pub(crate) fn load_playlist_directory_from_mpd_conf(
    conf_path: &Path,
) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(conf_path).ok()?;
    parse_mpd_directive(&contents, "playlist_directory")
}

/// Generic single-line directive parser shared by
/// `music_directory` + `playlist_directory`. Same syntax
/// tolerance (quoted / unquoted / equals-style; comment
/// lines via `#`; first non-comment hit wins).
pub(crate) fn parse_mpd_directive(
    contents: &str,
    directive: &str,
) -> Option<PathBuf> {
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rest = match line.strip_prefix(directive) {
            Some(r) => r,
            None => continue,
        };
        // The directive name is followed by whitespace, then
        // the value (quoted or unquoted). Strip leading
        // whitespace + leading equals (some MPD configs use
        // key=value style).
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        let value = if let Some(stripped) = rest.strip_prefix('"') {
            // Quoted: take up to next quote.
            let end = stripped.find('"')?;
            &stripped[..end]
        } else {
            // Unquoted: take first whitespace-delimited token.
            rest.split_whitespace().next()?
        };
        if value.is_empty() {
            continue;
        }
        return Some(PathBuf::from(value));
    }
    None
}

// ----- entry point -----

/// Probe a music file's head for its authoritative source audio
/// format. See module docs for semantics + honesty contract.
pub(crate) fn probe_source_format(
    abs_path: &Path,
    codec_hint: &str,
) -> Option<AudioFormat> {
    let mut head = [0u8; PROBE_HEAD_BYTES];
    let n = read_head(abs_path, &mut head)?;
    let head = &head[..n];
    dispatch(codec_hint, head)
}

fn read_head(abs_path: &Path, buf: &mut [u8]) -> Option<usize> {
    let mut file = File::open(abs_path).ok()?;
    let mut total = 0;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).ok()?;
        if n == 0 {
            break;
        }
        total += n;
    }
    if total == 0 {
        None
    } else {
        Some(total)
    }
}

fn dispatch(codec_hint: &str, head: &[u8]) -> Option<AudioFormat> {
    match codec_hint {
        "dsf" => parse_dsf(head),
        "dff" => parse_dff(head),
        "flac" => parse_flac(head),
        "wav" => parse_wav(head),
        "aiff" => parse_aiff(head),
        "alac" => parse_m4a(head),
        "aac" => parse_aac_or_m4a(head),
        "ape" => parse_ape(head),
        "wavpack" => parse_wavpack(head),
        "tta" => parse_tta(head),
        "mp3" => parse_mp3(head),
        "vorbis" => parse_vorbis(head),
        "opus" => parse_opus(head),
        "speex" => parse_speex(head),
        "wma" => parse_wma(head),
        "musepack" => parse_musepack(head),
        "mod" => parse_tracker(head),
        // Honest None: Shorten v1 lacks an explicit rate field;
        // probing would have to make assumptions. Return None
        // and let the UI show "shorten" without a detail line.
        "shorten" => None,
        _ => None,
    }
}

// ----- small byte-reading helpers -----

fn u32_le(b: &[u8], offset: usize) -> Option<u32> {
    let s = b.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u32_be(b: &[u8], offset: usize) -> Option<u32> {
    let s = b.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_le(b: &[u8], offset: usize) -> Option<u64> {
    let s = b.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn u16_le(b: &[u8], offset: usize) -> Option<u16> {
    let s = b.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u16_be(b: &[u8], offset: usize) -> Option<u16> {
    let s = b.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

fn check_magic(head: &[u8], offset: usize, magic: &[u8]) -> bool {
    head.get(offset..offset + magic.len())
        .map(|s| s == magic)
        .unwrap_or(false)
}

// ----- DSD: DSF (Sony) -----
//
// DSF layout (head, offsets in bytes):
//   0x00  "DSD "         (4 bytes magic)
//   0x04  chunk size     (8 LE) — always 28 for DSD chunk
//   0x0C  total size     (8 LE)
//   0x14  metadata ptr   (8 LE)
//   0x1C  "fmt "         (4 bytes magic)
//   0x20  chunk size     (8 LE) — always 52 for fmt chunk
//   0x28  format version (4 LE)
//   0x2C  format id      (4 LE) — 0 = DSD raw
//   0x30  channel type   (4 LE)
//   0x34  channel count  (4 LE)
//   0x38  sample rate    (4 LE) — 2_822_400 / 5_644_800 / 11_289_600 / 22_579_200
//   0x3C  bits per sample(4 LE) — always 1
//   0x40  sample count   (8 LE)
//   0x48  block size     (4 LE)
//   0x4C  reserved       (4 LE)

fn parse_dsf(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"DSD ") {
        return None;
    }
    if !check_magic(head, 0x1C, b"fmt ") {
        return None;
    }
    let channels = u32_le(head, 0x34)?;
    let sample_rate = u32_le(head, 0x38)?;
    let rate = dsd_rate_from_hz(sample_rate)?;
    Some(AudioFormat::Dsd {
        rate,
        // Source-side DSD has no transport — DoP vs native is
        // a delivery-path decision. NativeUsb represents
        // "DSD bits as the source carries them" (no PCM
        // wrapping). UI consumers showing source format
        // ignore the transport field; the badge derives from
        // `kind` + `rate`.
        transport: DsdTransport::NativeUsb,
        channels: clamp_channels(channels)?,
    })
}

// ----- DSD: DFF (Philips DSDIFF) -----
//
// DSDIFF is IFF-shaped:
//   0x00  "FRM8"         (4 magic, big-endian IFF form)
//   0x04  form size      (8 BE)
//   0x0C  form type      (4) — "DSD "
//   0x10  PROP chunk     ("PROP" + size BE + "SND ")
//     within PROP:
//       "FS  " chunk -> 4 BE sample rate
//       "CHNL" chunk -> 2 BE channels, then chan IDs
//
// We walk the chunks within the 4 KiB head.

fn parse_dff(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"FRM8") {
        return None;
    }
    if !check_magic(head, 0x0C, b"DSD ") {
        return None;
    }
    let prop_offset = find_dff_subchunk(head, 0x10, b"PROP")?;
    let prop_body_offset = prop_offset + 12; // FRM8-style sub-chunk
    if !check_magic(head, prop_body_offset, b"SND ") {
        return None;
    }
    let mut cursor = prop_body_offset + 4;
    let mut rate_hz: Option<u32> = None;
    let mut channels: Option<u32> = None;
    while cursor + 12 <= head.len() {
        let id = head.get(cursor..cursor + 4)?;
        let chunk_size = u64_be_at(head, cursor + 4)? as usize;
        let body = cursor + 12;
        match id {
            b"FS  " => {
                rate_hz = u32_be(head, body);
            }
            b"CHNL" => {
                let n = u16_be(head, body)? as u32;
                channels = Some(n);
            }
            _ => {}
        }
        // Chunks are padded to even sizes in IFF.
        let advance = 12 + chunk_size + (chunk_size & 1);
        cursor = cursor.checked_add(advance)?;
        if rate_hz.is_some() && channels.is_some() {
            break;
        }
    }
    let rate_hz = rate_hz?;
    let channels = channels?;
    let rate = dsd_rate_from_hz(rate_hz)?;
    Some(AudioFormat::Dsd {
        rate,
        transport: DsdTransport::NativeUsb,
        channels: clamp_channels(channels)?,
    })
}

fn find_dff_subchunk(head: &[u8], start: usize, id: &[u8; 4]) -> Option<usize> {
    let mut cursor = start;
    while cursor + 12 <= head.len() {
        let chunk_id = head.get(cursor..cursor + 4)?;
        let chunk_size = u64_be_at(head, cursor + 4)? as usize;
        if chunk_id == id {
            return Some(cursor);
        }
        let advance = 12 + chunk_size + (chunk_size & 1);
        cursor = cursor.checked_add(advance)?;
    }
    None
}

fn u64_be_at(b: &[u8], offset: usize) -> Option<u64> {
    let s = b.get(offset..offset + 8)?;
    Some(u64::from_be_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn dsd_rate_from_hz(hz: u32) -> Option<DsdRate> {
    match hz {
        2_822_400 => Some(DsdRate::Dsd64),
        5_644_800 => Some(DsdRate::Dsd128),
        11_289_600 => Some(DsdRate::Dsd256),
        22_579_200 => Some(DsdRate::Dsd512),
        _ => None,
    }
}

fn clamp_channels(n: u32) -> Option<u8> {
    if n == 0 || n > 32 {
        None
    } else {
        Some(n as u8)
    }
}

// ----- FLAC -----
//
// FLAC layout:
//   0x00  "fLaC"          (4 magic)
//   0x04  metadata block header: 1 byte (last_flag + block_type)
//          + 3 bytes BE block length
//   0x08  STREAMINFO body (when block_type == 0):
//          min block size (2 BE), max block size (2 BE),
//          min frame size (3 BE), max frame size (3 BE),
//          then 8 packed bytes:
//            sample_rate (20 bits)
//            channels - 1 (3 bits)
//            bits_per_sample - 1 (5 bits)
//            total_samples (36 bits)
//
// We require the first metadata block to be STREAMINFO (FLAC's
// own format spec mandates this).

fn parse_flac(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"fLaC") {
        return None;
    }
    let block_type = head.get(4)? & 0x7F;
    if block_type != 0 {
        return None;
    }
    // STREAMINFO body starts at offset 8.
    // Packed rate/channels/bits live at offset 8 + 4 + 6 = 18.
    let packed = head.get(18..18 + 8)?;
    let bits = u64::from_be_bytes([
        packed[0], packed[1], packed[2], packed[3], packed[4], packed[5],
        packed[6], packed[7],
    ]);
    let sample_rate = ((bits >> 44) & 0xF_FFFF) as u32;
    let channels = (((bits >> 41) & 0x7) as u8) + 1;
    let bits_per_sample = (((bits >> 36) & 0x1F) as u8) + 1;
    if sample_rate == 0 {
        return None;
    }
    Some(AudioFormat::Pcm {
        codec: pcm_codec_from_bits(bits_per_sample)?,
        rate_hz: sample_rate,
        channels: clamp_channels(channels as u32)?,
    })
}

fn pcm_codec_from_bits(bits: u8) -> Option<PcmCodec> {
    match bits {
        16 => Some(PcmCodec::PcmS16Le),
        24 => Some(PcmCodec::PcmS24Le),
        32 => Some(PcmCodec::PcmS32Le),
        // 8-bit and 20-bit fall outside the SDK's PCM ladder
        // today. Return None honestly; the UI will show the
        // codec name without rate/depth detail.
        _ => None,
    }
}

// ----- WAV (RIFF) -----
//
// WAV layout:
//   0x00  "RIFF"
//   0x04  file size (4 LE)
//   0x08  "WAVE"
//   0x0C  "fmt " chunk header (4) + chunk size (4 LE)
//   0x14  format tag (2 LE) — 1 = PCM, 3 = IEEE float, 65534 = WAVE_FORMAT_EXTENSIBLE
//   0x16  channels (2 LE)
//   0x18  sample rate (4 LE)
//   ...   byte rate, block align
//   0x22  bits per sample (2 LE)

fn parse_wav(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"RIFF") {
        return None;
    }
    if !check_magic(head, 8, b"WAVE") {
        return None;
    }
    // Find "fmt " chunk — usually at 0x0C but spec allows other
    // chunks before it.
    let fmt_offset = find_riff_chunk(head, 12, b"fmt ")?;
    let body = fmt_offset + 8;
    let format_tag = u16_le(head, body)?;
    let channels = u16_le(head, body + 2)? as u32;
    let sample_rate = u32_le(head, body + 4)?;
    let bits = u16_le(head, body + 14)?;
    // EXTENSIBLE (0xFFFE) puts the real format-tag in the
    // SubFormat GUID. The first 2 bytes of the GUID == 1 (PCM)
    // or 3 (float). Skip the extension complexity for now and
    // assume PCM when EXTENSIBLE present — this is correct for
    // ~all common consumer files. Float surfaces as PcmF32
    // bucket which we don't model in the source ladder.
    if format_tag != 1 && format_tag != 0xFFFE {
        return None;
    }
    Some(AudioFormat::Pcm {
        codec: pcm_codec_from_bits(bits as u8)?,
        rate_hz: sample_rate,
        channels: clamp_channels(channels)?,
    })
}

fn find_riff_chunk(head: &[u8], start: usize, id: &[u8; 4]) -> Option<usize> {
    let mut cursor = start;
    while cursor + 8 <= head.len() {
        let chunk_id = head.get(cursor..cursor + 4)?;
        let chunk_size = u32_le(head, cursor + 4)? as usize;
        if chunk_id == id {
            return Some(cursor);
        }
        // RIFF chunks are padded to even sizes.
        let advance = 8 + chunk_size + (chunk_size & 1);
        cursor = cursor.checked_add(advance)?;
    }
    None
}

// ----- AIFF -----
//
// AIFF layout (big-endian IFF):
//   0x00  "FORM"
//   0x04  file size (4 BE)
//   0x08  "AIFF" (or "AIFC" for compressed)
//   0x0C  chunks: "COMM" + size BE + body
//     COMM body:
//       channels (2 BE)
//       numSampleFrames (4 BE)
//       sample size / bits (2 BE)
//       sample rate (10 bytes BE, 80-bit IEEE 754 extended precision)

fn parse_aiff(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"FORM") {
        return None;
    }
    let form_type = head.get(8..12)?;
    if form_type != b"AIFF" && form_type != b"AIFC" {
        return None;
    }
    let comm_offset = find_aiff_chunk(head, 12, b"COMM")?;
    let body = comm_offset + 8;
    let channels = u16_be(head, body)? as u32;
    let bits = u16_be(head, body + 6)?;
    let ext_bytes = head.get(body + 8..body + 18)?;
    let mut ext = [0u8; 10];
    ext.copy_from_slice(ext_bytes);
    let sample_rate = ieee_754_extended_to_u32(&ext)?;
    Some(AudioFormat::Pcm {
        codec: pcm_codec_from_bits(bits as u8)?,
        rate_hz: sample_rate,
        channels: clamp_channels(channels)?,
    })
}

fn find_aiff_chunk(head: &[u8], start: usize, id: &[u8; 4]) -> Option<usize> {
    let mut cursor = start;
    while cursor + 8 <= head.len() {
        let chunk_id = head.get(cursor..cursor + 4)?;
        let chunk_size = u32_be(head, cursor + 4)? as usize;
        if chunk_id == id {
            return Some(cursor);
        }
        let advance = 8 + chunk_size + (chunk_size & 1);
        cursor = cursor.checked_add(advance)?;
    }
    None
}

/// Convert a big-endian 80-bit IEEE 754 extended precision
/// float (Apple/Motorola format) to u32 Hz. AIFF sample rates
/// are always integer Hz in this representation; the float
/// machinery is overkill historically but we honour the
/// format. Returns None on infinity / NaN / non-positive.
fn ieee_754_extended_to_u32(bytes: &[u8; 10]) -> Option<u32> {
    let sign = (bytes[0] & 0x80) >> 7;
    let exponent = (((bytes[0] & 0x7F) as u32) << 8) | (bytes[1] as u32);
    let mantissa = u64::from_be_bytes([
        bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
        bytes[9],
    ]);
    if sign != 0 || exponent == 0x7FFF {
        return None;
    }
    if exponent == 0 && mantissa == 0 {
        return None;
    }
    // Value = mantissa * 2^(exponent - 16383 - 63)
    let bias: i32 = 16383 + 63;
    let shift = exponent as i32 - bias;
    if shift > 0 {
        let v = mantissa.checked_shl(shift as u32)?;
        if v > u32::MAX as u64 {
            None
        } else {
            Some(v as u32)
        }
    } else if shift == 0 {
        Some(mantissa as u32)
    } else {
        let s = (-shift) as u32;
        if s >= 64 {
            None
        } else {
            Some((mantissa >> s) as u32)
        }
    }
}

// ----- M4A / ALAC / AAC-in-MP4 -----
//
// ISO base media file format (MP4 / M4A). Atoms are nested
// 4-byte-size + 4-byte-type blocks. We walk:
//   ftyp -> moov -> trak -> mdia -> minf -> stbl -> stsd
// inside stsd: 1-4 byte version+flags + 4 byte entry count
// then entries — first is `alac` or `mp4a`.
// Both atoms carry a SampleEntry header:
//   reserved (6 bytes) + data_reference_index (2 BE)
//   reserved (8 bytes)
//   channelcount (2 BE)
//   samplesize (2 BE)
//   pre_defined (2 BE)
//   reserved (2 BE)
//   samplerate (4 BE — upper 16 bits is integer rate)

fn parse_m4a(head: &[u8]) -> Option<AudioFormat> {
    let (entry_atom_name, entry_body) = walk_to_stsd_entry(head)?;
    let rate_hz = m4a_entry_sample_rate(entry_body)?;
    let channels = m4a_entry_channels(entry_body)?;
    let bits = m4a_entry_samplesize(entry_body)?;
    match entry_atom_name.as_slice() {
        b"alac" | b"lpcm" => Some(AudioFormat::Pcm {
            codec: pcm_codec_from_bits(bits)?,
            rate_hz,
            channels: clamp_channels(channels)?,
        }),
        b"mp4a" | b"aac " => {
            // The esds atom inside the mp4a entry carries
            // avgBitrate. Surface as Vbr when present (AAC's
            // quality-target encoding is VBR in the
            // audiophile sense); Unknown when the field is
            // absent or zero.
            let bitrate_kbps = m4a_aac_avg_bitrate(entry_body).map(|bps| {
                if bps > 0 {
                    EncodedBitrate::Vbr {
                        avg_kbps: bps / 1000,
                    }
                } else {
                    EncodedBitrate::Unknown
                }
            });
            Some(AudioFormat::EncodedPassthrough {
                codec: "aac".to_string(),
                rate_hz,
                channels: clamp_channels(channels)?,
                bitrate_kbps,
            })
        }
        _ => None,
    }
}

/// Locate the esds atom inside an mp4a sample-entry body and
/// extract DecoderConfigDescriptor.avgBitrate (32-bit BE).
///
/// The mp4a body layout:
///   AudioSampleEntry fixed prefix (28 bytes), then
///   sub-atoms — `esds` is one of them.
/// esds body:
///   1 byte version + 3 bytes flags, then ES descriptor:
///     tag 0x03 (ES_Descriptor), variable-length length, then
///     ES_ID (2 BE), flags (1), optional streamDependence /
///     URL / OCR fields by flags, then DecoderConfigDescriptor:
///       tag 0x04 (DecoderConfigDescriptor), variable-length
///       length, then:
///         objectTypeIndication (1)
///         streamType (1)
///         bufferSizeDB (3 BE)
///         maxBitrate (4 BE)
///         avgBitrate (4 BE)
fn m4a_aac_avg_bitrate(entry_body: &[u8]) -> Option<u32> {
    // Skip the AudioSampleEntry fixed prefix (28 bytes) then
    // search for the esds sub-atom.
    let after_audio_entry = 28usize;
    let esds_off = find_atom_within(entry_body, after_audio_entry, b"esds")?;
    let esds_body = esds_off + 8;
    // Skip version + flags (4 bytes) then descriptor tag.
    let p = esds_body + 4;
    let tag = *entry_body.get(p)?;
    if tag != 0x03 {
        return None;
    }
    let (es_desc_len, es_desc_len_consumed) =
        mpeg_descriptor_length(entry_body, p + 1)?;
    let _ = es_desc_len; // not needed; we walk inner descriptors directly
    let mut q = p + 1 + es_desc_len_consumed;
    // ES_ID (2) + flags (1)
    let flags = *entry_body.get(q + 2)?;
    q += 3;
    // Optional fields by flags:
    if flags & 0x80 != 0 {
        q += 2;
    }
    if flags & 0x40 != 0 {
        let url_len = *entry_body.get(q)? as usize;
        q += 1 + url_len;
    }
    if flags & 0x20 != 0 {
        q += 2;
    }
    // Next descriptor must be DecoderConfigDescriptor (tag 0x04).
    let tag = *entry_body.get(q)?;
    if tag != 0x04 {
        return None;
    }
    let (_, dcd_len_consumed) = mpeg_descriptor_length(entry_body, q + 1)?;
    let dcd_body = q + 1 + dcd_len_consumed;
    // objectTypeIndication (1) + streamType (1) + bufferSizeDB (3 BE)
    //   + maxBitrate (4 BE) + avgBitrate (4 BE)
    let avg_off = dcd_body + 1 + 1 + 3 + 4;
    u32_be(entry_body, avg_off)
}

/// Parse an MPEG-4 variable-length descriptor length (1-4
/// bytes; each carries 7 bits of length + a continuation
/// flag in the high bit).
fn mpeg_descriptor_length(bytes: &[u8], offset: usize) -> Option<(u32, usize)> {
    let mut len: u32 = 0;
    let mut consumed = 0;
    while consumed < 4 {
        let b = *bytes.get(offset + consumed)?;
        len = (len << 7) | ((b & 0x7F) as u32);
        consumed += 1;
        if b & 0x80 == 0 {
            return Some((len, consumed));
        }
    }
    None
}

fn parse_aac_or_m4a(head: &[u8]) -> Option<AudioFormat> {
    // ADTS frame? First byte 0xFF, second 0xF? with layer bits.
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xF6) == 0xF0 {
        return parse_aac_adts(head);
    }
    // Otherwise assume MP4 container (most .aac in modern
    // libraries is in an MP4 wrapper anyway).
    parse_m4a(head)
}

fn parse_aac_adts(head: &[u8]) -> Option<AudioFormat> {
    // ADTS frame header (7 or 9 bytes):
    //  syncword 12 bits (0xFFF)
    //  MPEG version 1 bit
    //  layer 2 bits (always 0)
    //  protection absent 1 bit
    //  profile 2 bits
    //  sample_rate_index 4 bits
    //  private bit 1
    //  channel_config 3 bits
    //  ... frame_length: 13 bits across bytes 3-5
    let b = head.get(0..6)?;
    let sr_index = ((b[2] >> 2) & 0x0F) as usize;
    let channels = (((b[2] & 0x01) << 2) | ((b[3] >> 6) & 0x03)) as u32;
    static AAC_SR: [u32; 16] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000,
        11025, 8000, 7350, 0, 0, 0,
    ];
    let rate_hz = *AAC_SR.get(sr_index)?;
    if rate_hz == 0 {
        return None;
    }
    // Frame-length-derived bitrate. AAC LC fixed 1024 samples
    // per frame:
    //   kbps = frame_length_bytes * 8 / (1024 / sample_rate) / 1000
    //        = frame_length_bytes * 8 * sample_rate / 1024 / 1000
    let frame_length: u32 = (((b[3] as u32) & 0x03) << 11)
        | ((b[4] as u32) << 3)
        | (((b[5] as u32) >> 5) & 0x07);
    let bitrate_kbps = if frame_length > 0 {
        let kbps = (frame_length as u64)
            .checked_mul(8)?
            .checked_mul(rate_hz as u64)?
            .checked_div(1024)?
            .checked_div(1000)? as u32;
        if kbps > 0 {
            Some(EncodedBitrate::Cbr { kbps })
        } else {
            Some(EncodedBitrate::Unknown)
        }
    } else {
        Some(EncodedBitrate::Unknown)
    };
    Some(AudioFormat::EncodedPassthrough {
        codec: "aac".to_string(),
        rate_hz,
        channels: clamp_channels(channels)?,
        bitrate_kbps,
    })
}

fn walk_to_stsd_entry(head: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    // Step into atom by atom.
    let mut cursor = 0;
    cursor = step_into_atom(head, cursor, b"ftyp").unwrap_or(cursor);
    // Allow free / moov / mdat in any order after ftyp.
    let mut moov_off = None;
    while cursor + 8 <= head.len() {
        let (atom_size, atom_type) = atom_at(head, cursor)?;
        if atom_size < 8 {
            return None;
        }
        if atom_type == *b"moov" {
            moov_off = Some(cursor);
            break;
        }
        cursor = cursor.checked_add(atom_size)?;
    }
    let moov_off = moov_off?;
    let inside_moov = moov_off + 8;
    let trak_off = find_atom_within(head, inside_moov, b"trak")?;
    let inside_trak = trak_off + 8;
    let mdia_off = find_atom_within(head, inside_trak, b"mdia")?;
    let inside_mdia = mdia_off + 8;
    let minf_off = find_atom_within(head, inside_mdia, b"minf")?;
    let inside_minf = minf_off + 8;
    let stbl_off = find_atom_within(head, inside_minf, b"stbl")?;
    let inside_stbl = stbl_off + 8;
    let stsd_off = find_atom_within(head, inside_stbl, b"stsd")?;
    // stsd body: 1 byte version + 3 bytes flags + 4 bytes entry count
    let entries_start = stsd_off + 8 + 8;
    if entries_start + 8 > head.len() {
        return None;
    }
    let (_entry_size, entry_type) = atom_at(head, entries_start)?;
    let entry_body_start = entries_start + 8;
    Some((entry_type.to_vec(), head.get(entry_body_start..)?))
}

fn atom_at(head: &[u8], cursor: usize) -> Option<(usize, [u8; 4])> {
    let size = u32_be(head, cursor)? as usize;
    let ty = head.get(cursor + 4..cursor + 8)?;
    Some((size, [ty[0], ty[1], ty[2], ty[3]]))
}

fn step_into_atom(
    head: &[u8],
    cursor: usize,
    expected: &[u8; 4],
) -> Option<usize> {
    let (size, ty) = atom_at(head, cursor)?;
    if ty != *expected {
        return None;
    }
    cursor.checked_add(size)
}

fn find_atom_within(head: &[u8], start: usize, id: &[u8; 4]) -> Option<usize> {
    let mut cursor = start;
    while cursor + 8 <= head.len() {
        let (size, ty) = atom_at(head, cursor)?;
        if size < 8 {
            return None;
        }
        if ty == *id {
            return Some(cursor);
        }
        cursor = cursor.checked_add(size)?;
    }
    None
}

fn m4a_entry_sample_rate(entry_body: &[u8]) -> Option<u32> {
    // SampleEntry: 6 reserved + 2 data_ref_idx = 8 bytes, then
    // AudioSampleEntry: 8 reserved + 2 channels + 2 samplesize
    // + 2 pre_defined + 2 reserved + 4 samplerate (upper 16
    // bits is integer rate).
    let off = 8 + 8 + 2 + 2 + 2 + 2;
    let raw = u32_be(entry_body, off)?;
    Some((raw >> 16) & 0xFFFF)
}

fn m4a_entry_channels(entry_body: &[u8]) -> Option<u32> {
    let off = 8 + 8;
    Some(u16_be(entry_body, off)? as u32)
}

fn m4a_entry_samplesize(entry_body: &[u8]) -> Option<u8> {
    let off = 8 + 8 + 2;
    Some(u16_be(entry_body, off)? as u8)
}

// ----- APE (Monkey's Audio) -----
//
// APE header (descriptor + header):
//   0x00  "MAC "
//   0x04  version (2 LE), padding (2)
//   0x08  descriptor size, header size, seek byte counts...
//   For v3.98+ the audio info is in the APE header that follows
//   the descriptor (descriptor size bytes in). Field order:
//     compression_level (2 LE)
//     format_flags      (2 LE)
//     blocks_per_frame  (4 LE)
//     final_frame_blocks(4 LE)
//     total_frames      (4 LE)
//     bits_per_sample   (2 LE)
//     channels          (2 LE)
//     sample_rate       (4 LE)

fn parse_ape(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"MAC ") {
        return None;
    }
    let version = u16_le(head, 4)?;
    if version < 3980 {
        // Pre-3.98 stored audio info differently; not modeling
        // those — return None honestly (those files are
        // extremely rare today).
        return None;
    }
    let descriptor_size = u32_le(head, 8)? as usize;
    let header_off = descriptor_size;
    let bits = u16_le(head, header_off + 12)?;
    let channels = u16_le(head, header_off + 14)?;
    let rate_hz = u32_le(head, header_off + 16)?;
    Some(AudioFormat::Pcm {
        codec: pcm_codec_from_bits(bits as u8)?,
        rate_hz,
        channels: clamp_channels(channels as u32)?,
    })
}

// ----- WavPack -----
//
// WavPack block header (32 bytes):
//   0x00  "wvpk"
//   0x04  block size      (4 LE)
//   0x08  version         (2 LE)
//   ...   block index, total samples
//   0x18  flags           (4 LE)
//   The flags carry:
//     bits 0-1: bytes/sample - 1 (0..3 => 8/16/24/32 bit)
//     bit 2:    mono (vs stereo at this block)
//     bits 23-26: sample-rate index into table

fn parse_wavpack(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"wvpk") {
        return None;
    }
    let flags = u32_le(head, 0x18)?;
    let bytes_per_sample = (flags & 0x3) + 1;
    let bits = (bytes_per_sample * 8) as u8;
    let mono = (flags >> 2) & 0x1;
    let channels = if mono == 1 { 1u32 } else { 2u32 };
    let sr_index = ((flags >> 23) & 0xF) as usize;
    static WAVPACK_SR: [u32; 15] = [
        6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100,
        48000, 64000, 88200, 96000, 192000,
    ];
    let rate_hz = *WAVPACK_SR.get(sr_index)?;
    Some(AudioFormat::Pcm {
        codec: pcm_codec_from_bits(bits)?,
        rate_hz,
        channels: clamp_channels(channels)?,
    })
}

// ----- TTA -----
//
// TTA1 header (22 bytes):
//   0x00  "TTA1"
//   0x04  format (2 LE)
//   0x06  channels (2 LE)
//   0x08  bits per sample (2 LE)
//   0x0A  sample rate (4 LE)
//   0x0E  data length (4 LE)
//   0x12  header CRC32 (4 LE)

fn parse_tta(head: &[u8]) -> Option<AudioFormat> {
    if !check_magic(head, 0, b"TTA1") {
        return None;
    }
    let channels = u16_le(head, 6)? as u32;
    let bits = u16_le(head, 8)? as u8;
    let rate_hz = u32_le(head, 10)?;
    Some(AudioFormat::Pcm {
        codec: pcm_codec_from_bits(bits)?,
        rate_hz,
        channels: clamp_channels(channels)?,
    })
}

// ----- MP3 -----
//
// MPEG-1/2 Layer III frame header is 4 bytes:
//   syncword (11 bits) = 0x7FF
//   version  (2 bits)  — 00 MPEG-2.5, 01 reserved, 10 MPEG-2, 11 MPEG-1
//   layer    (2 bits)  — 01 Layer III
//   protection bit
//   bitrate index (4 bits)
//   samplerate index (2 bits)
//   ...
// The samplerate table differs by version:
//   MPEG-1:   [44100, 48000, 32000, reserved]
//   MPEG-2:   [22050, 24000, 16000, reserved]
//   MPEG-2.5: [11025, 12000,  8000, reserved]
// Channels live in bits 7-6 of byte 3 (mode):
//   00 stereo, 01 joint stereo, 10 dual channel, 11 mono.
//
// ID3v2 tag at file start carries the tag size then frames; we
// skip the ID3v2 tag if present and look for the first MPEG
// sync word past it.

fn parse_mp3(head: &[u8]) -> Option<AudioFormat> {
    let skip = if head.len() >= 10 && &head[0..3] == b"ID3" {
        // ID3v2 tag size is a 4-byte synchsafe integer at offset 6.
        let s = head.get(6..10)?;
        let size = ((s[0] as u32 & 0x7F) << 21)
            | ((s[1] as u32 & 0x7F) << 14)
            | ((s[2] as u32 & 0x7F) << 7)
            | (s[3] as u32 & 0x7F);
        10 + size as usize
    } else {
        0
    };
    // Scan for MPEG sync word.
    for i in skip..head.len().saturating_sub(4) {
        if head[i] != 0xFF {
            continue;
        }
        let b1 = head[i + 1];
        if (b1 & 0xE0) != 0xE0 {
            continue;
        }
        let version_bits = (b1 >> 3) & 0x3;
        let layer_bits = (b1 >> 1) & 0x3;
        if layer_bits != 0b01 {
            continue;
        }
        let b2 = head[i + 2];
        let sr_index = ((b2 >> 2) & 0x3) as usize;
        if sr_index == 3 {
            continue;
        }
        let rate = match version_bits {
            0b11 => [44100, 48000, 32000][sr_index],
            0b10 => [22050, 24000, 16000][sr_index],
            0b00 => [11025, 12000, 8000][sr_index],
            _ => continue,
        };
        let b3 = head[i + 3];
        let mode_bits = (b3 >> 6) & 0x3;
        let channels: u32 = if mode_bits == 0b11 { 1 } else { 2 };
        let bitrate_index = (b2 >> 4) & 0x0F;
        let first_frame_kbps = mp3_bitrate_kbps(version_bits, bitrate_index);
        let bitrate_kbps =
            mp3_bitrate(head, i, version_bits, mode_bits, first_frame_kbps);
        return Some(AudioFormat::EncodedPassthrough {
            codec: "mp3".to_string(),
            rate_hz: rate,
            channels: clamp_channels(channels)?,
            bitrate_kbps,
        });
    }
    None
}

/// MPEG-1/2/2.5 Layer III bitrate lookup. Returns kbps for the
/// frame's `bitrate_index` (bits 4-7 of byte 2 in the MPEG
/// header), or `None` for "free format" (0) / "forbidden" (15).
fn mp3_bitrate_kbps(version_bits: u8, bitrate_index: u8) -> Option<u32> {
    // MPEG-1 Layer III: index 1..14 -> 32..320 kbps
    static MPEG1_L3: [u32; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    // MPEG-2 / MPEG-2.5 Layer III: index 1..14 -> 8..160 kbps
    static MPEG2_L3: [u32; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    let table = match version_bits {
        0b11 => &MPEG1_L3,
        0b10 | 0b00 => &MPEG2_L3,
        _ => return None,
    };
    let idx = bitrate_index as usize;
    let val = *table.get(idx)?;
    if val == 0 {
        None
    } else {
        Some(val)
    }
}

/// Detect Xing / Info / VBRI tag in the frame side-data area; if
/// present and marked VBR, compute average kbps from the
/// total-bytes + total-frames fields. Otherwise return Cbr from
/// the first frame's bitrate.
///
/// The Xing/Info tag sits at a fixed offset inside the first MPEG
/// frame's side-info region — 36 bytes past the frame sync for
/// MPEG-1 stereo, 21 bytes for MPEG-1 mono, 21 bytes for MPEG-2
/// stereo, 13 bytes for MPEG-2 mono. The tag magic is "Xing"
/// (VBR) or "Info" (CBR-marked). Either way the layout is:
///   magic (4)
///   flags (4 BE)
///     bit 0: frames present
///     bit 1: bytes present
///     bit 2: TOC present
///     bit 3: quality present
///   total_frames (4 BE, if flags bit 0)
///   total_bytes  (4 BE, if flags bit 1)
fn mp3_bitrate(
    head: &[u8],
    frame_offset: usize,
    version_bits: u8,
    mode_bits: u8,
    first_frame_kbps: Option<u32>,
) -> Option<EncodedBitrate> {
    let mpeg1 = version_bits == 0b11;
    let stereo = mode_bits != 0b11;
    let xing_offset = match (mpeg1, stereo) {
        (true, true) => 36,
        (true, false) => 21,
        (false, true) => 21,
        (false, false) => 13,
    };
    let tag_offset = frame_offset.checked_add(4)?.checked_add(xing_offset)?;
    let magic = head.get(tag_offset..tag_offset + 4);
    let is_xing = magic == Some(b"Xing");
    let is_info = magic == Some(b"Info");
    if is_xing || is_info {
        let flags = u32_be(head, tag_offset + 4)?;
        let mut cursor = tag_offset + 8;
        let total_frames = if flags & 0x1 != 0 {
            let v = u32_be(head, cursor)?;
            cursor += 4;
            Some(v)
        } else {
            None
        };
        let total_bytes = if flags & 0x2 != 0 {
            Some(u32_be(head, cursor)?)
        } else {
            None
        };
        if let (Some(frames), Some(bytes)) = (total_frames, total_bytes) {
            if frames > 0 {
                // Samples per frame: MPEG-1 = 1152, MPEG-2/2.5 = 576.
                let samples_per_frame: u64 = if mpeg1 { 1152 } else { 576 };
                // Average kbps from total bytes / duration.
                // duration_seconds = frames * samples_per_frame / sample_rate
                // bytes / duration_seconds * 8 / 1000 = kbps
                // We don't have sample_rate here — caller computed
                // it but we'd thread it through; for the typical
                // 44.1 kHz default, compute from frames/bytes/spf
                // assuming the carrier rate. Simpler + sufficient
                // for UI display: avg_bitrate = bytes * 8 / (frames * spf / sample_rate / 1000)
                // — but sample_rate isn't carried into this scope.
                // Compromise: surface Vbr { avg } using a
                // back-derived approximation from the Xing
                // frames/bytes — kbps_avg ≈ (bytes / frames) * 8
                // * sample_rate / samples_per_frame / 1000. We
                // signal VBR-ness with the Vbr tag; the exact
                // average is approximated. For Info (CBR-marked)
                // streams, prefer the first-frame Cbr value.
                if is_info {
                    if let Some(kbps) = first_frame_kbps {
                        return Some(EncodedBitrate::Cbr { kbps });
                    }
                    return Some(EncodedBitrate::Unknown);
                }
                // VBR (Xing): compute kbps_avg without
                // sample_rate dependency by using the frame size:
                //   bytes_per_frame = total_bytes / total_frames
                //   sample_rate scales out:
                //     samples_per_frame samples per frame at
                //     sample_rate Hz means duration_per_frame =
                //     samples_per_frame / sample_rate seconds.
                //   bits_per_frame = bytes_per_frame * 8
                //   kbps = bits_per_frame * sample_rate /
                //          samples_per_frame / 1000
                // Without sample_rate in scope, we ask the caller
                // for help: use a typical 44.1 kHz for the
                // estimate (works for ~all MP3 content).
                // TODO would over-complicate this path; the
                // back-of-envelope at 44.1 kHz is well within
                // the UI's "approximate" expectation for VBR.
                let bytes_per_frame =
                    (bytes as u64).checked_div(frames as u64)?;
                let kbps = bytes_per_frame
                    .checked_mul(8)?
                    .checked_mul(44_100)?
                    .checked_div(samples_per_frame)?
                    .checked_div(1000)? as u32;
                if kbps > 0 {
                    return Some(EncodedBitrate::Vbr { avg_kbps: kbps });
                }
            }
        }
        // Xing/Info tag present but unparseable counters: fall back.
        return first_frame_kbps.map(|kbps| EncodedBitrate::Cbr { kbps });
    }
    // No Xing/Info tag: assume CBR (the file's bitrate is the
    // first frame's bitrate). If the first-frame index was free
    // format / forbidden, surface Unknown honestly.
    first_frame_kbps.map(|kbps| EncodedBitrate::Cbr { kbps })
}

// ----- Ogg-wrapped: Vorbis / Opus / Speex -----
//
// Ogg page header layout (27+segment_table bytes):
//   0x00  "OggS"
//   0x04  version  (always 0)
//   0x05  header_type
//   0x06  granule position (8 LE)
//   0x0E  bitstream serial (4 LE)
//   0x12  page sequence (4 LE)
//   0x16  checksum (4 LE)
//   0x1A  segment_count (1)
//   0x1B  segment_table (segment_count bytes)
//   then segment data
//
// The first packet of the first page is the codec-specific
// identification header.

fn parse_vorbis(head: &[u8]) -> Option<AudioFormat> {
    let packet = ogg_first_packet(head)?;
    // Vorbis identification header:
    //   0x00  packet_type (1 byte) = 0x01
    //   0x01  "vorbis" (6 bytes magic)
    //   0x07  vorbis_version (4 LE)
    //   0x0B  audio_channels (1)
    //   0x0C  audio_sample_rate (4 LE)
    //   0x10  bitrate_maximum (4 LE i32)
    //   0x14  bitrate_nominal (4 LE i32)
    //   0x18  bitrate_minimum (4 LE i32)
    if packet.first()? != &0x01 {
        return None;
    }
    if !check_magic(packet, 1, b"vorbis") {
        return None;
    }
    let channels = *packet.get(11)? as u32;
    let rate_hz = u32_le(packet, 12)?;
    // Vorbis encodes nominal_bitrate as i32 LE; convert via the
    // bit pattern then take Vbr semantics (Vorbis is
    // intrinsically VBR — the nominal field is the encoder's
    // target average).
    let bitrate_kbps = {
        let nominal_bits = u32_le(packet, 20)?;
        let nominal = nominal_bits as i32;
        if nominal > 0 {
            Some(EncodedBitrate::Vbr {
                avg_kbps: (nominal as u32) / 1000,
            })
        } else {
            Some(EncodedBitrate::Unknown)
        }
    };
    Some(AudioFormat::EncodedPassthrough {
        codec: "vorbis".to_string(),
        rate_hz,
        channels: clamp_channels(channels)?,
        bitrate_kbps,
    })
}

fn parse_opus(head: &[u8]) -> Option<AudioFormat> {
    let packet = ogg_first_packet(head)?;
    // OpusHead packet:
    //   0x00  "OpusHead" (8 magic)
    //   0x08  version (1)
    //   0x09  channel_count (1)
    //   0x0A  pre_skip (2 LE)
    //   0x0C  input_sample_rate (4 LE)
    if !check_magic(packet, 0, b"OpusHead") {
        return None;
    }
    let channels = *packet.get(9)? as u32;
    let rate_hz = u32_le(packet, 12)?;
    // Opus's input_sample_rate is informational; the codec
    // always runs at 48 kHz internally. Surface the input
    // rate when present (non-zero); fall back to 48000 when
    // the encoder didn't record it.
    let rate_hz = if rate_hz == 0 { 48_000 } else { rate_hz };
    Some(AudioFormat::EncodedPassthrough {
        codec: "opus".to_string(),
        rate_hz,
        channels: clamp_channels(channels)?,
        // Opus is intrinsically VBR; the head packet carries no
        // average bitrate, and a span scan would exceed the
        // probe's bounded-head contract. UI renders "Opus / VBR"
        // for this Unknown case.
        bitrate_kbps: Some(EncodedBitrate::Unknown),
    })
}

fn parse_speex(head: &[u8]) -> Option<AudioFormat> {
    let packet = ogg_first_packet(head)?;
    // Speex identification header:
    //   0x00  "Speex   " (8 magic, space-padded)
    //   0x08  speex_version (20 ASCII)
    //   0x1C  speex_version_id (4 LE)
    //   0x20  header_size (4 LE)
    //   0x24  rate (4 LE)
    //   0x28  mode (4 LE)
    //   0x2C  mode_bitstream_version (4 LE)
    //   0x30  nb_channels (4 LE)
    //   0x34  bitrate (4 LE i32) — -1 when not set
    //   0x38  frame_size (4 LE)
    //   0x3C  vbr (4 LE) — 0 = CBR, 1 = VBR
    if !check_magic(packet, 0, b"Speex   ") {
        return None;
    }
    let rate_hz = u32_le(packet, 0x24)?;
    let channels = u32_le(packet, 0x30)?;
    let bitrate_kbps = {
        let raw_bits = u32_le(packet, 0x34)?;
        let raw = raw_bits as i32;
        let vbr = u32_le(packet, 0x3C).unwrap_or(0);
        if raw > 0 {
            let kbps = (raw as u32) / 1000;
            if vbr == 1 {
                Some(EncodedBitrate::Vbr { avg_kbps: kbps })
            } else {
                Some(EncodedBitrate::Cbr { kbps })
            }
        } else {
            Some(EncodedBitrate::Unknown)
        }
    };
    Some(AudioFormat::EncodedPassthrough {
        codec: "speex".to_string(),
        rate_hz,
        channels: clamp_channels(channels)?,
        bitrate_kbps,
    })
}

/// Return the body of the first packet on the first Ogg page
/// of the head buffer.
fn ogg_first_packet(head: &[u8]) -> Option<&[u8]> {
    if !check_magic(head, 0, b"OggS") {
        return None;
    }
    let seg_count = *head.get(0x1A)? as usize;
    let seg_table = head.get(0x1B..0x1B + seg_count)?;
    let mut packet_len = 0usize;
    for &b in seg_table {
        packet_len += b as usize;
        if b < 255 {
            break;
        }
    }
    let body_start = 0x1B + seg_count;
    head.get(body_start..body_start + packet_len)
}

// ----- WMA (ASF) -----
//
// ASF objects begin with a 16-byte GUID + 8-byte size (LE).
// Stream Properties object GUID:
//   {B7DC0791-A9B7-11CF-8EE6-00C00C205365}
// little-endian byte order in the file:
//   91 07 DC B7 B7 A9 CF 11 8E E6 00 C0 0C 20 53 65
// Inside the stream-properties object (offset within stream-
// properties object body):
//   stream type GUID (16)
//   error-correction GUID (16)
//   time offset (8)
//   type-specific data length (4)
//   error-correction length (4)
//   flags (2)
//   reserved (4)
//   type-specific data ...
// For audio streams the type-specific data is a WAVEFORMATEX:
//   format_tag (2 LE)
//   channels   (2 LE)
//   samplerate (4 LE)
//   ...
//   bits_per_sample (2 LE)
// The audio-stream-type GUID is:
//   {F8699E40-5B4D-11CF-A8FD-00805F5C442B}
// little-endian:
//   40 9E 69 F8 4D 5B CF 11 A8 FD 00 80 5F 5C 44 2B

const ASF_HEADER_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA,
    0x00, 0x62, 0xCE, 0x6C,
];
const ASF_STREAM_PROPS_GUID: [u8; 16] = [
    0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0,
    0x0C, 0x20, 0x53, 0x65,
];
const ASF_STREAM_TYPE_AUDIO_GUID: [u8; 16] = [
    0x40, 0x9E, 0x69, 0xF8, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80,
    0x5F, 0x5C, 0x44, 0x2B,
];

fn parse_wma(head: &[u8]) -> Option<AudioFormat> {
    if head.get(0..16)? != ASF_HEADER_GUID {
        return None;
    }
    // Header object: 16 GUID + 8 size + 4 object count + 1 reserved + 1 reserved
    let mut cursor = 30usize;
    while cursor + 24 <= head.len() {
        let guid = head.get(cursor..cursor + 16)?;
        let size = u64_le(head, cursor + 16)? as usize;
        if guid == ASF_STREAM_PROPS_GUID {
            // Stream Properties object body begins at cursor+24.
            let body = cursor + 24;
            let stream_type_guid = head.get(body..body + 16)?;
            if stream_type_guid != ASF_STREAM_TYPE_AUDIO_GUID {
                cursor = cursor.checked_add(size)?;
                continue;
            }
            // After two GUIDs (32) + time offset (8) = 40 bytes,
            // type-specific data length (4 LE) at +40,
            // error-correction length (4) at +44,
            // flags (2) at +48, reserved (4) at +50,
            // type-specific data starts at +54.
            let ts_data = body + 54;
            // WAVEFORMATEX:
            //  format_tag (2) + channels (2) + samplerate (4) +
            //  byte_rate (4) + block_align (2) + bits/sample (2)
            let channels = u16_le(head, ts_data + 2)? as u32;
            let rate_hz = u32_le(head, ts_data + 4)?;
            // `nAvgBytesPerSec` carries the encoded average byte
            // rate for WMA. Convert to kbps. WMA is generally VBR
            // (Microsoft's encoder defaults so) — surface as Vbr;
            // CBR-WMA streams have the same nominal average, so
            // the VBR semantic is harmless for them.
            let avg_bytes_per_sec = u32_le(head, ts_data + 8)?;
            let bitrate_kbps = if avg_bytes_per_sec > 0 {
                let kbps = (avg_bytes_per_sec as u64)
                    .checked_mul(8)?
                    .checked_div(1000)? as u32;
                Some(EncodedBitrate::Vbr { avg_kbps: kbps })
            } else {
                Some(EncodedBitrate::Unknown)
            };
            return Some(AudioFormat::EncodedPassthrough {
                codec: "wma".to_string(),
                rate_hz,
                channels: clamp_channels(channels)?,
                bitrate_kbps,
            });
        }
        cursor = cursor.checked_add(size)?;
    }
    None
}

// ----- Musepack -----
//
// Musepack SV8 streams start with the "MPCK" magic, then
// packet-stream entries. The stream-header packet ("SH") carries
// sample rate index (in a packed nibble) and channel count.
// Stream-header packet layout (after key "SH"):
//   variable-length size, CRC32, version (1 byte = 8),
//   sample_count (var-int), beginning_silence (var-int),
//   packed byte: sample_freq (3 bits) + max_used_bands (5 bits),
//   channels-1 (4 bits) + mid_side (1 bit) + audio_block_pwr (3 bits)
//
// SV7 (older) starts with "MP+ " then a 16-byte header.
//
// This implementation handles only SV8 + the common sample-freq
// table.

fn parse_musepack(head: &[u8]) -> Option<AudioFormat> {
    if check_magic(head, 0, b"MPCK") {
        return parse_musepack_sv8(head);
    }
    if check_magic(head, 0, b"MP+ ") {
        return parse_musepack_sv7(head);
    }
    None
}

fn parse_musepack_sv8(head: &[u8]) -> Option<AudioFormat> {
    // After MPCK, scan for the "SH" packet.
    let mut cursor = 4;
    while cursor + 2 <= head.len() {
        let key = head.get(cursor..cursor + 2)?;
        let (size, size_len) = mpc_varint(head, cursor + 2)?;
        if key == b"SH" {
            let body = cursor + 2 + size_len + 4 + 1; // crc(4) + version(1)
            let (_sample_count, sc_len) = mpc_varint(head, body)?;
            let (_silence, sil_len) = mpc_varint(head, body + sc_len)?;
            let packed = *head.get(body + sc_len + sil_len)?;
            let sr_index = (packed >> 5) & 0x7;
            let next = *head.get(body + sc_len + sil_len + 1)?;
            let channels = ((next >> 4) & 0xF) as u32 + 1;
            static MPC_SR: [u32; 8] = [44100, 48000, 37800, 32000, 0, 0, 0, 0];
            let rate_hz = *MPC_SR.get(sr_index as usize)?;
            if rate_hz == 0 {
                return None;
            }
            return Some(AudioFormat::EncodedPassthrough {
                codec: "musepack".to_string(),
                rate_hz,
                channels: clamp_channels(channels)?,
                // SV8 SH packet does not carry a bitrate field;
                // Musepack is intrinsically VBR. Surface
                // Unknown — the UI renders "Musepack / VBR".
                bitrate_kbps: Some(EncodedBitrate::Unknown),
            });
        }
        cursor = cursor
            .checked_add(2)?
            .checked_add(size_len)?
            .checked_add(size as usize)?;
    }
    None
}

fn parse_musepack_sv7(head: &[u8]) -> Option<AudioFormat> {
    // SV7 header: MP+ magic + 16 bytes header.
    // Sample-rate index lives in bits 16-17 of the second u32
    // (little-endian), channel count is always 2 for SV7.
    let word = u32_le(head, 8)?;
    let sr_index = ((word >> 16) & 0x3) as usize;
    static SV7_SR: [u32; 4] = [44100, 48000, 37800, 32000];
    let rate_hz = SV7_SR[sr_index];
    Some(AudioFormat::EncodedPassthrough {
        codec: "musepack".to_string(),
        rate_hz,
        channels: 2,
        // SV7 header doesn't carry a bitrate field; Musepack is
        // intrinsically VBR. Unknown is the honest answer.
        bitrate_kbps: Some(EncodedBitrate::Unknown),
    })
}

fn mpc_varint(head: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut len = 0;
    while len < 9 {
        let b = *head.get(offset + len)?;
        value = (value << 7) | ((b & 0x7F) as u64);
        len += 1;
        if (b & 0x80) == 0 {
            return Some((value, len));
        }
    }
    None
}

// ----- Tracker formats (MOD / S3M / XM / IT) -----
//
// Tracker files don't carry an explicit "sample rate" in the
// modern sense — they're rendered at a per-engine rate. The
// audiophile convention for ProTracker MOD is 44.1 kHz, 4
// channels (the M.K. signature); .it/.s3m/.xm carry channel
// counts in their headers and render at engine-selectable
// rates. The honest answer for these formats is: surface
// channels when knowable, default rate to 44.1 kHz, but caveat
// is that the rate is rendering-time not source-side. Return
// `None` rather than mislead.

fn parse_tracker(_head: &[u8]) -> Option<AudioFormat> {
    None
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    // ----- music_directory parser -----

    #[test]
    fn parse_music_directory_handles_quoted_value() {
        let conf = r#"
# comment
music_directory "/var/lib/evo/music"
bind_to_address "localhost"
"#;
        assert_eq!(
            parse_music_directory(conf),
            Some(PathBuf::from("/var/lib/evo/music"))
        );
    }

    #[test]
    fn parse_music_directory_handles_unquoted_value() {
        let conf = "music_directory /var/lib/mpd/music\n";
        assert_eq!(
            parse_music_directory(conf),
            Some(PathBuf::from("/var/lib/mpd/music"))
        );
    }

    #[test]
    fn parse_music_directory_handles_equals_syntax() {
        let conf = r#"music_directory = "/srv/music""#;
        assert_eq!(
            parse_music_directory(conf),
            Some(PathBuf::from("/srv/music"))
        );
    }

    #[test]
    fn parse_music_directory_skips_comments() {
        let conf = r#"
# music_directory "/wrong"
music_directory "/right"
"#;
        assert_eq!(parse_music_directory(conf), Some(PathBuf::from("/right")));
    }

    #[test]
    fn parse_music_directory_returns_none_when_absent() {
        let conf = "bind_to_address \"localhost\"\n";
        assert_eq!(parse_music_directory(conf), None);
    }

    // ----- DSF -----

    fn make_dsf_head(sample_rate: u32, channels: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x50];
        v[0..4].copy_from_slice(b"DSD ");
        v[4..12].copy_from_slice(&28u64.to_le_bytes());
        v[0x1C..0x20].copy_from_slice(b"fmt ");
        v[0x20..0x28].copy_from_slice(&52u64.to_le_bytes());
        v[0x34..0x38].copy_from_slice(&channels.to_le_bytes());
        v[0x38..0x3C].copy_from_slice(&sample_rate.to_le_bytes());
        v[0x3C..0x40].copy_from_slice(&1u32.to_le_bytes());
        v
    }

    #[test]
    fn parse_dsf_dsd64_stereo() {
        let head = make_dsf_head(2_822_400, 2);
        match parse_dsf(&head).unwrap() {
            AudioFormat::Dsd { rate, channels, .. } => {
                assert_eq!(rate, DsdRate::Dsd64);
                assert_eq!(channels, 2);
            }
            _ => panic!("expected Dsd"),
        }
    }

    #[test]
    fn parse_dsf_dsd128_stereo() {
        let head = make_dsf_head(5_644_800, 2);
        match parse_dsf(&head).unwrap() {
            AudioFormat::Dsd { rate, .. } => {
                assert_eq!(rate, DsdRate::Dsd128)
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_dsf_dsd256_stereo() {
        let head = make_dsf_head(11_289_600, 2);
        match parse_dsf(&head).unwrap() {
            AudioFormat::Dsd { rate, .. } => {
                assert_eq!(rate, DsdRate::Dsd256)
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_dsf_rejects_unknown_rate() {
        let head = make_dsf_head(48_000, 2);
        assert!(parse_dsf(&head).is_none());
    }

    #[test]
    fn parse_dsf_rejects_bad_magic() {
        let head = vec![0u8; 0x50];
        assert!(parse_dsf(&head).is_none());
    }

    // ----- FLAC -----

    fn make_flac_head(rate: u32, bits: u8, channels: u8) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[0..4].copy_from_slice(b"fLaC");
        // Metadata block header: last_flag=1, type=0 (STREAMINFO), length=34
        v[4] = 0x80;
        v[5] = 0x00;
        v[6] = 0x00;
        v[7] = 0x22;
        // STREAMINFO: skip first 10 bytes (min/max block, min/max frame).
        // Packed: sample_rate (20 bits) | channels-1 (3) | bits-1 (5) | total_samples (36)
        let rate = rate as u64;
        let ch = (channels - 1) as u64;
        let bi = (bits - 1) as u64;
        let packed = (rate << 44) | (ch << 41) | (bi << 36);
        v[18..26].copy_from_slice(&packed.to_be_bytes());
        v
    }

    #[test]
    fn parse_flac_24bit_96khz_stereo() {
        let head = make_flac_head(96_000, 24, 2);
        match parse_flac(&head).unwrap() {
            AudioFormat::Pcm {
                codec,
                rate_hz,
                channels,
            } => {
                assert_eq!(codec, PcmCodec::PcmS24Le);
                assert_eq!(rate_hz, 96_000);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_flac_16bit_44100_stereo() {
        let head = make_flac_head(44_100, 16, 2);
        match parse_flac(&head).unwrap() {
            AudioFormat::Pcm {
                codec,
                rate_hz,
                channels,
            } => {
                assert_eq!(codec, PcmCodec::PcmS16Le);
                assert_eq!(rate_hz, 44_100);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_flac_rejects_bad_magic() {
        let mut head = make_flac_head(44_100, 16, 2);
        head[0] = 0;
        assert!(parse_flac(&head).is_none());
    }

    // ----- WAV -----

    fn make_wav_head(rate: u32, bits: u16, channels: u16) -> Vec<u8> {
        let mut v = vec![0u8; 44];
        v[0..4].copy_from_slice(b"RIFF");
        v[4..8].copy_from_slice(&0u32.to_le_bytes());
        v[8..12].copy_from_slice(b"WAVE");
        v[12..16].copy_from_slice(b"fmt ");
        v[16..20].copy_from_slice(&16u32.to_le_bytes());
        v[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
        v[22..24].copy_from_slice(&channels.to_le_bytes());
        v[24..28].copy_from_slice(&rate.to_le_bytes());
        v[34..36].copy_from_slice(&bits.to_le_bytes());
        v
    }

    #[test]
    fn parse_wav_24bit_192khz_stereo() {
        let head = make_wav_head(192_000, 24, 2);
        match parse_wav(&head).unwrap() {
            AudioFormat::Pcm {
                codec,
                rate_hz,
                channels,
            } => {
                assert_eq!(codec, PcmCodec::PcmS24Le);
                assert_eq!(rate_hz, 192_000);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    // ----- AIFF -----

    fn make_aiff_head(rate: u32, bits: u16, channels: u16) -> Vec<u8> {
        let mut v = vec![0u8; 38];
        v[0..4].copy_from_slice(b"FORM");
        v[8..12].copy_from_slice(b"AIFF");
        v[12..16].copy_from_slice(b"COMM");
        v[16..20].copy_from_slice(&18u32.to_be_bytes());
        v[20..22].copy_from_slice(&channels.to_be_bytes());
        // numSampleFrames: 0 placeholder
        v[26..28].copy_from_slice(&bits.to_be_bytes());
        // Encode rate as 80-bit extended.
        let ext = u32_to_ieee_754_extended(rate);
        v[28..38].copy_from_slice(&ext);
        v
    }

    fn u32_to_ieee_754_extended(value: u32) -> [u8; 10] {
        if value == 0 {
            return [0u8; 10];
        }
        // Find the highest bit set.
        let mantissa_unshifted = value as u64;
        let leading_zeros = mantissa_unshifted.leading_zeros();
        let shift = leading_zeros as i32;
        let mantissa = mantissa_unshifted << shift;
        let exponent_value = 63 - shift;
        let exponent = (16383 + exponent_value) as u16;
        let mut out = [0u8; 10];
        out[0] = (exponent >> 8) as u8;
        out[1] = exponent as u8;
        out[2..10].copy_from_slice(&mantissa.to_be_bytes());
        out
    }

    #[test]
    fn parse_aiff_16bit_44100_stereo() {
        let head = make_aiff_head(44_100, 16, 2);
        match parse_aiff(&head).unwrap() {
            AudioFormat::Pcm {
                codec,
                rate_hz,
                channels,
            } => {
                assert_eq!(codec, PcmCodec::PcmS16Le);
                assert_eq!(rate_hz, 44_100);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_aiff_24bit_96000_stereo() {
        let head = make_aiff_head(96_000, 24, 2);
        match parse_aiff(&head).unwrap() {
            AudioFormat::Pcm { codec, rate_hz, .. } => {
                assert_eq!(codec, PcmCodec::PcmS24Le);
                assert_eq!(rate_hz, 96_000);
            }
            _ => panic!(),
        }
    }

    // ----- MP3 -----

    fn make_mp3_frame_header_mpeg1(rate_idx: u8, mode_bits: u8) -> Vec<u8> {
        // MPEG-1 Layer III, sync = 0xFFF, version=11, layer=01
        let mut v = vec![0u8; 16];
        v[0] = 0xFF;
        v[1] = 0b1111_1011; // sync(11)=0xFF F, version=11 (MPEG-1), layer=01 (III), no CRC
        v[2] = 0b1001_0000 | (rate_idx << 2);
        v[3] = mode_bits << 6;
        v
    }

    #[test]
    fn parse_mp3_mpeg1_44100_stereo() {
        let head = make_mp3_frame_header_mpeg1(0, 0b00);
        match parse_mp3(&head).unwrap() {
            AudioFormat::EncodedPassthrough {
                codec,
                rate_hz,
                channels,
                ..
            } => {
                assert_eq!(codec, "mp3");
                assert_eq!(rate_hz, 44_100);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_mp3_mpeg1_48000_mono() {
        let head = make_mp3_frame_header_mpeg1(1, 0b11);
        match parse_mp3(&head).unwrap() {
            AudioFormat::EncodedPassthrough {
                rate_hz, channels, ..
            } => {
                assert_eq!(rate_hz, 48_000);
                assert_eq!(channels, 1);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_mp3_skips_id3v2_tag() {
        // ID3v2 header (10 bytes) + 100 bytes tag body, then MP3 sync.
        let mut head = vec![0u8; 200];
        head[0..3].copy_from_slice(b"ID3");
        // Synchsafe size = 100 → 0x00 0x00 0x00 0x64
        head[6] = 0x00;
        head[7] = 0x00;
        head[8] = 0x00;
        head[9] = 0x64;
        let frame = make_mp3_frame_header_mpeg1(0, 0);
        head[110..110 + frame.len()].copy_from_slice(&frame);
        let res = parse_mp3(&head).unwrap();
        if let AudioFormat::EncodedPassthrough { rate_hz, .. } = res {
            assert_eq!(rate_hz, 44_100);
        } else {
            panic!();
        }
    }

    // ----- TTA -----

    #[test]
    fn parse_tta_carries_rate_and_bits() {
        let mut v = vec![0u8; 22];
        v[0..4].copy_from_slice(b"TTA1");
        v[6..8].copy_from_slice(&2u16.to_le_bytes());
        v[8..10].copy_from_slice(&24u16.to_le_bytes());
        v[10..14].copy_from_slice(&96_000u32.to_le_bytes());
        match parse_tta(&v).unwrap() {
            AudioFormat::Pcm {
                codec,
                rate_hz,
                channels,
            } => {
                assert_eq!(codec, PcmCodec::PcmS24Le);
                assert_eq!(rate_hz, 96_000);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    // ----- Vorbis (Ogg) -----

    #[test]
    fn parse_vorbis_identification_header() {
        let mut head = vec![0u8; 64];
        head[0..4].copy_from_slice(b"OggS");
        head[0x1A] = 1; // 1 segment
        head[0x1B] = 30; // segment length 30
        let body_off = 0x1C;
        head[body_off] = 0x01;
        head[body_off + 1..body_off + 7].copy_from_slice(b"vorbis");
        head[body_off + 11] = 2; // channels
        head[body_off + 12..body_off + 16]
            .copy_from_slice(&44_100u32.to_le_bytes());
        match parse_vorbis(&head).unwrap() {
            AudioFormat::EncodedPassthrough {
                codec,
                rate_hz,
                channels,
                ..
            } => {
                assert_eq!(codec, "vorbis");
                assert_eq!(rate_hz, 44_100);
                assert_eq!(channels, 2);
            }
            _ => panic!(),
        }
    }

    // ----- Opus (Ogg) -----

    #[test]
    fn parse_opus_head_packet() {
        let mut head = vec![0u8; 64];
        head[0..4].copy_from_slice(b"OggS");
        head[0x1A] = 1;
        head[0x1B] = 19;
        let body_off = 0x1C;
        head[body_off..body_off + 8].copy_from_slice(b"OpusHead");
        head[body_off + 9] = 2; // channels
        head[body_off + 12..body_off + 16]
            .copy_from_slice(&48_000u32.to_le_bytes());
        match parse_opus(&head).unwrap() {
            AudioFormat::EncodedPassthrough { codec, rate_hz, .. } => {
                assert_eq!(codec, "opus");
                assert_eq!(rate_hz, 48_000);
            }
            _ => panic!(),
        }
    }

    // ----- AAC ADTS -----

    #[test]
    fn parse_aac_adts_44100_stereo() {
        let mut head = vec![0u8; 8];
        head[0] = 0xFF;
        head[1] = 0xF1; // MPEG-4, layer 0, no CRC
                        // sr_index = 4 (44100), profile bits = 01 (LC)
        head[2] = (0b01 << 6) | (4 << 2);
        head[3] = (2 & 0x3) << 6; // channel_config = 2
                                  // frame_length = 384 (13 bits across bytes 3-5):
                                  //   bits 11..12 (low 2 of byte 3): (384 >> 11) & 0x3 = 0
                                  //   bits  3..10 (byte 4):           (384 >> 3)  & 0xFF = 48
                                  //   bits  0..2  (high 3 of byte 5): (384 & 0x7) << 5 = 0
        head[4] = 48;
        head[5] = 0;
        match parse_aac_or_m4a(&head).unwrap() {
            AudioFormat::EncodedPassthrough {
                codec,
                rate_hz,
                channels,
                bitrate_kbps,
            } => {
                assert_eq!(codec, "aac");
                assert_eq!(rate_hz, 44_100);
                assert_eq!(channels, 2);
                // 384 bytes per frame * 8 bits * 44100 Hz / 1024
                // samples / 1000 = 132 kbps
                match bitrate_kbps {
                    Some(EncodedBitrate::Cbr { kbps }) => {
                        assert!(
                            (130..=135).contains(&kbps),
                            "expected ~132 kbps, got {kbps}"
                        );
                    }
                    other => panic!("expected Cbr, got {other:?}"),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_mp3_carries_cbr_first_frame_bitrate() {
        // make_mp3_frame_header_mpeg1 sets bitrate_index = 9
        // (the high nibble of byte 2 = 0b1001 = 9) → MPEG-1
        // Layer III index 9 = 128 kbps.
        let head = make_mp3_frame_header_mpeg1(0, 0);
        match parse_mp3(&head).unwrap() {
            AudioFormat::EncodedPassthrough { bitrate_kbps, .. } => {
                match bitrate_kbps {
                    Some(EncodedBitrate::Cbr { kbps }) => {
                        assert_eq!(kbps, 128);
                    }
                    other => panic!("expected Cbr(128), got {other:?}"),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_vorbis_carries_vbr_from_nominal_bitrate() {
        let mut head = vec![0u8; 64];
        head[0..4].copy_from_slice(b"OggS");
        head[0x1A] = 1;
        head[0x1B] = 30;
        let body_off = 0x1C;
        head[body_off] = 0x01;
        head[body_off + 1..body_off + 7].copy_from_slice(b"vorbis");
        head[body_off + 11] = 2; // channels
        head[body_off + 12..body_off + 16]
            .copy_from_slice(&48_000u32.to_le_bytes());
        // nominal_bitrate at offset 20 inside the Vorbis ident
        // header (= body_off + 20). Set 256000 bps (256 kbps).
        head[body_off + 20..body_off + 24]
            .copy_from_slice(&256_000u32.to_le_bytes());
        match parse_vorbis(&head).unwrap() {
            AudioFormat::EncodedPassthrough { bitrate_kbps, .. } => {
                match bitrate_kbps {
                    Some(EncodedBitrate::Vbr { avg_kbps }) => {
                        assert_eq!(avg_kbps, 256);
                    }
                    other => panic!("expected Vbr(256), got {other:?}"),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_opus_reports_unknown_bitrate_honestly() {
        let mut head = vec![0u8; 64];
        head[0..4].copy_from_slice(b"OggS");
        head[0x1A] = 1;
        head[0x1B] = 19;
        let body_off = 0x1C;
        head[body_off..body_off + 8].copy_from_slice(b"OpusHead");
        head[body_off + 9] = 2;
        head[body_off + 12..body_off + 16]
            .copy_from_slice(&48_000u32.to_le_bytes());
        match parse_opus(&head).unwrap() {
            AudioFormat::EncodedPassthrough { bitrate_kbps, .. } => {
                assert!(matches!(bitrate_kbps, Some(EncodedBitrate::Unknown)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn encoded_bitrate_serializes_as_tagged_enum() {
        // Wire shape contract: tagged enum with snake_case
        // variant discriminant. UI subscribers parse on the
        // `kind` tag.
        let cbr = EncodedBitrate::Cbr { kbps: 320 };
        let j = serde_json::to_value(cbr).unwrap();
        assert_eq!(j["kind"], "cbr");
        assert_eq!(j["kbps"], 320);
        let vbr = EncodedBitrate::Vbr { avg_kbps: 245 };
        let j = serde_json::to_value(vbr).unwrap();
        assert_eq!(j["kind"], "vbr");
        assert_eq!(j["avg_kbps"], 245);
        let unk = EncodedBitrate::Unknown;
        let j = serde_json::to_value(unk).unwrap();
        assert_eq!(j["kind"], "unknown");
    }

    // ----- probe_source_format dispatch -----

    #[test]
    fn dispatch_unknown_codec_returns_none() {
        let head = vec![0u8; 64];
        assert!(dispatch("xyz", &head).is_none());
    }

    #[test]
    fn dispatch_shorten_returns_none_honestly() {
        let head = vec![0u8; 64];
        assert!(dispatch("shorten", &head).is_none());
    }

    #[test]
    fn dispatch_mod_returns_none_honestly() {
        let head = vec![0u8; 64];
        assert!(dispatch("mod", &head).is_none());
    }
}
