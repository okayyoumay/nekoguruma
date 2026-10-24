//! A `tracing` layer that writes each event as one line of text, for stderr (design 12.2: stderr
//! stays human-readable).

use std::io::Write;
use std::time::SystemTime;

use serde_json::Value;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::json::{Fields, record_new_span, record_span};
use crate::time::utc_timestamp;

/// How a text output writes an `external` field (ADR-348 item 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalText {
    /// The text itself: an output that stays on the device, such as a terminal's stderr.
    Full,
    /// Its length only: an output the OS may forward off the device, such as a stderr that is
    /// not a terminal (a service manager's), `journald` or `eventlog`.
    LengthOnly,
}

/// Writes each event as one line:
///
/// ```text
/// 2026-10-24T06:40:07.123456Z  WARN agent::runner: job{job_id="run-1" generation=0 external=<7 bytes>}: could not close the link error="..." external=<12 bytes>
/// ```
///
/// The spans come root first, then the message, then the event's fields. A string field is
/// written quoted and escaped, the message with its line breaks and control characters escaped,
/// so an entry stays one line. A failed write is dropped: the log cannot report its own failure.
#[derive(Debug)]
pub struct TextLayer<W> {
    writer: W,
    external: ExternalText,
}

impl<W> TextLayer<W> {
    /// A layer writing to what `writer` makes (`std::io::stderr`, for example), with `external`
    /// fields written as `external` says.
    pub fn new(writer: W, external: ExternalText) -> Self {
        Self { writer, external }
    }

    fn line<S>(&self, event: &Event<'_>, ctx: &Context<'_, S>, time: SystemTime) -> String
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let metadata = event.metadata();
        let mut line = format!(
            "{} {:>5} {}: ",
            utc_timestamp(time),
            metadata.level().as_str(),
            metadata.target()
        );
        for span in ctx
            .event_scope(event)
            .into_iter()
            .flat_map(|scope| scope.from_root())
        {
            line.push_str(span.name());
            if let Some(span_fields) = span.extensions().get::<Fields>() {
                let mut parts = Vec::new();
                if let Some(message) = &span_fields.message {
                    parts.push(format!("message={message:?}"));
                }
                self.push_fields(&mut parts, span_fields);
                if !parts.is_empty() {
                    line.push('{');
                    line.push_str(&parts.join(" "));
                    line.push('}');
                }
            }
            line.push_str(": ");
        }
        line.push_str(&escape(fields.message.as_deref().unwrap_or_default()));
        let mut parts = Vec::new();
        self.push_fields(&mut parts, &fields);
        for part in parts {
            line.push(' ');
            line.push_str(&part);
        }
        line.push('\n');
        line
    }

    fn push_fields(&self, parts: &mut Vec<String>, fields: &Fields) {
        for (name, value) in &fields.values {
            let value = match value {
                Value::String(text) => format!("{text:?}"),
                other => other.to_string(),
            };
            parts.push(format!("{name}={value}"));
        }
        if let Some(external) = &fields.external {
            parts.push(match self.external {
                ExternalText::Full => format!("external={external:?}"),
                ExternalText::LengthOnly => format!("external=<{} bytes>", external.len()),
            });
        }
    }
}

/// `text` with its backslashes, line breaks and other control characters escaped.
fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escaped: String = if c == '\\' || c.is_control() {
                c.escape_default().collect()
            } else {
                c.to_string()
            };
            escaped.chars().collect::<Vec<_>>()
        })
        .collect()
}

impl<S, W> Layer<S> for TextLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        record_new_span(attrs, id, &ctx);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        record_span(id, values, &ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let line = self.line(event, &ctx, SystemTime::now());
        let _ = self.writer.make_writer().write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    /// A writer that keeps what is written.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn lines(external: ExternalText, log: impl FnOnce()) -> String {
        let captured = Captured::default();
        let subscriber =
            tracing_subscriber::registry().with(TextLayer::new(captured.clone(), external));
        tracing::subscriber::with_default(subscriber, log);
        String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
    }

    fn log_in_a_job() {
        let job = tracing::info_span!("job", job_id = "run-1", external = "Ann's VCI");
        let _job = job.enter();
        tracing::warn!(
            pc = 7_u32,
            external = "vendor\nsaid",
            "could not close\nthe link"
        );
    }

    /// An entry is one line: timestamp, level, target, the spans with their fields, the message
    /// and the fields, with the message's line break escaped.
    #[test]
    fn an_entry_is_one_line_with_its_spans_and_fields() {
        let text = lines(ExternalText::Full, log_in_a_job);
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        let (timestamp, rest) = text.split_once(' ').unwrap();
        assert!(timestamp.ends_with('Z'), "{text}");
        assert_eq!(
            rest.trim_start(),
            format!(
                "WARN {}: job{{job_id=\"run-1\" external=\"Ann's VCI\"}}: could not close\\nthe link \
                 pc=7 external=\"vendor\\nsaid\"\n",
                module_path!()
            )
        );
    }

    /// An output that may be forwarded writes each `external` field as its length only.
    #[test]
    fn a_forwarded_output_writes_external_as_its_length() {
        let text = lines(ExternalText::LengthOnly, log_in_a_job);
        assert!(!text.contains("Ann") && !text.contains("vendor"), "{text}");
        assert!(
            text.contains("job{job_id=\"run-1\" external=<9 bytes>}"),
            "{text}"
        );
        assert!(text.ends_with("pc=7 external=<11 bytes>\n"), "{text}");
    }
}
