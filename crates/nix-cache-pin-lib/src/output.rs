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
    probes: Vec<ProbeEvidence>,
    branch_tip: Option<String>,
    branch_members: std::collections::HashSet<String>,
}

/// Exact evaluator configuration and typed results retained for every unique probe.
#[derive(Clone, serde::Serialize)]
pub struct ProbeEvidence {
    pub config: crate::config::PinConfig,
    pub current_consumer: bool,
    pub verification: crate::narinfo::VerifyResult,
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
            probes: Vec::new(),
            branch_tip: None,
            branch_members: Default::default(),
        }
    }

    /// Buffered mode: lines are stored until `flush()` is called.
    pub fn buffered(name: &str) -> Self {
        Self {
            name: name.to_string(),
            mode: Mode::Buffered { buffer: Vec::new() },
            probes: Vec::new(),
            branch_tip: None,
            branch_members: Default::default(),
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
            probes: Vec::new(),
            branch_tip: None,
            branch_members: Default::default(),
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
            probes: Vec::new(),
            branch_tip: None,
            branch_members: Default::default(),
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

    pub fn probes(&self) -> &[ProbeEvidence] {
        &self.probes
    }

    pub fn lines(&self) -> &[String] {
        match &self.mode {
            Mode::Buffered { buffer } => buffer,
            _ => &[],
        }
    }

    pub(crate) fn set_branch_members(&mut self, commits: &[String]) {
        self.branch_tip = commits.first().cloned();
        self.branch_members = commits.iter().cloned().collect();
    }
    pub(crate) fn branch_contains(&self, rev: &str) -> bool {
        self.branch_members.contains(rev)
    }
    pub(crate) fn branch_tip(&self) -> Option<&str> {
        self.branch_tip.as_deref()
    }

    pub(crate) fn prior_probe(
        &self,
        cfg: &crate::config::PinConfig,
        rev: &str,
        current: bool,
    ) -> Option<crate::narinfo::VerifyResult> {
        self.probes
            .iter()
            .find(|p| {
                p.verification.rev == rev
                    && p.current_consumer == current
                    && same_probe_context(&p.config, cfg)
            })
            .map(|p| p.verification.clone())
    }

    pub(crate) fn record_probe(
        &mut self,
        config: &crate::config::PinConfig,
        current_consumer: bool,
        verification: crate::narinfo::VerifyResult,
    ) {
        self.probes.push(ProbeEvidence {
            config: config.clone(),
            current_consumer,
            verification,
        });
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

fn same_probe_context(a: &crate::config::PinConfig, b: &crate::config::PinConfig) -> bool {
    a.input_name == b.input_name
        && a.flake_ref == b.flake_ref
        && a.arch == b.arch
        && a.flake_output == b.flake_output
        && a.full_attr_prefix() == b.full_attr_prefix()
        && a.required_labels() == b.required_labels()
        && a.caches == b.caches
        && a.consumer_flake_ref == b.consumer_flake_ref
        && a.consumer_targets == b.consumer_targets
        && a.required_consumer_targets == b.required_consumer_targets
        && a.verify_closure == b.verify_closure
        && a.version_constraints == b.version_constraints
        && a.fail_fast == b.fail_fast
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
