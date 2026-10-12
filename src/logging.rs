//! One JSON object per line on stdout, with the keys the shared host reads: `time` (RFC 3339
//! UTC with milliseconds), `level` (debug, info, warn, error), `msg`, plus `target` and the
//! event's own fields. `tracing-subscriber`'s JSON layer names them `timestamp` and
//! `fields.message`, hence a formatter of this repository's own.

use std::fmt;

use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;

pub fn init(config: &Config) {
    let filter = EnvFilter::try_new(&config.rust_log).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout);
    if config.app_env == "development" && std::env::var("LOG_FORMAT").as_deref() == Ok("pretty") {
        builder.pretty().init();
    } else {
        builder.event_format(HostFormat).init();
    }
}

struct HostFormat;

impl<S, N> FormatEvent<S, N> for HostFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let level = match *event.metadata().level() {
            tracing::Level::TRACE | tracing::Level::DEBUG => "debug",
            tracing::Level::INFO => "info",
            tracing::Level::WARN => "warn",
            tracing::Level::ERROR => "error",
        };
        let mut line = serde_json::Map::new();
        line.insert(
            "time".into(),
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
                .into(),
        );
        line.insert("level".into(), level.into());
        line.insert("target".into(), event.metadata().target().into());
        line.insert("msg".into(), fields.message.unwrap_or_default().into());
        for (key, value) in fields.other {
            line.insert(key, value);
        }
        writeln!(writer, "{}", serde_json::Value::Object(line))
    }
}

#[derive(Default)]
struct Fields {
    message: Option<String>,
    other: Vec<(String, serde_json::Value)>,
}

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.other.push((field.name().to_string(), text.into()));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.other.push((field.name().to_string(), value.into()));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.other.push((field.name().to_string(), value.into()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.other.push((field.name().to_string(), value.into()));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.other.push((field.name().to_string(), value.into()));
    }
}
