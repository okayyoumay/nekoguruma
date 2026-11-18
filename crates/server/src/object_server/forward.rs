//! Forwards what the object server writes to its standard output and error into the server's log
//! (ADR-491): one `tracing` event per line, bounded in length, with control characters replaced
//! and the secret key redacted. The pipes are drained until the child closes them, so a full pipe
//! never blocks it.

use std::sync::Arc;

use shared_crypto::Zeroizing;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::task::JoinHandle;

/// The most bytes of one line the forwarder keeps before it throws the rest of the line away. It
/// is far above the secret's length, so the redaction sees a whole secret before the cut.
const MAX_RAW_LINE: usize = 8192;
/// The most characters of a line that reach the log.
pub(crate) const MAX_LOG_LINE: usize = 1024;

/// Starts a task that forwards `reader` line by line; `stream` names it in the log. It ends at the
/// end of the stream.
pub(crate) fn spawn(
    stream: &'static str,
    reader: impl AsyncRead + Unpin + Send + 'static,
    secret: Arc<Zeroizing<String>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut line: Vec<u8> = Vec::new();
        let mut cut = false;
        loop {
            let (used, ended_line) = {
                let chunk = match reader.fill_buf().await {
                    Ok([]) | Err(_) => break,
                    Ok(chunk) => chunk,
                };
                let (part, ended_line) = match chunk.iter().position(|&b| b == b'\n') {
                    Some(at) => (&chunk[..at], true),
                    None => (chunk, false),
                };
                let room = MAX_RAW_LINE.saturating_sub(line.len());
                if part.len() > room {
                    cut = true;
                }
                line.extend_from_slice(&part[..part.len().min(room)]);
                (part.len() + usize::from(ended_line), ended_line)
            };
            reader.consume(used);
            if ended_line {
                emit(stream, &line, cut, secret.as_str());
                line.clear();
                cut = false;
            }
        }
        if !line.is_empty() {
            emit(stream, &line, cut, secret.as_str());
        }
    })
}

fn emit(stream: &'static str, raw: &[u8], cut: bool, secret: &str) {
    let text = sanitize(raw, cut, secret);
    tracing::info!(target: "object_server", stream, line = %text, "object server output");
}

/// The text of one line for the log: invalid UTF-8 replaced, every occurrence of `secret`
/// replaced by `[redacted]`, control characters (a terminal's colour codes among them) replaced by
/// `?` (a tab by a space), and the result cut to [`MAX_LOG_LINE`] characters with `...` marking a
/// cut (also when the forwarder had to drop the rest of an over-long line, `cut`).
pub(crate) fn sanitize(raw: &[u8], cut: bool, secret: &str) -> String {
    let lossy = String::from_utf8_lossy(raw);
    let redacted = if secret.is_empty() {
        lossy.into_owned()
    } else {
        lossy.replace(secret, "[redacted]")
    };
    let mut out = String::new();
    let mut rest = redacted.chars();
    for c in rest.by_ref().take(MAX_LOG_LINE) {
        out.push(match c {
            '\t' => ' ',
            c if c.is_control() => '?',
            c => c,
        });
    }
    let truncated = cut || rest.next().is_some();
    if truncated {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_secret_is_redacted_wherever_it_appears() {
        let line = format!("key={SECRET} again {SECRET}!");
        let shown = sanitize(line.as_bytes(), false, SECRET);
        assert_eq!(shown, "key=[redacted] again [redacted]!");
        assert!(!shown.contains(SECRET));
    }

    #[test]
    fn control_characters_are_replaced_and_invalid_utf8_is_lossy() {
        let shown = sanitize(b"\x1b[31mred\x1b[0m\ta\rb\x00c\xff", false, SECRET);
        assert!(!shown.chars().any(|c| c.is_control()), "{shown:?}");
        assert!(shown.starts_with("?[31mred?[0m a?b?c"), "{shown:?}");
    }

    #[test]
    fn a_long_line_is_cut_and_marked() {
        let long = "x".repeat(MAX_LOG_LINE + 50);
        let shown = sanitize(long.as_bytes(), false, SECRET);
        assert_eq!(shown.chars().count(), MAX_LOG_LINE + 3);
        assert!(shown.ends_with("..."));
        let exact = "x".repeat(MAX_LOG_LINE);
        assert_eq!(sanitize(exact.as_bytes(), false, SECRET), exact);
        assert!(sanitize(b"short", true, SECRET).ends_with("..."));
        // The cut is on a character boundary.
        let wide = "é".repeat(MAX_LOG_LINE + 1);
        assert_eq!(
            sanitize(wide.as_bytes(), false, SECRET).chars().count(),
            MAX_LOG_LINE + 3
        );
    }

    #[test]
    fn a_secret_cut_by_the_raw_bound_is_not_logged_in_part() {
        // The raw bound (8 KiB) is above the log bound, so what the forwarder drops never reaches
        // the log, and a secret that straddles the raw bound sits past the log bound too.
        const { assert!(MAX_RAW_LINE > MAX_LOG_LINE + 40) };
    }

    #[tokio::test]
    async fn the_forwarder_reads_to_the_end_and_bounds_a_line_without_a_newline() {
        let mut data = Vec::new();
        data.extend_from_slice(b"first\nsecond");
        data.extend(std::iter::repeat_n(b'y', MAX_RAW_LINE * 3));
        data.extend_from_slice(b"\nlast no newline");
        let task = spawn(
            "stdout",
            std::io::Cursor::new(data),
            Arc::new(Zeroizing::new(SECRET.to_owned())),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("the forwarder ends at the end of the stream")
            .unwrap();
    }
}
