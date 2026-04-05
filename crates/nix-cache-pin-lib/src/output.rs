use colored::Colorize;

/// Abstraction over stderr output that supports both immediate and buffered modes.
///
/// - **Immediate**: prints directly to stderr (single-pin, backward-compatible behavior).
/// - **Buffered**: stores lines and flushes them as an atomic block when a pin's search completes,
///   preventing interleaved output from concurrent pins.
pub struct Output {
    name: String,
    buffer: Vec<String>,
    buffered: bool,
}

impl Output {
    /// Immediate mode: each `println` goes straight to stderr.
    pub fn immediate(name: &str) -> Self {
        Self {
            name: name.to_string(),
            buffer: Vec::new(),
            buffered: false,
        }
    }

    /// Buffered mode: lines are stored until `flush()` is called.
    pub fn buffered(name: &str) -> Self {
        Self {
            name: name.to_string(),
            buffer: Vec::new(),
            buffered: true,
        }
    }

    /// Print a line. In immediate mode, writes to stderr. In buffered mode, stores the line.
    ///
    /// ANSI color codes are preserved in buffered mode because `colored` formats them eagerly
    /// at `format!()` time (when `Display::fmt` runs), not at write time.
    pub fn println(&mut self, msg: impl std::fmt::Display) {
        if self.buffered {
            self.buffer.push(format!("{msg}"));
        } else {
            eprintln!("{msg}");
        }
    }

    /// Flush buffered output to stderr as an atomic block with a header separator.
    /// No-op in immediate mode or when the buffer is empty.
    pub fn flush(&self) {
        if !self.buffer.is_empty() {
            eprintln!("\n{}", format!("=== {} ===", self.name).cyan().bold());
            for line in &self.buffer {
                eprintln!("{line}");
            }
        }
    }
}
