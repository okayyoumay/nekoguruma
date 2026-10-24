//! A `tracing` layer that writes each event as one JSON object per line (design 12.2).

use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::file::RotatingFile;
use crate::time::utc_timestamp;

/// The name of the field that holds text NGR does not write itself (ADR-348 item 6), which an
/// entry keeps apart from its other fields.
const EXTERNAL: &str = "external";

/// Writes each event to a [`RotatingFile`] as one line:
///
/// ```json
/// {"timestamp":"2026-10-24T06:40:07.123456Z","level":"WARN","target":"agent::runner",
///  "process":"agent","message":"could not close the link","fields":{"error":"..."},
///  "external":"...","spans":[{"name":"job","fields":{"job_id":"..."},"external":"..."}]}
/// ```
///
/// (one line in the file). `fields`, `external` and `spans` are left out when empty, and a
/// span's `fields` and `external` likewise. A value recorded as a number or a boolean is written
/// as one; everything else as a string. A failed write is dropped: the log cannot report its own
/// failure.
#[derive(Debug)]
pub struct JsonLayer {
    process: String,
    file: Mutex<RotatingFile>,
}

impl JsonLayer {
    /// A layer writing to `file`, with `process` as each entry's process: `agent`, `server`,
    /// or a worker's API and number, never a configured library or VCI name (design 12.2).
    pub fn new(process: impl Into<String>, file: RotatingFile) -> Self {
        Self {
            process: process.into(),
            file: Mutex::new(file),
        }
    }

    fn entry<S>(&self, event: &Event<'_>, ctx: &Context<'_, S>, time: SystemTime) -> Vec<u8>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let metadata = event.metadata();
        let mut entry = Map::new();
        entry.insert("timestamp".into(), utc_timestamp(time).into());
        entry.insert("level".into(), metadata.level().as_str().into());
        entry.insert("target".into(), metadata.target().into());
        entry.insert("process".into(), self.process.clone().into());
        entry.insert(
            "message".into(),
            fields.message.take().unwrap_or_default().into(),
        );
        fields.into_entry(&mut entry);
        let spans: Vec<Value> = ctx
            .event_scope(event)
            .into_iter()
            .flat_map(|scope| scope.from_root())
            .map(|span| {
                let mut object = Map::new();
                object.insert("name".into(), span.name().into());
                if let Some(fields) = span.extensions().get::<Fields>() {
                    fields.clone().into_entry(&mut object);
                }
                Value::Object(object)
            })
            .collect();
        if !spans.is_empty() {
            entry.insert("spans".into(), Value::Array(spans));
        }
        let mut line = serialize_in_order(&entry);
        line.push(b'\n');
        line
    }
}

/// `entry` as a JSON object with its keys in insertion order, which `serde_json`'s map does not
/// keep without a feature: timestamp, level, target, process and message come first.
fn serialize_in_order(entry: &Map<String, Value>) -> Vec<u8> {
    const ORDER: [&str; 8] = [
        "timestamp",
        "level",
        "target",
        "process",
        "message",
        "fields",
        EXTERNAL,
        "spans",
    ];
    let mut out = b"{".to_vec();
    for key in ORDER.iter().filter(|key| entry.contains_key(**key)) {
        if out.len() > 1 {
            out.push(b',');
        }
        // Writing to a `Vec` and serializing a string or a `Value` cannot fail.
        serde_json::to_writer(&mut out, key).expect("a key serializes");
        out.push(b':');
        serde_json::to_writer(&mut out, &entry[*key]).expect("a value serializes");
    }
    out.push(b'}');
    out
}

impl<S> Layer<S> for JsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<Fields>()
        {
            values.record(fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let line = self.entry(event, &ctx, SystemTime::now());
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_entry(&line);
        }
    }
}

/// An event's or a span's recorded fields.
#[derive(Debug, Clone, Default)]
struct Fields {
    message: Option<String>,
    values: Map<String, Value>,
    external: Option<String>,
}

impl Fields {
    fn insert(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => {
                self.message = Some(match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                })
            }
            EXTERNAL => {
                self.external = Some(match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                })
            }
            name => {
                self.values.insert(name.to_owned(), value);
            }
        }
    }

    /// Adds `fields` and `external` to `object` when they are not empty; a span's message, which
    /// a span cannot have, is ignored.
    fn into_entry(self, object: &mut Map<String, Value>) {
        if !self.values.is_empty() {
            object.insert("fields".into(), Value::Object(self.values));
        }
        if let Some(external) = self.external {
            object.insert(EXTERNAL.into(), external.into());
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, format!("{value:?}").into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // NaN and the infinities have no JSON number; they are written as text.
        let value = serde_json::Number::from_f64(value)
            .map_or_else(|| value.to_string().into(), Value::Number);
        self.insert(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value.into());
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::file::FileSettings;

    /// Runs `log` with a [`JsonLayer`] as the only subscriber and returns the entries written.
    fn entries(log: impl FnOnce()) -> Vec<Value> {
        let temp = tempfile::tempdir().unwrap();
        let settings = FileSettings::new(temp.path().join("agent.log"));
        let layer = JsonLayer::new("agent", RotatingFile::open(settings.clone()).unwrap());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, log);
        let text = std::fs::read_to_string(&settings.path).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// An entry carries the timestamp, level, target, process and message, the event's fields,
    /// its `external` field apart from them, and its spans' fields root first.
    #[test]
    fn an_entry_is_one_json_line_with_its_fields_and_spans() {
        let entries = entries(|| {
            let job = tracing::info_span!("job", job_id = "job-1", generation = 3_u64);
            let _job = job.enter();
            let step = tracing::debug_span!("step", external = "VCI \"bench\"");
            let _step = step.enter();
            tracing::warn!(
                pc = 12_u32,
                ok = false,
                error = %"the link\nfailed",
                external = "vendor said\\nno",
                "could not close the link"
            );
        });
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let timestamp = entry["timestamp"].as_str().unwrap();
        assert_eq!(
            timestamp.len(),
            "2026-10-24T06:40:07.123456Z".len(),
            "{timestamp}"
        );
        assert!(timestamp.ends_with('Z'), "{timestamp}");
        assert_eq!(entry["level"], "WARN");
        assert_eq!(entry["target"], module_path!());
        assert_eq!(entry["process"], "agent");
        assert_eq!(entry["message"], "could not close the link");
        assert_eq!(
            entry["fields"],
            serde_json::json!({"pc": 12, "ok": false, "error": "the link\nfailed"})
        );
        assert_eq!(entry["external"], "vendor said\\nno");
        assert_eq!(
            entry["spans"],
            serde_json::json!([
                {"name": "job", "fields": {"job_id": "job-1", "generation": 3}},
                {"name": "step", "external": "VCI \"bench\""},
            ])
        );
    }

    /// The keys come in a fixed order, timestamp first, and the line holds no raw line break
    /// even when a value does.
    #[test]
    fn an_entry_stays_one_line_with_its_keys_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let settings = FileSettings::new(temp.path().join("agent.log"));
        let layer = JsonLayer::new("agent", RotatingFile::open(settings.clone()).unwrap());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(detail = "a\nb\r\u{1b}", "two\nlines");
        });
        let text = std::fs::read_to_string(&settings.path).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        assert!(text.starts_with(r#"{"timestamp":""#), "{text}");
        let keys = [
            "\"level\"",
            "\"target\"",
            "\"process\"",
            "\"message\"",
            "\"fields\"",
        ];
        let positions: Vec<usize> = keys.iter().map(|key| text.find(key).unwrap()).collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    }

    /// A field recorded after the span was created is in its entries; an entry with no fields
    /// and no span has none of those keys.
    #[test]
    fn a_span_s_later_fields_are_kept_and_empty_parts_left_out() {
        let entries = entries(|| {
            let job = tracing::info_span!("job", job_id = tracing::field::Empty);
            job.record("job_id", "job-2");
            job.in_scope(|| tracing::info!("started"));
            tracing::info!("outside");
        });
        assert_eq!(
            entries[0]["spans"],
            serde_json::json!([{"name": "job", "fields": {"job_id": "job-2"}}])
        );
        let outside = entries[1].as_object().unwrap();
        for key in ["fields", "external", "spans"] {
            assert!(!outside.contains_key(key), "{outside:?}");
        }
    }

    /// Entries go through the rotating file, so the size limit holds for them too.
    #[test]
    fn entries_are_rotated_with_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let settings = FileSettings {
            max_bytes: 300,
            max_files: 1,
            ..FileSettings::new(temp.path().join("agent.log"))
        };
        let layer = JsonLayer::new("agent", RotatingFile::open(settings.clone()).unwrap());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            for n in 0..10 {
                tracing::info!(n, "an entry");
            }
        });
        let current = std::fs::read_to_string(&settings.path).unwrap();
        let rotated = std::fs::read_to_string(settings.rotated_path(1)).unwrap();
        assert!(current.len() <= 300 && rotated.len() <= 300);
        assert!(current.ends_with("}\n") && rotated.ends_with("}\n"));
        assert!(current.contains(r#""n":9"#), "{current}");
        assert!(!settings.rotated_path(2).exists());
    }
}
