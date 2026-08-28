//! Terminal-aware styling for human-readable CLI output.

use std::ffi::OsStr;
use std::io::IsTerminal as _;
use std::time::Duration;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_BOLD_RED: &str = "\x1b[1;31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_BOLD_MAGENTA: &str = "\x1b[1;35m";

/// Styles human-readable output according to the target terminal and color environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputStyle {
    color: bool,
}

/// Human-readable status for one test or scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestStatus {
    Pass,
    ExpectedFailure,
    UnexpectedPass,
    Fail,
    Skip,
    Retry,
    Unknown,
}

impl TestStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::ExpectedFailure => "XFAIL",
            Self::UnexpectedPass => "XPASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
            Self::Retry => "RETRY",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// One command or quiet phase whose lifecycle is shown on standard error.
#[derive(Debug)]
pub(crate) struct Activity {
    description: String,
    finished: bool,
}

impl Activity {
    /// Prints an immediate loading line.
    pub(crate) fn start(description: impl Into<String>) -> Self {
        let activity = Self {
            description: description.into(),
            finished: false,
        };
        eprintln!(
            "{}",
            render_activity_line(
                ActivityState::Running,
                &activity.description,
                OutputStyle::stderr()
            )
        );
        activity
    }

    /// Prints the same description with a green check or red cross.
    pub(crate) fn finish(mut self, succeeded: bool) {
        let state = if succeeded {
            ActivityState::Succeeded
        } else {
            ActivityState::Failed
        };
        eprintln!(
            "{}",
            render_activity_line(state, &self.description, OutputStyle::stderr())
        );
        self.finished = true;
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if !self.finished {
            eprintln!(
                "{}",
                render_activity_line(
                    ActivityState::Failed,
                    &self.description,
                    OutputStyle::stderr(),
                )
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityState {
    Running,
    Succeeded,
    Failed,
}

impl OutputStyle {
    /// Resolves styling for standard output.
    #[must_use]
    pub(crate) fn stdout() -> Self {
        Self::resolve(std::io::stdout().is_terminal())
    }

    /// Resolves styling for standard error.
    #[must_use]
    pub(crate) fn stderr() -> Self {
        Self::resolve(std::io::stderr().is_terminal())
    }

    /// Styles informational text.
    #[must_use]
    pub(crate) fn info(self, text: &str) -> String {
        self.ansi(text, ANSI_CYAN)
    }

    /// Styles a prominent informational heading.
    #[must_use]
    pub(crate) fn heading(self, text: &str) -> String {
        self.ansi(text, ANSI_BOLD_CYAN)
    }

    /// Styles successful output.
    #[must_use]
    pub(crate) fn success(self, text: &str) -> String {
        self.ansi(text, ANSI_GREEN)
    }

    /// Styles a prominent success summary.
    #[must_use]
    pub(crate) fn success_heading(self, text: &str) -> String {
        self.ansi(text, ANSI_BOLD_GREEN)
    }

    /// Styles failure output.
    #[must_use]
    pub(crate) fn failure(self, text: &str) -> String {
        self.ansi(text, ANSI_RED)
    }

    /// Styles a prominent failure summary.
    #[must_use]
    pub(crate) fn failure_heading(self, text: &str) -> String {
        self.ansi(text, ANSI_BOLD_RED)
    }

    /// Styles warning or skipped output.
    #[must_use]
    pub(crate) fn warning(self, text: &str) -> String {
        self.ansi(text, ANSI_YELLOW)
    }

    /// Styles output whose result is unknown.
    #[must_use]
    pub(crate) fn unknown(self, text: &str) -> String {
        self.ansi(text, ANSI_MAGENTA)
    }

    /// Styles a prominent summary whose result is unknown.
    #[must_use]
    pub(crate) fn unknown_heading(self, text: &str) -> String {
        self.ansi(text, ANSI_BOLD_MAGENTA)
    }

    /// Renders one aligned, nextest-style test result line.
    #[must_use]
    pub(crate) fn test_result(
        self,
        status: TestStatus,
        name: &str,
        elapsed: Option<Duration>,
        position: Option<(usize, usize)>,
    ) -> String {
        let label = format!("{:>12}", status.label());
        let label = match status {
            TestStatus::Pass => self.success(&label),
            TestStatus::ExpectedFailure | TestStatus::Skip => self.warning(&label),
            TestStatus::UnexpectedPass | TestStatus::Fail => self.failure(&label),
            TestStatus::Retry => self.info(&label),
            TestStatus::Unknown => self.unknown(&label),
        };
        let elapsed = elapsed.map_or_else(String::new, |elapsed| {
            format!(" [{:>8.3}s]", elapsed.as_secs_f64())
        });
        let position = position.map_or_else(String::new, |(current, total)| {
            format!(" ({current}/{total})")
        });
        format!("{label}{elapsed}{position} {name}")
    }

    #[cfg(test)]
    pub(crate) const fn plain() -> Self {
        Self { color: false }
    }

    #[cfg(test)]
    pub(crate) const fn colored() -> Self {
        Self { color: true }
    }

    fn resolve(stream_is_terminal: bool) -> Self {
        Self {
            color: resolve_color(
                stream_is_terminal,
                std::env::var_os("NO_COLOR").is_some(),
                std::env::var_os("CARGO_TERM_COLOR").as_deref(),
            ),
        }
    }

    fn ansi(self, text: &str, style: &str) -> String {
        if self.color {
            format!("{style}{text}{ANSI_RESET}")
        } else {
            text.to_owned()
        }
    }
}

fn render_activity_line(state: ActivityState, description: &str, style: OutputStyle) -> String {
    let marker = match state {
        ActivityState::Running => style.info("⠋"),
        ActivityState::Succeeded => style.success("✓"),
        ActivityState::Failed => style.failure("✗"),
    };
    format!("{marker} {description}")
}

fn resolve_color(
    stream_is_terminal: bool,
    no_color: bool,
    cargo_term_color: Option<&OsStr>,
) -> bool {
    if no_color {
        return false;
    }
    match cargo_term_color.and_then(OsStr::to_str) {
        Some("always") => true,
        Some("never") => false,
        _ => stream_is_terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_policy_honors_terminal_cargo_and_no_color_settings() {
        assert!(resolve_color(true, false, None));
        assert!(!resolve_color(false, false, None));
        assert!(resolve_color(false, false, Some(OsStr::new("always"))));
        assert!(!resolve_color(true, false, Some(OsStr::new("never"))));
        assert!(!resolve_color(true, true, Some(OsStr::new("always"))));
    }

    #[test]
    fn plain_style_leaves_human_readable_output_unchanged() {
        let style = OutputStyle::plain();

        assert_eq!(style.info("Waiting"), "Waiting");
        assert_eq!(style.success("PASS"), "PASS");
        assert_eq!(style.failure("FAIL"), "FAIL");
        assert_eq!(style.warning("warning"), "warning");
        assert_eq!(style.unknown("UNKNOWN"), "UNKNOWN");
    }

    #[test]
    fn colored_style_uses_consistent_semantic_colors() {
        let style = OutputStyle::colored();

        assert_eq!(style.heading("Lane"), "\x1b[1;36mLane\x1b[0m");
        assert_eq!(style.success("PASS"), "\x1b[32mPASS\x1b[0m");
        assert_eq!(style.failure("FAIL"), "\x1b[31mFAIL\x1b[0m");
        assert_eq!(style.warning("SKIP"), "\x1b[33mSKIP\x1b[0m");
        assert_eq!(style.unknown("UNKNOWN"), "\x1b[35mUNKNOWN\x1b[0m");
    }

    #[test]
    fn activity_completion_repeats_the_loading_description_with_semantic_color() {
        let style = OutputStyle::colored();

        assert_eq!(
            render_activity_line(ActivityState::Running, "run probe", style),
            "\x1b[36m⠋\x1b[0m run probe"
        );
        assert_eq!(
            render_activity_line(ActivityState::Succeeded, "run probe", style),
            "\x1b[32m✓\x1b[0m run probe"
        );
        assert_eq!(
            render_activity_line(ActivityState::Failed, "run probe", style),
            "\x1b[31m✗\x1b[0m run probe"
        );
    }

    #[test]
    fn nextest_style_statuses_use_expected_result_colors() {
        let style = OutputStyle::colored();

        assert_eq!(
            style.test_result(
                TestStatus::Pass,
                "suite::passes",
                Some(Duration::from_millis(25)),
                Some((1, 4)),
            ),
            "\x1b[32m        PASS\x1b[0m [   0.025s] (1/4) suite::passes"
        );
        assert!(
            style
                .test_result(TestStatus::ExpectedFailure, "known", None, None)
                .starts_with("\x1b[33m       XFAIL\x1b[0m")
        );
        assert!(
            style
                .test_result(TestStatus::UnexpectedPass, "stale", None, None)
                .starts_with("\x1b[31m       XPASS\x1b[0m")
        );
        assert!(
            style
                .test_result(TestStatus::Fail, "unexpected", None, None)
                .starts_with("\x1b[31m        FAIL\x1b[0m")
        );
    }
}
