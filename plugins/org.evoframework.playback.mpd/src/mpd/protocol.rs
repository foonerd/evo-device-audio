//! MPD wire protocol: command serialisation and response parsing.
//!
//! This module is transport-agnostic. It operates on byte strings
//! going out and on UTF-8 strings coming in (the framing layer is
//! responsible for turning raw bytes into line-shaped UTF-8). Every
//! function here is pure in its inputs: no I/O, no time, no async.
//! That makes protocol-level behaviour unit-testable against exact
//! byte strings without any network.
//!
//! The MPD protocol this module implements:
//!
//! - Welcome banner on connect: `OK MPD <major>.<minor>.<patch>\n`.
//! - Commands: `<name>[ "<arg>"[ "<arg>"...]]\n`, with argument
//!   quoting per [`encode_argument`].
//! - Responses: a sequence of `<Key>: <value>\n` lines followed by
//!   a terminator, either `OK\n` for success or
//!   `ACK [<code>@<cmd_list_num>] {<command>} <message>\n` for
//!   command-level failure.
//!
//! Command lists, binary responses, and the idle subprotocol are
//! future work (Phases 3.2+); this module's surface is intentionally
//! bounded to what Phase 3.1 requires.

use super::error::ProtocolError;
use super::types::MpdVersion;

/// Maximum length (bytes) of a single protocol line we accept from
/// the server. MPD's own historical default is 65536; matching that
/// keeps our behaviour predictable against real deployments while
/// still bounding memory use per line.
pub(crate) const LINE_MAX: usize = 64 * 1024;

/// Classification of a response line after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClassifiedLine {
    /// The response-ending `OK` terminator.
    Ok,
    /// The response-ending `ACK` terminator, with its parts broken
    /// out per the MPD protocol.
    Ack {
        /// MPD error code.
        code: u32,
        /// Position in a command list, or 0 for single-command
        /// dispatch.
        list_position: u32,
        /// Name of the command that failed.
        command: String,
        /// Human-readable message from MPD.
        message: String,
    },
    /// A key/value field line from the body of the response.
    Field(Field),
}

/// A parsed key/value field from a response line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    /// Field name as emitted by MPD (case preserved; MPD uses mixed
    /// case: `Title`, `Artist`, `volume`, `state`, etc.).
    pub(crate) key: String,
    /// Field value, trimmed of the `: ` separator but otherwise
    /// preserved verbatim (including any internal colons, spaces,
    /// or unicode).
    pub(crate) value: String,
}

/// Serialise a command with its arguments into a single wire frame.
///
/// The command name is validated to reject unprintable characters;
/// every argument is validated to reject `\n`, `\r`, and NUL (none
/// can be represented on the wire). Arguments are uniformly quoted
/// to avoid a "does this one need quoting" dance at the call site.
pub(crate) fn serialise_command(
    command: &str,
    args: &[&str],
) -> Result<Vec<u8>, ProtocolError> {
    for ch in command.chars() {
        if ch == '\n' || ch == '\r' || ch == '\0' {
            return Err(ProtocolError::CommandForbiddenChar { ch });
        }
    }

    let mut out = Vec::with_capacity(
        command.len() + args.iter().map(|a| a.len() + 3).sum::<usize>() + 2,
    );
    out.extend_from_slice(command.as_bytes());

    for arg in args {
        out.push(b' ');
        encode_argument(arg, &mut out)?;
    }
    out.push(b'\n');
    Ok(out)
}

/// Encode a single argument, always quoting with double-quotes and
/// escaping `\` and `"`. Rejects characters that cannot be placed on
/// the wire at all (newline, CR, NUL).
fn encode_argument(arg: &str, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
    for ch in arg.chars() {
        if ch == '\n' || ch == '\r' || ch == '\0' {
            return Err(ProtocolError::CommandForbiddenChar { ch });
        }
    }
    out.push(b'"');
    for ch in arg.chars() {
        match ch {
            '\\' => out.extend_from_slice(b"\\\\"),
            '"' => out.extend_from_slice(b"\\\""),
            c => {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                out.extend_from_slice(encoded.as_bytes());
            }
        }
    }
    out.push(b'"');
    Ok(())
}

/// Parse the welcome banner MPD sends immediately on connect.
///
/// `line` must already have its trailing newline stripped by the
/// framing layer. Expected shape: `OK MPD <major>.<minor>.<patch>`.
pub(crate) fn parse_welcome(line: &str) -> Result<MpdVersion, ProtocolError> {
    let rest = line
        .strip_prefix("OK MPD ")
        .ok_or_else(|| ProtocolError::BadWelcome(line.to_string()))?;
    parse_version_triple(rest)
}

fn parse_version_triple(s: &str) -> Result<MpdVersion, ProtocolError> {
    let trimmed = s.trim();
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() != 3 {
        return Err(ProtocolError::BadVersion(trimmed.to_string()));
    }
    let major = parts[0]
        .parse::<u32>()
        .map_err(|_| ProtocolError::BadVersion(trimmed.to_string()))?;
    let minor = parts[1]
        .parse::<u32>()
        .map_err(|_| ProtocolError::BadVersion(trimmed.to_string()))?;
    let patch = parts[2]
        .parse::<u32>()
        .map_err(|_| ProtocolError::BadVersion(trimmed.to_string()))?;
    Ok(MpdVersion::new(major, minor, patch))
}

/// Classify a single response line.
///
/// `line` must already have its trailing newline stripped. The return
/// value tells the caller whether the response body continues
/// (`Field`) or has ended (`Ok` or `Ack`).
pub(crate) fn classify_line(
    line: &str,
) -> Result<ClassifiedLine, ProtocolError> {
    if line == "OK" {
        return Ok(ClassifiedLine::Ok);
    }
    if line.starts_with("ACK [") {
        return parse_ack_line(line);
    }
    let field = parse_field_line(line)?;
    Ok(ClassifiedLine::Field(field))
}

/// Parse a key/value body line. Called only when the line is known
/// not to be a terminator.
fn parse_field_line(line: &str) -> Result<Field, ProtocolError> {
    let (key, value) = line
        .split_once(": ")
        .ok_or_else(|| ProtocolError::MalformedKeyValue(line.to_string()))?;
    if key.is_empty() {
        return Err(ProtocolError::MalformedKeyValue(line.to_string()));
    }
    Ok(Field {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// Parse an `ACK` terminator line. Called only when the line is known
/// to begin with `"ACK ["`.
fn parse_ack_line(line: &str) -> Result<ClassifiedLine, ProtocolError> {
    let rest = line
        .strip_prefix("ACK [")
        .ok_or_else(|| ProtocolError::MalformedAck(line.to_string()))?;

    let (code_pos, rest) = rest
        .split_once("] ")
        .ok_or_else(|| ProtocolError::MalformedAck(line.to_string()))?;
    let (code_s, pos_s) = code_pos
        .split_once('@')
        .ok_or_else(|| ProtocolError::MalformedAck(line.to_string()))?;
    let code: u32 = code_s
        .parse()
        .map_err(|_| ProtocolError::MalformedAck(line.to_string()))?;
    let list_position: u32 = pos_s
        .parse()
        .map_err(|_| ProtocolError::MalformedAck(line.to_string()))?;

    let rest = rest
        .strip_prefix('{')
        .ok_or_else(|| ProtocolError::MalformedAck(line.to_string()))?;
    let brace_end = rest
        .find('}')
        .ok_or_else(|| ProtocolError::MalformedAck(line.to_string()))?;
    let command = &rest[..brace_end];
    let after = &rest[brace_end + 1..];
    let message = after.strip_prefix(' ').unwrap_or(after);

    Ok(ClassifiedLine::Ack {
        code,
        list_position,
        command: command.to_string(),
        message: message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- command serialisation -----

    #[test]
    fn serialise_command_no_args() {
        let bytes = serialise_command("status", &[]).unwrap();
        assert_eq!(bytes, b"status\n");
    }

    #[test]
    fn serialise_command_one_arg_quoted() {
        let bytes = serialise_command("play", &["0"]).unwrap();
        assert_eq!(bytes, b"play \"0\"\n");
    }

    #[test]
    fn serialise_command_multiple_args() {
        let bytes = serialise_command("add", &["music/x.flac", "2"]).unwrap();
        assert_eq!(bytes, b"add \"music/x.flac\" \"2\"\n");
    }

    #[test]
    fn serialise_command_arg_with_space() {
        let bytes = serialise_command("find", &["Album", "Dark Side"]).unwrap();
        assert_eq!(bytes, b"find \"Album\" \"Dark Side\"\n");
    }

    #[test]
    fn serialise_command_escapes_backslash() {
        let bytes = serialise_command("add", &["path\\with\\bs"]).unwrap();
        assert_eq!(bytes, b"add \"path\\\\with\\\\bs\"\n");
    }

    #[test]
    fn serialise_command_escapes_double_quote() {
        let bytes =
            serialise_command("find", &["Artist", "say \"hi\""]).unwrap();
        assert_eq!(bytes, b"find \"Artist\" \"say \\\"hi\\\"\"\n");
    }

    #[test]
    fn serialise_command_passes_apostrophe_unescaped() {
        // Apostrophes inside a double-quoted string do not require
        // escaping (unlike MPD's older single-quoted form).
        let bytes = serialise_command("find", &["Album", "B'Day"]).unwrap();
        assert_eq!(bytes, b"find \"Album\" \"B'Day\"\n");
    }

    #[test]
    fn serialise_command_empty_arg_emits_empty_quotes() {
        let bytes = serialise_command("search", &[""]).unwrap();
        assert_eq!(bytes, b"search \"\"\n");
    }

    #[test]
    fn serialise_command_preserves_utf8_arg() {
        let bytes =
            serialise_command("find", &["Artist", "Bj\u{00f6}rk"]).unwrap();
        let expected = b"find \"Artist\" \"Bj\xc3\xb6rk\"\n";
        assert_eq!(bytes, expected);
    }

    #[test]
    fn serialise_command_rejects_newline_in_command() {
        let err = serialise_command("bad\ncommand", &[]).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CommandForbiddenChar { ch: '\n' }
        ));
    }

    #[test]
    fn serialise_command_rejects_newline_in_arg() {
        let err = serialise_command("add", &["line1\nline2"]).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CommandForbiddenChar { ch: '\n' }
        ));
    }

    #[test]
    fn serialise_command_rejects_cr_in_arg() {
        let err = serialise_command("add", &["line1\rline2"]).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CommandForbiddenChar { ch: '\r' }
        ));
    }

    #[test]
    fn serialise_command_rejects_nul_in_arg() {
        let err = serialise_command("add", &["a\0b"]).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::CommandForbiddenChar { ch: '\0' }
        ));
    }

    // ----- welcome parsing -----

    #[test]
    fn parse_welcome_standard_version() {
        let v = parse_welcome("OK MPD 0.23.5").unwrap();
        assert_eq!(v, MpdVersion::new(0, 23, 5));
    }

    #[test]
    fn parse_welcome_older_version() {
        let v = parse_welcome("OK MPD 0.21.0").unwrap();
        assert_eq!(v, MpdVersion::new(0, 21, 0));
    }

    #[test]
    fn parse_welcome_rejects_missing_prefix() {
        let err = parse_welcome("HELLO MPD 0.23.5").unwrap_err();
        assert!(matches!(err, ProtocolError::BadWelcome(_)));
    }

    #[test]
    fn parse_welcome_rejects_no_space_after_mpd() {
        let err = parse_welcome("OK MPD0.23.5").unwrap_err();
        assert!(matches!(err, ProtocolError::BadWelcome(_)));
    }

    #[test]
    fn parse_welcome_rejects_non_numeric_version() {
        let err = parse_welcome("OK MPD foo.bar.baz").unwrap_err();
        assert!(matches!(err, ProtocolError::BadVersion(_)));
    }

    #[test]
    fn parse_welcome_rejects_two_part_version() {
        let err = parse_welcome("OK MPD 0.23").unwrap_err();
        assert!(matches!(err, ProtocolError::BadVersion(_)));
    }

    #[test]
    fn parse_welcome_rejects_four_part_version() {
        // MPD does not use four-part versions; a future shape change
        // should surface as an error so we notice.
        let err = parse_welcome("OK MPD 0.23.5.1").unwrap_err();
        assert!(matches!(err, ProtocolError::BadVersion(_)));
    }

    // ----- field parsing -----

    #[test]
    fn classify_line_simple_field() {
        let c = classify_line("volume: 50").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Field(Field {
                key: "volume".to_string(),
                value: "50".to_string(),
            })
        );
    }

    #[test]
    fn classify_line_field_with_colons_in_value() {
        let c = classify_line("audio: 44100:24:2").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Field(Field {
                key: "audio".to_string(),
                value: "44100:24:2".to_string(),
            })
        );
    }

    #[test]
    fn classify_line_field_preserves_spaces_in_value() {
        let c = classify_line("Title: Dark Side Of The Moon").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Field(Field {
                key: "Title".to_string(),
                value: "Dark Side Of The Moon".to_string(),
            })
        );
    }

    #[test]
    fn classify_line_field_preserves_utf8_in_value() {
        let c = classify_line("Artist: Bj\u{00f6}rk").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Field(Field {
                key: "Artist".to_string(),
                value: "Bj\u{00f6}rk".to_string(),
            })
        );
    }

    #[test]
    fn classify_line_rejects_field_without_separator() {
        let err = classify_line("novalue").unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedKeyValue(_)));
    }

    #[test]
    fn classify_line_rejects_empty_key() {
        let err = classify_line(": value").unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedKeyValue(_)));
    }

    // ----- terminator parsing -----

    #[test]
    fn classify_line_ok_terminator() {
        assert_eq!(classify_line("OK").unwrap(), ClassifiedLine::Ok);
    }

    #[test]
    fn classify_line_ack_standard_form() {
        let c = classify_line("ACK [2@0] {play} Bad song index").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Ack {
                code: 2,
                list_position: 0,
                command: "play".to_string(),
                message: "Bad song index".to_string(),
            }
        );
    }

    #[test]
    fn classify_line_ack_with_brackets_in_message() {
        // `[2]` appearing in the human-readable part of the message
        // must not confuse the parser (the first `]` comes after the
        // code@pos and is the end of the bracket group).
        let c = classify_line("ACK [50@0] {add} [2] not found").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Ack {
                code: 50,
                list_position: 0,
                command: "add".to_string(),
                message: "[2] not found".to_string(),
            }
        );
    }

    #[test]
    fn classify_line_ack_with_list_position_non_zero() {
        let c = classify_line("ACK [1@3] {play} No such song").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Ack {
                code: 1,
                list_position: 3,
                command: "play".to_string(),
                message: "No such song".to_string(),
            }
        );
    }

    #[test]
    fn classify_line_rejects_ack_without_brackets() {
        let err = classify_line("ACK [missing_at] {cmd} msg").unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedAck(_)));
    }

    #[test]
    fn classify_line_rejects_ack_with_non_numeric_code() {
        let err = classify_line("ACK [abc@0] {cmd} msg").unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedAck(_)));
    }

    #[test]
    fn classify_line_rejects_ack_missing_braces() {
        let err = classify_line("ACK [2@0] cmd msg").unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedAck(_)));
    }

    #[test]
    fn classify_line_rejects_ack_without_message() {
        // `{play}` with nothing after - the brace-end is present but
        // we expect a space and then a message. Tolerated as empty
        // message if no space, erroring if truly malformed. Our
        // implementation tolerates `{cmd}` with no trailing content
        // as empty message for robustness; assert that contract.
        let c = classify_line("ACK [2@0] {play}").unwrap();
        assert_eq!(
            c,
            ClassifiedLine::Ack {
                code: 2,
                list_position: 0,
                command: "play".to_string(),
                message: "".to_string(),
            }
        );
    }

    // ----- field vs ACK disambiguation -----

    #[test]
    fn classify_line_does_not_treat_ack_in_value_as_terminator() {
        // A field named `acknowledge` (hypothetical; MPD does not use
        // this, but defensive programming) would classify as a field
        // not an ACK.
        let c = classify_line("acknowledge: yes").unwrap();
        if let ClassifiedLine::Field(f) = c {
            assert_eq!(f.key, "acknowledge");
        } else {
            panic!("expected Field");
        }
    }
}
