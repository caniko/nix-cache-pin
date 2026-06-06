use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::time::Duration;

/// Internal mode discriminant for `Output`.
enum Mode {
    /// Prints directly to stderr (single-pin, non-TTY fallback).
    Immediate,
    /// Stores lines and flushes them as an atomic block.
    Buffered { buffer: Vec<String> },
    /// Drives an `indicatif` spinner: transient action on the spinner line,
    /// persistent milestones printed above via `ProgressBar::println`.
    Spinner(ProgressBar),
}

/// Abstraction over stderr output that supports immediate, buffered, and spinner modes.
pub struct Output {
    name: String,
    mode: Mode,
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .expect("static spinner template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "])
}

impl Output {
    /// Immediate mode: each call goes straight to stderr.
    pub fn immediate(name: &str) -> Self {
        Self {
            name: name.to_string(),
            mode: Mode::Immediate,
        }
    }

    /// Buffered mode: lines are stored until `flush()` is called.
    pub fn buffered(name: &str) -> Self {
        Self {
            name: name.to_string(),
            mode: Mode::Buffered { buffer: Vec::new() },
        }
    }

    /// Spinner mode: standalone spinner on stderr.
    pub fn spinner(name: &str) -> Self {
        let pb = ProgressBar::new_spinner();
        pb.set_style(spinner_style());
        pb.enable_steady_tick(Duration::from_millis(80));
        Self {
            name: name.to_string(),
            mode: Mode::Spinner(pb),
        }
    }

    /// Spinner mode: attached to a shared `MultiProgress`.
    pub fn spinner_in(name: &str, mp: &MultiProgress) -> Self {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(spinner_style());
        pb.enable_steady_tick(Duration::from_millis(80));
        pb.set_message(format!("{}", format!("{name}: searching...").cyan()));
        Self {
            name: name.to_string(),
            mode: Mode::Spinner(pb),
        }
    }

    /// Update the transient spinner action text.
    /// In non-spinner modes this is a no-op (the info would just scroll away).
    pub fn set_action(&self, msg: impl std::fmt::Display) {
        if let Mode::Spinner(pb) = &self.mode {
            pb.set_message(format!("{msg}"));
        }
    }

    /// Record a persistent milestone.
    /// - Spinner: prints above the spinner line.
    /// - Immediate: prints to stderr.
    /// - Buffered: appends to the buffer.
    pub fn milestone(&mut self, msg: impl std::fmt::Display) {
        match &mut self.mode {
            Mode::Spinner(pb) => {
                pb.println(format!("{msg}"));
            }
            Mode::Immediate => {
                eprintln!("{msg}");
            }
            Mode::Buffered { buffer } => {
                buffer.push(format!("{msg}"));
            }
        }
    }

    /// Backward-compatible print. In spinner mode delegates to `milestone`.
    pub fn println(&mut self, msg: impl std::fmt::Display) {
        self.milestone(msg);
    }

    /// Finish the spinner with a success marker.
    pub fn finish_ok(&self, msg: impl std::fmt::Display) {
        if let Mode::Spinner(pb) = &self.mode {
            pb.finish_with_message(format!("{}", format!("✓ {msg}").green()));
        }
    }

    /// Finish the spinner with an error marker.
    pub fn finish_err(&self, msg: impl std::fmt::Display) {
        if let Mode::Spinner(pb) = &self.mode {
            pb.finish_with_message(format!("{}", format!("✗ {msg}").red()));
        }
    }

    /// Flush buffered output to stderr as an atomic block with a header separator.
    /// No-op in immediate and spinner modes.
    pub fn flush(&self) {
        if let Mode::Buffered { buffer } = &self.mode {
            if !buffer.is_empty() {
                eprintln!("\n{}", format!("=== {} ===", self.name).cyan().bold());
                for line in buffer {
                    eprintln!("{line}");
                }
            }
        }
    }

    /// Access the buffered output lines for testing.
    /// Returns an empty slice in non-buffered modes.
    #[cfg(test)]
    pub fn test_buffer(&self) -> &[String] {
        match &self.mode {
            Mode::Buffered { buffer } => buffer,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffered_milestone_collects() {
        let mut out = Output::buffered("test");
        out.milestone("line one");
        out.milestone("line two");
        assert_eq!(out.test_buffer(), &["line one", "line two"]);
    }

    #[test]
    fn test_buffered_empty_initially() {
        let out = Output::buffered("test");
        assert!(out.test_buffer().is_empty());
    }

    #[test]
    fn test_immediate_has_empty_buffer() {
        let out = Output::immediate("test");
        assert!(out.test_buffer().is_empty());
    }
}
