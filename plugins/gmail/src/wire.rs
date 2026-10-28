//! CRLF framing, shared by the IMAP and SMTP clients.
//!
//! Both read a line at a time off a growing buffer, and both had the same
//! off-by-one: the terminator search found the `\r`, the drain stopped after
//! it, and the leftover `\n` then led every following line. The first sync
//! against a real mailbox hung on it — the greeting was recognisable, but
//! `T1 OK …` never matched its tag because it arrived as `\nT1 OK …`, and
//! the client waited for an answer it had already buffered until the read
//! timeout fired. The two callers now share one drain, so the next reader
//! cannot repeat it.

/// Remove one CRLF-terminated line from `buffer` and return it without the
/// terminator. `None` means no complete line has arrived yet.
pub fn take_line(buffer: &mut Vec<u8>) -> Option<String> {
    let end = buffer.windows(2).position(|pair| pair == b"\r\n")?;
    // `end` is the `\r`; the `\n` after it is half of the terminator, so the
    // drain has to take both bytes.
    let mut line: Vec<u8> = buffer.drain(..=end + 1).collect();
    line.truncate(line.len() - 2);
    Some(String::from_utf8_lossy(&line).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_leaves_the_next_one_whole() {
        let mut buffer = b"* OK hi\r\nT1 OK done\r\n".to_vec();
        assert_eq!(take_line(&mut buffer).as_deref(), Some("* OK hi"));
        assert_eq!(take_line(&mut buffer).as_deref(), Some("T1 OK done"));
        assert_eq!(take_line(&mut buffer), None);
        assert!(buffer.is_empty(), "the terminator goes with the line");
    }

    #[test]
    fn a_partial_line_waits_for_its_terminator() {
        let mut buffer = b"* OK hi\r".to_vec();
        assert_eq!(take_line(&mut buffer), None);
        buffer.push(b'\n');
        assert_eq!(take_line(&mut buffer).as_deref(), Some("* OK hi"));
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_empty_line_is_a_line() {
        let mut buffer = b"\r\nnext\r\n".to_vec();
        assert_eq!(take_line(&mut buffer).as_deref(), Some(""));
        assert_eq!(take_line(&mut buffer).as_deref(), Some("next"));
    }
}
