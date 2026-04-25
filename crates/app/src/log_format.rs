//! Custom tracing-subscriber event formatter. Emits each log line in green
//! ANSI, matching the banner. Errors and warnings get a brighter green +
//! a small glyph so they stand out without breaking the theme.

use std::fmt;
use std::time::Instant;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::{
    format::Writer, FmtContext, FormatEvent, FormatFields,
};
use tracing_subscriber::registry::LookupSpan;

const GREEN: &str = "\x1b[32m";
const BRIGHT_GREEN: &str = "\x1b[1;92m";
const DIM_GREEN: &str = "\x1b[2;32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Green-themed event formatter. Stateless apart from a process-relative
/// "uptime" reference so log timestamps look like `[12.345s]` instead of
/// the full system clock.
pub struct GreenFormat {
    start: Instant,
}

impl GreenFormat {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }
}

impl<S, N> FormatEvent<S, N> for GreenFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let elapsed = self.start.elapsed();
        let secs = elapsed.as_secs_f64();

        let (level_color, glyph) = match *meta.level() {
            Level::ERROR => (RED, "✗"),
            Level::WARN => (YELLOW, "!"),
            Level::INFO => (BRIGHT_GREEN, "›"),
            Level::DEBUG => (GREEN, "·"),
            Level::TRACE => (DIM_GREEN, "."),
        };

        write!(
            writer,
            "{DIM_GREEN}[{:>7.3}s]{RESET} {}{} {:<5}{RESET} {GREEN}",
            secs,
            level_color,
            glyph,
            meta.level().as_str(),
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer, "{RESET}")
    }
}
