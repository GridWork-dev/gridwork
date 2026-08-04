//! A tracing sink that renders each event as one structured JSON line —
//! what every log call in this crate goes through instead of a bare
//! `eprintln!`.
//!
//! `tracing-subscriber` is not in this workspace's dependency graph and
//! adding it would be a real new dependency (several new crates,
//! transitively) for what implementing seven `Subscriber` trait methods
//! covers on its own. `tracing` itself IS already resolved — `sqlx` and
//! `tokio-util` pull it in transitively — so [`TracingLogger`] implements
//! `tracing::Subscriber` directly against that, rather than reaching for a
//! dependency this workspace does not already carry.
//!
//! Derivation: none — a logging sink; no terminal byte is parsed and no
//! process is supervised here.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};

/// One event, rendered to JSON and written as one line to `W`.
///
/// # What this sink does not do
///
/// ponytail: spans are accepted — [`new_span`](Subscriber::new_span) mints
/// an id, `enter`/`exit`/`record` are legal no-ops — but never tracked into
/// a "current span" context, so a span's own fields never land on an event
/// logged inside it; only the event's own fields do. A real span-aware
/// layer (or the `tracing-subscriber` dependency this crate deliberately
/// does not take) is the upgrade path if a caller ever needs that.
pub struct TracingLogger<W> {
    sink: Mutex<W>,
    max_level: Level,
    next_span: AtomicU64,
}

impl<W: Write + Send + 'static> TracingLogger<W> {
    /// A sink writing to `writer`, admitting events at `max_level` and
    /// everything more severe (`Level::INFO` admits `INFO`/`WARN`/`ERROR`
    /// and filters `DEBUG`/`TRACE` — see `tracing::Level`'s own doc on
    /// comparing levels: `ERROR < WARN < INFO < DEBUG < TRACE`).
    pub fn new(writer: W, max_level: Level) -> Self {
        Self {
            sink: Mutex::new(writer),
            max_level,
            // Span ids are backed by `NonZeroU64` (`tracing_core::span::Id`);
            // starting at 1 keeps every minted id nonzero without a branch on
            // the first call.
            next_span: AtomicU64::new(1),
        }
    }
}

impl<W: Write + Send + 'static> Subscriber for TracingLogger<W> {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= &self.max_level
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = self.next_span.fetch_add(1, Ordering::Relaxed);
        tracing::span::Id::from_u64(id)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {
        // ponytail: span fields are not retained — see the type doc.
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = FieldVisitor(serde_json::Map::new());
        event.record(&mut fields);

        let metadata = event.metadata();
        let mut line = serde_json::Map::new();
        line.insert(
            "level".to_owned(),
            serde_json::Value::String(metadata.level().to_string()),
        );
        line.insert(
            "target".to_owned(),
            serde_json::Value::String(metadata.target().to_owned()),
        );
        line.insert(
            "ts_unix_ms".to_owned(),
            serde_json::Value::from(now_unix_ms()),
        );
        line.extend(fields.0);

        let Ok(rendered) = serde_json::to_string(&serde_json::Value::Object(line)) else {
            // Every field value this visitor produces is already a `Value`
            // built from primitives or a formatted string, so a `Value`
            // this shape refuses to serialize is not a real failure mode —
            // dropping the line rather than panicking is still the correct
            // response if it somehow were.
            return;
        };
        if let Ok(mut sink) = self.sink.lock() {
            let _ = writeln!(sink, "{rendered}");
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Collects one event's fields into a JSON object, one key per field.
struct FieldVisitor(serde_json::Map<String, serde_json::Value>);

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(
            field.name().to_owned(),
            serde_json::Value::String(value.to_owned()),
        );
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0
            .insert(field.name().to_owned(), serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0
            .insert(field.name().to_owned(), serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0
            .insert(field.name().to_owned(), serde_json::Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // `Number::from_f64` refuses NaN/infinite — fall back to the debug
        // rendering rather than silently dropping the field.
        let rendered = serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(format!("{value:?}")));
        self.0.insert(field.name().to_owned(), rendered);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_owned(),
            serde_json::Value::String(format!("{value:?}")),
        );
    }
}

/// Milliseconds since the Unix epoch, for `ts_unix_ms`.
///
/// A plain integer rather than an RFC 3339 string: journald (this sink's
/// intended destination once the systemd unit runs it) already timestamps
/// every line on receipt, so a second in-line calendar date would be a
/// hand-rolled civil-date conversion earning its keep nowhere — `gridwork`'s
/// own `rfc3339`/`civil_from_days` (`crates/gridwork/src/main.rs`) exists
/// because a CLI's JSON answer has no other clock attached to it; a log line
/// captured by a service manager does.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| std::io::Error::other("poisoned test buffer"))?;
            guard.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines_of(buf: &SharedBuf) -> Vec<serde_json::Value> {
        let guard = buf.0.lock().expect("lock the test buffer");
        String::from_utf8_lossy(&guard)
            .lines()
            .map(|line| serde_json::from_str(line).expect("every emitted line is valid JSON"))
            .collect()
    }

    #[test]
    fn an_event_renders_as_one_structured_json_line_with_its_fields() {
        let buf = SharedBuf::default();
        let logger = TracingLogger::new(buf.clone(), Level::INFO);

        tracing::subscriber::with_default(logger, || {
            tracing::info!(session = "sess-1", bytes = 42u64, "spawned");
        });

        let lines = lines_of(&buf);
        assert_eq!(lines.len(), 1, "exactly one event was logged");
        let line = &lines[0];
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["session"], "sess-1");
        assert_eq!(line["bytes"], 42);
        assert!(line["target"].is_string());
        assert!(line["ts_unix_ms"].is_number());
        assert_eq!(line["message"], "spawned");
    }

    #[test]
    fn events_below_the_configured_level_are_never_written() {
        let buf = SharedBuf::default();
        let logger = TracingLogger::new(buf.clone(), Level::INFO);

        tracing::subscriber::with_default(logger, || {
            tracing::debug!("should not appear");
            tracing::info!("should appear");
        });

        let lines = lines_of(&buf);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["message"], "should appear");
    }

    #[test]
    fn a_non_finite_float_field_falls_back_to_its_debug_rendering_rather_than_being_dropped() {
        let buf = SharedBuf::default();
        let logger = TracingLogger::new(buf.clone(), Level::INFO);

        tracing::subscriber::with_default(logger, || {
            tracing::info!(ratio = f64::NAN, "odd reading");
        });

        let lines = lines_of(&buf);
        assert_eq!(lines[0]["ratio"], "NaN");
    }
}
