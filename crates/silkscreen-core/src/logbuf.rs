//! Keeps recent log lines in memory for the GUI's debug screen — the same
//! lines the terminal shows.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::SystemTime;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

const CAPACITY: usize = 5000;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: SystemTime,
    pub level: Level,
    pub text: String,
}

static LINES: Mutex<VecDeque<LogLine>> = Mutex::new(VecDeque::new());

/// Copy of the buffered lines, oldest first.
pub fn snapshot() -> Vec<LogLine> {
    LINES.lock().unwrap().iter().cloned().collect()
}

pub fn clear() {
    LINES.lock().unwrap().clear();
}

pub struct BufferLayer;

struct Fields(String);

impl Visit for Fields {
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

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut f = Fields(String::new());
        event.record(&mut f);
        let mut lines = LINES.lock().unwrap();
        if lines.len() == CAPACITY {
            lines.pop_front();
        }
        lines.push_back(LogLine {
            time: SystemTime::now(),
            level: *event.metadata().level(),
            text: f.0,
        });
        drop(lines);
        crate::ctx::wake_ui();
    }
}
