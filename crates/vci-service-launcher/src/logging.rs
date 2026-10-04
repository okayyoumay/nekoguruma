use tracing_subscriber::{filter::EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::{FileOutput, LogOutput, LogOutputKind, LoggingConfig};

pub fn init_logging(cfg: LoggingConfig) {
    let level = cfg.level.as_deref().unwrap_or("info");
    let filter = EnvFilter::new(level);

    match cfg.output.unwrap_or_default() {
        LogOutput::Named(LogOutputKind::Stderr) => init_stderr(filter),
        LogOutput::Named(LogOutputKind::Null) => init_null(filter),
        LogOutput::File(file_cfg) => init_file(filter, file_cfg),
        #[cfg(feature = "eventlog")]
        LogOutput::Named(LogOutputKind::Eventlog) => eventlog::init(filter),
        #[cfg(feature = "journald")]
        LogOutput::Named(LogOutputKind::Journald) => journald::init(filter),
    }
}

// ── Named destinations ────────────────────────────────────────────────────────

pub(crate) fn init_stderr(filter: EnvFilter) {
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(true),
        )
        .init();
}

fn init_null(filter: EnvFilter) {
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::sink)
                .with_target(true),
        )
        .init();
}

fn init_file(filter: EnvFilter, file_cfg: FileOutput) {
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_cfg.file)
    {
        Ok(file) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::sync::Mutex::new(file))
                        .with_target(true),
                )
                .init();
        }
        Err(e) => {
            eprintln!(
                "warning: cannot open log file '{}': {e}; falling back to stderr",
                file_cfg.file
            );
            init_stderr(filter);
        }
    }
}

// ── Shared field visitor (Windows Event Log) ──────────────────────────────────

/// Extracts structured tracing fields into a flat message string for backends
/// that do not support native structured logging.
#[cfg(all(feature = "eventlog", windows))]
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    extras: Vec<String>,
}

#[cfg(all(feature = "eventlog", windows))]
impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.extras.push(format!("{}={}", field.name(), value));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.extras.push(format!("{}={:?}", field.name(), value));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.extras.push(format!("{}={}", field.name(), value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.extras.push(format!("{}={}", field.name(), value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.extras.push(format!("{}={}", field.name(), value));
    }
}

#[cfg(all(feature = "eventlog", windows))]
impl FieldVisitor {
    fn into_message(self, target: &str) -> String {
        let mut parts = vec![format!("[{target}]")];
        if let Some(msg) = self.message {
            parts.push(msg);
        }
        parts.extend(self.extras);
        parts.join(" ")
    }
}

// ── Windows Event Log ─────────────────────────────────────────────────────────

#[cfg(feature = "eventlog")]
mod eventlog {
    #[cfg(windows)]
    use super::FieldVisitor;
    use super::init_stderr;
    use tracing_subscriber::filter::EnvFilter;

    pub fn init(filter: EnvFilter) {
        #[cfg(windows)]
        {
            let source = process_name();
            match EventLogLayer::new(&source) {
                Ok(layer) => {
                    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(layer)
                        .init();
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "warning: cannot register Windows event log source '{source}': {e}; \
                         falling back to stderr"
                    );
                }
            }
        }
        #[cfg(not(windows))]
        eprintln!("warning: eventlog output is only supported on Windows; falling back to stderr");
        init_stderr(filter);
    }

    // ── Layer implementation (Windows only) ───────────────────────────────────

    // In windows-sys 0.52, HANDLE = isize, which is Send + Sync — no wrapper needed.
    #[cfg(windows)]
    struct EventLogLayer {
        handle: windows_sys::Win32::Foundation::HANDLE,
    }

    #[cfg(windows)]
    impl EventLogLayer {
        fn new(source: &str) -> Result<Self, std::io::Error> {
            let source_wide: Vec<u16> = source.encode_utf16().chain(Some(0)).collect();
            let handle = unsafe {
                windows_sys::Win32::System::EventLog::RegisterEventSourceW(
                    std::ptr::null(),
                    source_wide.as_ptr(),
                )
            };
            if handle == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { handle })
        }
    }

    #[cfg(windows)]
    impl Drop for EventLogLayer {
        fn drop(&mut self) {
            unsafe {
                windows_sys::Win32::System::EventLog::DeregisterEventSource(self.handle);
            }
        }
    }

    #[cfg(windows)]
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLogLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            use tracing::Level;
            use windows_sys::Win32::System::EventLog::{
                EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE, ReportEventW,
            };

            let event_type = match *event.metadata().level() {
                Level::ERROR => EVENTLOG_ERROR_TYPE,
                Level::WARN => EVENTLOG_WARNING_TYPE,
                _ => EVENTLOG_INFORMATION_TYPE,
            };

            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            let msg = visitor.into_message(event.metadata().target());
            let msg_wide: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
            let strings: [*const u16; 1] = [msg_wide.as_ptr()];

            unsafe {
                ReportEventW(
                    self.handle,
                    event_type,
                    0,                               // category
                    0,                               // event ID
                    std::ptr::null_mut::<u8>() as _, // user SID (null)
                    1,                               // number of strings
                    0,                               // binary data size
                    strings.as_ptr(),
                    std::ptr::null::<u8>() as _, // binary data (null)
                );
            }
        }
    }

    fn process_name() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| env!("CARGO_PKG_NAME").to_owned())
    }
}

// ── systemd journal (journald) ────────────────────────────────────────────────

#[cfg(feature = "journald")]
mod journald {
    use super::init_stderr;
    use tracing_subscriber::filter::EnvFilter;

    pub fn init(filter: EnvFilter) {
        #[cfg(target_os = "linux")]
        {
            match ::tracing_journald::layer() {
                Ok(layer) => {
                    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(layer)
                        .init();
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "warning: cannot connect to journald socket: {e}; \
                         falling back to stderr"
                    );
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        eprintln!("warning: journald output is only supported on Linux; falling back to stderr");
        init_stderr(filter);
    }
}
