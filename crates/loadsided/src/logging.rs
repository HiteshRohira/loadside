//! tracing → log file + in-memory ring buffer (served to clients via `logs`
//! and streamed as `log` events).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use loadside_core::protocol::{Event, LogLine};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

const CAPACITY: usize = 1000;

#[derive(Default)]
pub struct LogBuffer {
    lines: Mutex<VecDeque<LogLine>>,
    events: Mutex<Option<broadcast::Sender<Event>>>,
}

impl LogBuffer {
    pub fn set_sink(&self, tx: broadcast::Sender<Event>) {
        *self.events.lock().unwrap() = Some(tx);
    }
    pub fn recent(&self, limit: usize) -> Vec<LogLine> {
        let l = self.lines.lock().unwrap();
        l.iter().skip(l.len().saturating_sub(limit)).cloned().collect()
    }
    fn push(&self, line: LogLine) {
        {
            let mut l = self.lines.lock().unwrap();
            if l.len() >= CAPACITY {
                l.pop_front();
            }
            l.push_back(line.clone());
        }
        if let Some(tx) = self.events.lock().unwrap().as_ref() {
            let _ = tx.send(Event::Log { line });
        }
    }
}

struct BufferLayer(Arc<LogBuffer>);

#[derive(Default)]
struct MsgVisitor(String);
impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        self.0.push(LogLine {
            ts: loadside_core::now(),
            level: meta.level().to_string().to_lowercase(),
            target: meta.target().split("::").next().unwrap_or("").to_string(),
            message: v.0,
        });
    }
}

pub fn init(foreground: bool) -> Arc<LogBuffer> {
    let buffer = Arc::new(LogBuffer::default());
    let path = loadside_core::paths::log_file();
    if std::fs::metadata(&path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let file = {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path).ok();
        // Older files may have been created world-readable.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        f
    };
    let filter = || {
        EnvFilter::try_from_env("LOADSIDE_LOG")
            .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,reqwest=warn,rustls=warn,idevice=warn,tungstenite=warn,tokio_tungstenite=warn"))
    };
    let file_layer = file.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_writer(Mutex::new(f))
            .with_filter(filter())
    });
    let stderr_layer = foreground.then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(filter()));
    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .with(BufferLayer(buffer.clone()).with_filter(filter()))
        .init();
    buffer
}
