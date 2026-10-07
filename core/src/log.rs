//! Shared log-severity classification for the log panel.
//!
//! Classification lives here, tested once, so rendering never re-sniffs
//! message prefixes. Levels:
//!
//! * [`Level::Error`] — failures, missing tools, panics, truncations.
//! * [`Level::Warn`] — warnings, cancellations.
//! * [`Level::Success`] — finished files, summaries, saved artifacts.
//! * [`Level::Skip`] — preflight skips and their reasons.
//! * [`Level::Command`] — auditable ffmpeg command echoes / dry-runs.
//! * [`Level::Info`] — everything else (scan/probe progress, headers).

/// Severity of one log line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warn,
    Success,
    Skip,
    Command,
    Info,
}

impl Level {
    /// Short badge text for log panels (`ERR`, `WARN`, `OK`, …).
    pub fn badge(&self) -> &'static str {
        match self {
            Level::Error => "ERR",
            Level::Warn => "WARN",
            Level::Success => "OK",
            Level::Skip => "SKIP",
            Level::Command => "CMD",
            Level::Info => "INFO",
        }
    }

    /// All levels in chip order.
    pub fn all() -> [Level; 6] {
        [
            Level::Error,
            Level::Warn,
            Level::Success,
            Level::Skip,
            Level::Command,
            Level::Info,
        ]
    }
}

/// One timestamped, pre-classified log line. Frontends store these
/// instead of raw strings so timestamps, badges and filters never
/// re-parse text at render time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub level: Level,
    pub text: String,
    pub at: std::time::SystemTime,
}

impl LogEntry {
    pub fn new(text: String) -> Self {
        let level = classify(&text);
        Self {
            level,
            text,
            at: std::time::SystemTime::now(),
        }
    }

    /// `HH:MM:SS` (UTC) stamp for log panels.
    pub fn stamp(&self) -> String {
        let secs = self
            .at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            % 86_400;
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}

/// Toggle state for the six severity lanes, indexed by `Level as usize`.
/// Both frontends keep one of these next to their log vec.
#[derive(Clone, Copy, Debug)]
pub struct LevelFilter(pub [bool; 6]);

impl LevelFilter {
    pub fn all() -> Self {
        Self([true; 6])
    }

    pub fn visible(&self, level: Level) -> bool {
        self.0[level as usize]
    }

    pub fn toggle(&mut self, level: Level) {
        let i = level as usize;
        self.0[i] = !self.0[i];
    }

    pub fn any_hidden(&self) -> bool {
        self.0.contains(&false)
    }
}

/// Classify one log line. Prefix checks run on leading-whitespace-trimmed
/// text (worker messages are often indented with two spaces); `contains`
/// checks run on the lowercased line. Error is tested first so a line like
/// "skip …: probe failed" still lands in red.
pub fn classify(line: &str) -> Level {
    let t = line.trim_start();
    let l = line.to_lowercase();
    let starts = |p: &str| t.to_lowercase().starts_with(p);

    if starts("error")
        || starts("⚠")
        || l.contains("fail")
        || l.contains("missing")
        || l.contains("panic")
        || l.contains("truncated")
        || l.contains("not supported")
    {
        return Level::Error;
    }
    if starts("warn") || l.contains("warn") || l.contains("cancel") {
        return Level::Warn;
    }
    if starts("done")
        || starts("recommended")
        || l.contains("csv saved")
        || l.contains("saved to")
        || l.contains("freed ")
    {
        return Level::Success;
    }
    if starts("✗") || l.contains("skip") {
        return Level::Skip;
    }
    if starts("cmd") || l.contains("cmd[") || l.contains("dry-run") {
        return Level::Command;
    }
    Level::Info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_win_over_other_signals() {
        assert_eq!(classify("ERROR: ffmpeg not found on PATH."), Level::Error);
        assert_eq!(
            classify("⚠ REQUIRED TOOLS MISSING: ffmpeg=MISSING"),
            Level::Error
        );
        assert_eq!(classify("  FAIL: cuda init"), Level::Error);
        assert_eq!(classify("warn: worker panic"), Level::Error);
        assert_eq!(
            classify("output truncated (12.0s vs 600.0s source)"),
            Level::Error
        );
        // A skip line that also mentions failure stays red.
        assert_eq!(classify("skip x: probe failed"), Level::Error);
    }

    #[test]
    fn warns_cover_cancels() {
        assert_eq!(classify("probe warn: odd file"), Level::Warn);
        assert_eq!(classify("warn: something off"), Level::Warn);
        assert_eq!(classify("Cancelled."), Level::Warn);
    }

    #[test]
    fn successes_cover_all_completions() {
        // The old UI only greened lowercase "done…"; capital-D summaries
        // ("Done in 42s. Freed …") must land here too.
        assert_eq!(
            classify("done movie.mkv: 1.0 GB → 600.0 MB (60%) in 12s, …"),
            Level::Success
        );
        assert_eq!(
            classify("Done in 42s. Freed 12.0 GB. Re-scan to confirm."),
            Level::Success
        );
        assert_eq!(
            classify("Recommended default: B:nvenc-p3 (…)"),
            Level::Success
        );
        assert_eq!(
            classify("benchmark CSV saved to C:\\out.csv"),
            Level::Success
        );
    }

    #[test]
    fn convert_summaries_color_by_outcome() {
        // The clean-run summary must dodge the word "fail" (contains-check
        // below puts any such line in the red lane) and lead with "done".
        assert_eq!(classify("done: 1 file(s) converted in 0s."), Level::Success);
        assert_eq!(
            classify("Convert done in 2s: 1 converted, 2 failed."),
            Level::Error
        );
    }

    #[test]
    fn skips_and_commands_have_their_own_lanes() {
        assert_eq!(
            classify("  ✗ movie.mp4: already hevc — negligible"),
            Level::Skip
        );
        assert_eq!(classify("SKIP out.mkv: reason"), Level::Skip);
        assert_eq!(
            classify("Probed 10 files (3 would skip at 10% threshold)."),
            Level::Skip
        );
        assert_eq!(classify("cmd[0]: ffmpeg -y -hide_banner …"), Level::Command);
        assert_eq!(classify("[dry-run A:x264] ffmpeg …"), Level::Command);
    }

    #[test]
    fn everything_else_is_info() {
        assert_eq!(
            classify("Add folders and/or files, then Scan."),
            Level::Info
        );
        assert_eq!(classify("Scanning 2 selected item(s) …"), Level::Info);
        assert_eq!(
            classify("Shrink pressed but 0 files eligible — reasons:"),
            Level::Info
        );
        assert_eq!(
            classify("  hint: uncheck 'Skip already-efficient files'."),
            Level::Skip
        );
        assert_eq!(
            classify("Executing 4 files via Auto+p5 cq28 …"),
            Level::Info
        );
    }

    #[test]
    fn entries_stamp_and_filter() {
        let e = LogEntry::new("Done in 42s. Freed 1 GB.".into());
        assert_eq!(e.level, Level::Success);
        assert_eq!(e.level.badge(), "OK");
        let s = e.stamp();
        assert_eq!(s.len(), 8, "stamp is HH:MM:SS, got {s}");
        assert_eq!(s.chars().nth(2), Some(':'));
        let mut f = LevelFilter::all();
        assert!(f.visible(Level::Error));
        assert!(!f.any_hidden());
        f.toggle(Level::Skip);
        assert!(!f.visible(Level::Skip));
        assert!(f.visible(Level::Info));
        assert!(f.any_hidden());
        assert_eq!(Level::all().len(), 6);
    }
}
