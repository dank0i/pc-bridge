//! Live Steam download percentage, derived from `logs/content_log.txt`.
//!
//! # Why this and not the manifest
//!
//! `BytesDownloaded / BytesToDownload` from the appmanifest is the *correct*
//! number, and it is what Steam's own UI shows, but it is written far too rarely
//! to drive a progress bar. Measured on a real 112 GB install: frozen at 40.82%
//! for over three minutes, then straight to 100% in one step. On another it did
//! not move at all until the download was paused.
//!
//! # Why not the disk
//!
//! Two disk-based approaches were built and both failed on real data. NTFS Valid
//! Data Length over the staging tree reached 99.81% while the true figure was
//! 39%, because a reuse-heavy patch reconstructs most of the install from bytes
//! already on disk (one measured update: 13.55 GB downloaded against 95 GB
//! reused). Anything measuring disk writes counts that reuse; the percentage does
//! not. `steam.exe`'s read counter is worse still, running 7x the download.
//!
//! # What this does
//!
//! `content_log.txt` carries everything needed, in real time:
//!
//! * `AppID <id> update started : download a/b, ...` - authoritative anchor and
//!   denominator, written the moment a download begins.
//! * `Current download rate: N Mbps` and `... rate was X, now Y` - Steam's own
//!   measurement of *this* download, several times a minute.
//! * `AppID <id> App update changed : Running Update,Downloading,Staging,` -
//!   Steam naming its own phase, so we stop inferring it from byte counters.
//! * `AppID <id> state changed : Fully Installed,` - completion.
//!
//! Anchor, then integrate the rate forward with a zero-order hold. Whenever the
//! manifest *does* flush, snap onto it, so every correction pulls the estimate
//! back to exact.
//!
//! # Accuracy
//!
//! Measured against a manifest flush mid-download: estimate 20.84% against a true
//! 22.29%, so **1.45 points low**. It errs low by design preference: a bar that
//! lags slightly and catches up reads as honest, while one that saturates at 100%
//! with the download still running reads as broken. Error scales with how
//! volatile the transfer rate is, since the rate is held constant between
//! readings.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// Bytes per second from a rate reported in megabits per second.
///
/// Base 10, not 2^20: verified against a `stats:` line reporting 16013699648
/// bytes over 187 s as "686.22 Mbps" (16013699648 / 187 = 85.6 MB/s).
fn mbps_to_bytes_per_sec(mbps: f64) -> f64 {
    mbps * 1_000_000.0 / 8.0
}

/// A line from content_log.txt that we care about.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEvent {
    /// A download began: (app_id, bytes_already_done, bytes_total).
    Anchor(String, u64, u64),
    /// Steam's current transfer rate, in bytes/sec. Carries no app id, which is
    /// safe because Steam downloads one app at a time and `Anchor` names it.
    Rate(f64),
    /// Steam's own phase words, e.g. "Running Update,Downloading,Staging".
    Phase(String, String),
    /// The app settled to Fully Installed.
    Finished(String),
}

/// Read whatever is new in the log since `offset`.
///
/// Returns the new offset and the events found. Does blocking file I/O, so call
/// it from `spawn_blocking`. Never returns an error: an unreadable or missing log
/// simply yields no events and the estimator falls back to the manifest.
pub fn read_new_events(path: &Path, offset: u64) -> (u64, Vec<LogEvent>) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let Ok(meta) = std::fs::metadata(path) else {
        return (offset, Vec::new());
    };
    let len = meta.len();
    // Rotation: the file shrank, so our offset is meaningless. Start over rather
    // than seeking past the end and going permanently silent.
    let mut from = if len < offset { 0 } else { offset };

    if len == from {
        return (from, Vec::new());
    }

    let Ok(file) = std::fs::File::open(path) else {
        return (offset, Vec::new());
    };
    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(from)).is_err() {
        return (offset, Vec::new());
    }

    let mut events = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Stop BEFORE a line Steam is still writing. Consuming it would
                // advance the offset past a truncated line, so the real line is
                // lost when it lands, and a half-written anchor yields a tiny
                // bogus denominator that pins the bar at 100%.
                if !buf.ends_with(b"\n") {
                    break;
                }
                from += n as u64;
                // Lossy rather than strict: one invalid byte (game names in this
                // log are not always valid UTF-8) used to error out WITHOUT
                // advancing the offset, wedging the reader on that byte forever.
                if let Some(ev) = parse_line(&String::from_utf8_lossy(&buf)) {
                    events.push(ev);
                }
            }
            Err(_) => break,
        }
    }
    (from, events)
}

/// Match one log line. Deliberately string-based rather than regex: the patterns
/// are fixed, and this avoids pulling a regex crate in for four matches.
fn parse_line(line: &str) -> Option<LogEvent> {
    // "AppID 2767030 update started : download 0/14553042736, store 0/0, ..."
    if let Some(rest) = line.split_once("AppID ").map(|(_, r)| r) {
        let mut it = rest.splitn(2, ' ');
        let app_id = it.next()?;
        if !app_id.bytes().all(|b| b.is_ascii_digit()) || app_id.is_empty() {
            return None;
        }
        let tail = it.next()?;

        if let Some(after) = tail
            .split_once("update started : download ")
            .map(|(_, r)| r)
        {
            let frag = after.split(',').next()?;
            let (done, total) = frag.split_once('/')?;
            let done: u64 = done.trim().parse().ok()?;
            let total: u64 = total.trim().parse().ok()?;
            // Steam sometimes emits a zero-total line immediately before the real
            // one. Observed live; accepting it makes the denominator zero and the
            // bar meaningless.
            if total == 0 {
                return None;
            }
            return Some(LogEvent::Anchor(app_id.to_string(), done, total));
        }

        if let Some(after) = tail.split_once("App update changed : ").map(|(_, r)| r) {
            return Some(LogEvent::Phase(
                app_id.to_string(),
                after.trim().trim_end_matches(',').to_string(),
            ));
        }

        if let Some(after) = tail.split_once("state changed : ").map(|(_, r)| r) {
            let s = after.trim().trim_end_matches(',');
            if s == "Fully Installed" {
                return Some(LogEvent::Finished(app_id.to_string()));
            }
        }
        return None;
    }

    // "Current download rate: 434.783 Mbps"
    if let Some(after) = line.split_once("Current download rate: ").map(|(_, r)| r) {
        let n = after.split_whitespace().next()?;
        let mbps: f64 = n.parse().ok()?;
        return Some(LogEvent::Rate(mbps_to_bytes_per_sec(mbps)));
    }

    // "Increasing target number of download connections to 11 (rate was 262.262, now 434.783)"
    // These arrive more often than the periodic rate line, so they matter.
    if line.contains("rate was ")
        && let Some(after) = line.split_once(", now ").map(|(_, r)| r)
    {
        let n = after.trim().trim_end_matches(')');
        let mbps: f64 = n.parse().ok()?;
        return Some(LogEvent::Rate(mbps_to_bytes_per_sec(mbps)));
    }

    None
}

/// Tracks one download and produces a live percentage for it.
pub struct DownloadEstimator {
    log_path: PathBuf,
    offset: u64,
    /// The app the log is currently talking about.
    app_id: Option<String>,
    total_bytes: u64,
    est_bytes: f64,
    rate_bps: f64,
    /// When the last Rate event arrived, so a stale rate can be retired.
    rate_at: Option<Instant>,
    last_advance: Instant,
    phase: Option<String>,
}

impl DownloadEstimator {
    pub fn new(steam_root: &Path) -> Self {
        Self {
            log_path: steam_root.join("logs").join("content_log.txt"),
            offset: 0,
            app_id: None,
            total_bytes: 0,
            est_bytes: 0.0,
            rate_bps: 0.0,
            rate_at: None,
            last_advance: Instant::now(),
            phase: None,
        }
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Seek to the end without replaying history, then look back over the tail for
    /// the most recent anchor so a mid-download restart still has a denominator.
    pub fn prime(&mut self, len: u64, recent: &[LogEvent]) {
        for ev in recent {
            self.apply(ev.clone());
        }
        self.offset = len;
        self.last_advance = Instant::now();
    }

    pub fn apply(&mut self, ev: LogEvent) {
        match ev {
            LogEvent::Anchor(app, done, total) => {
                self.app_id = Some(app);
                self.total_bytes = total;
                self.est_bytes = done as f64;
                self.rate_bps = 0.0;
                self.rate_at = None;
                self.phase = None;
                self.last_advance = Instant::now();
            }
            LogEvent::Rate(bps) => {
                self.rate_bps = bps;
                self.rate_at = Some(Instant::now());
            }
            LogEvent::Phase(app, words) => {
                if self.app_id.as_deref() == Some(app.as_str()) {
                    self.phase = if words.eq_ignore_ascii_case("None") {
                        None
                    } else {
                        Some(words)
                    };
                }
            }
            LogEvent::Finished(app) => {
                if self.app_id.as_deref() == Some(app.as_str()) {
                    self.est_bytes = self.total_bytes as f64;
                    self.rate_bps = 0.0;
                    self.rate_at = None;
                    self.phase = None;
                }
            }
        }
    }

    pub fn set_offset(&mut self, offset: u64) {
        self.offset = offset;
    }

    /// Advance the estimate by `rate * elapsed`. Call once per tick.
    pub fn advance(&mut self) {
        /// Longest gap integrated in one step. Without this, any stall (a pause
        /// disables the tick, an MQTT reconnect, the PC sleeping) is integrated at
        /// the last held rate on resume: at a measured 434 Mbps, 268 seconds of
        /// gap is enough to saturate a 14.5 GB download to 100%.
        const MAX_STEP_SECS: f64 = 6.0;
        /// A rate this old is assumed dead. Steam reports several times a minute
        /// while downloading, so silence means it stopped.
        const RATE_STALE_SECS: f64 = 60.0;

        let now = Instant::now();
        let dt = now
            .duration_since(self.last_advance)
            .as_secs_f64()
            .min(MAX_STEP_SECS);
        self.last_advance = now;

        if self
            .rate_at
            .is_none_or(|t| now.duration_since(t).as_secs_f64() > RATE_STALE_SECS)
        {
            self.rate_bps = 0.0;
        }
        if self.total_bytes == 0 || dt <= 0.0 {
            return;
        }
        self.est_bytes = (self.est_bytes + self.rate_bps * dt).min(self.total_bytes as f64);
    }

    /// Pull the estimate onto a manifest reading. The manifest is exact but rare,
    /// so every flush is a chance to erase accumulated drift.
    ///
    /// Only ever moves the estimate FORWARD: a stale manifest value must not drag
    /// a correctly-advanced bar backwards.
    pub fn snap_to(&mut self, app_id: &str, downloaded: u64, total: u64) {
        if self.app_id.as_deref() != Some(app_id) || total == 0 {
            return;
        }
        if total != self.total_bytes {
            self.total_bytes = total;
        }
        let exact = downloaded.min(total) as f64;
        if exact > self.est_bytes {
            self.est_bytes = exact;
        }
    }

    /// Live percentage for `app_id`, if this estimator is tracking it.
    pub fn percent_for(&self, app_id: &str) -> Option<f64> {
        if self.app_id.as_deref() != Some(app_id) || self.total_bytes == 0 {
            return None;
        }
        Some(((self.est_bytes / self.total_bytes as f64) * 100.0).clamp(0.0, 100.0))
    }

    /// Steam's own phase word mapped onto the dashboard's vocabulary.
    ///
    /// Steam reports a comma-joined set like "Running Update,Downloading,Staging".
    /// Ordered most-specific first, because Downloading and Staging overlap while
    /// bytes are still arriving and the download is the more useful label then.
    pub fn phase_for(&self, app_id: &str) -> Option<&'static str> {
        if self.app_id.as_deref() != Some(app_id) {
            return None;
        }
        let words = self.phase.as_deref()?;
        let has = |needle: &str| words.contains(needle);
        Some(if has("Downloading") {
            "downloading"
        } else if has("Committing") {
            "committing"
        } else if has("Staging") {
            "staging"
        } else if has("Verifying") {
            "verifying"
        } else if has("Preallocating") {
            "preallocating"
        } else if has("Reconfiguring") {
            "reconfiguring"
        } else {
            return None;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anchor() {
        let line = "[2026-07-25 12:55:52] AppID 2767030 update started : download 0/14553042736, store 0/0, reuse 0/102092625121, delta 0/7009128625, stage 0/120764988236";
        assert_eq!(
            parse_line(line),
            Some(LogEvent::Anchor("2767030".into(), 0, 14_553_042_736))
        );
    }

    #[test]
    fn parses_anchor_with_progress_already_done() {
        let line = "[t] AppID 730 update started : download 1024/2048, store 0/0";
        assert_eq!(
            parse_line(line),
            Some(LogEvent::Anchor("730".into(), 1024, 2048))
        );
    }

    #[test]
    fn rejects_zero_total_anchor() {
        // Observed live: Steam emits a zero-total line immediately before the real
        // one, and accepting it made the denominator zero and sent the bar to 100%.
        let line = "[t] AppID 1091500 update started : download 0/0, store 0/0";
        assert_eq!(parse_line(line), None);
    }

    #[test]
    fn parses_periodic_rate() {
        let line = "[2026-07-25 12:56:59] Current download rate: 434.783 Mbps";
        match parse_line(line) {
            Some(LogEvent::Rate(bps)) => {
                // 434.783 Mbps base-10 => ~54.3 MB/s
                assert!((bps - 54_347_875.0).abs() < 1000.0, "got {bps}");
            }
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn parses_connection_change_rate() {
        // These arrive more often than the periodic line, so they must be caught.
        let line = "[t] Increasing target number of download connections to 11 (rate was 262.262, now 434.783)";
        match parse_line(line) {
            Some(LogEvent::Rate(bps)) => {
                assert!((bps - 54_347_875.0).abs() < 1000.0, "got {bps}");
            }
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn parses_phase_and_completion() {
        assert_eq!(
            parse_line(
                "[t] AppID 2767030 App update changed : Running Update,Downloading,Staging,"
            ),
            Some(LogEvent::Phase(
                "2767030".into(),
                "Running Update,Downloading,Staging".into()
            ))
        );
        assert_eq!(
            parse_line("[t] AppID 2767030 state changed : Fully Installed,"),
            Some(LogEvent::Finished("2767030".into()))
        );
    }

    #[test]
    fn ignores_noise() {
        assert_eq!(parse_line("[t] Moving to source priority class '6'"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(
            parse_line("AppID notanumber update started : download 1/2"),
            None
        );
        // A non-terminal state change must not read as completion.
        assert_eq!(
            parse_line("[t] AppID 730 state changed : Fully Installed,Update Queued,"),
            None
        );
    }

    fn est_with_anchor() -> DownloadEstimator {
        let mut e = DownloadEstimator::new(Path::new("C:\\Steam"));
        e.apply(LogEvent::Anchor("730".into(), 0, 1000));
        e
    }

    #[test]
    fn percent_none_for_unknown_app() {
        let e = est_with_anchor();
        assert!(e.percent_for("999").is_none());
    }

    #[test]
    fn advance_integrates_rate() {
        let mut e = est_with_anchor();
        e.apply(LogEvent::Rate(100.0)); // 100 bytes/sec
        std::thread::sleep(std::time::Duration::from_millis(50));
        e.advance();
        let pct = e.percent_for("730").expect("tracking");
        assert!(
            pct > 0.0 && pct < 5.0,
            "expected a small advance, got {pct}"
        );
    }

    #[test]
    fn advance_never_exceeds_total() {
        let mut e = est_with_anchor();
        e.apply(LogEvent::Rate(1_000_000_000.0));
        std::thread::sleep(std::time::Duration::from_millis(20));
        e.advance();
        assert!((e.percent_for("730").expect("tracking") - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn snap_corrects_drift_forward_only() {
        let mut e = est_with_anchor();
        e.snap_to("730", 500, 1000);
        assert!((e.percent_for("730").expect("tracking") - 50.0).abs() < 0.001);
        // A stale, lower manifest reading must not drag the bar backwards.
        e.snap_to("730", 100, 1000);
        assert!((e.percent_for("730").expect("tracking") - 50.0).abs() < 0.001);
    }

    #[test]
    fn snap_ignores_other_apps_and_zero_totals() {
        let mut e = est_with_anchor();
        e.snap_to("999", 900, 1000);
        e.snap_to("730", 900, 0);
        assert!((e.percent_for("730").expect("tracking") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn finished_pins_to_full() {
        let mut e = est_with_anchor();
        e.apply(LogEvent::Finished("730".into()));
        assert!((e.percent_for("730").expect("tracking") - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn phase_prefers_downloading_over_staging() {
        // Steam reports both while bytes are still arriving; downloading is the
        // more useful label at that moment.
        let mut e = est_with_anchor();
        e.apply(LogEvent::Phase(
            "730".into(),
            "Running Update,Downloading,Staging".into(),
        ));
        assert_eq!(e.phase_for("730"), Some("downloading"));
    }

    #[test]
    fn phase_maps_steam_words() {
        let cases = [
            ("Running Update,Staging", Some("staging")),
            ("Running Update,Committing", Some("committing")),
            ("Running Update,Verifying Installed", Some("verifying")),
            ("Running Update,Preallocating", Some("preallocating")),
            ("Running Update,Reconfiguring", Some("reconfiguring")),
            ("Running Update", None),
        ];
        for (words, want) in cases {
            let mut e = est_with_anchor();
            e.apply(LogEvent::Phase("730".into(), words.into()));
            assert_eq!(e.phase_for("730"), want, "for {words}");
        }
    }

    #[test]
    fn phase_cleared_by_none() {
        let mut e = est_with_anchor();
        e.apply(LogEvent::Phase(
            "730".into(),
            "Running Update,Staging".into(),
        ));
        e.apply(LogEvent::Phase("730".into(), "None".into()));
        assert_eq!(e.phase_for("730"), None);
    }

    #[test]
    fn anchor_switches_app_and_resets() {
        let mut e = est_with_anchor();
        e.snap_to("730", 900, 1000);
        e.apply(LogEvent::Anchor("440".into(), 0, 2000));
        assert!(e.percent_for("730").is_none(), "old app no longer tracked");
        assert!((e.percent_for("440").expect("tracking") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn partial_trailing_line_is_not_consumed() {
        // Steam writes the log while we read it. Consuming a half-written line
        // advances the offset past it, so the real line is lost when it lands, and
        // a truncated anchor yields a tiny denominator that pins the bar at 100%.
        let dir = std::env::temp_dir().join(format!("pcb_partial_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("content_log.txt");
        let full = "[t] AppID 730 update started : download 0/2048, store 0/0\n";
        let cut = &full[..40]; // mid-number, no newline
        std::fs::write(&f, cut).expect("write");

        let (off, evs) = read_new_events(&f, 0);
        assert_eq!(off, 0, "offset must not advance past a partial line");
        assert!(evs.is_empty(), "must not emit a truncated anchor");

        // Now the rest arrives; the whole line must be picked up intact.
        std::fs::write(&f, full).expect("write");
        let (off2, evs2) = read_new_events(&f, off);
        assert_eq!(off2, full.len() as u64);
        assert_eq!(evs2, vec![LogEvent::Anchor("730".into(), 0, 2048)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_does_not_wedge_the_reader() {
        // A single bad byte used to make read_line error WITHOUT advancing the
        // offset, so every later call re-read the same byte and returned nothing,
        // permanently. Meanwhile advance() kept integrating the last rate to 100%.
        let dir = std::env::temp_dir().join(format!("pcb_utf8_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("content_log.txt");
        let mut bytes = b"[t] Game \xFF\xFE name\n".to_vec();
        bytes.extend_from_slice(b"[t] AppID 730 update started : download 0/2048, store 0/0\n");
        std::fs::write(&f, &bytes).expect("write");

        let (off, evs) = read_new_events(&f, 0);
        assert_eq!(off, bytes.len() as u64, "must consume past the bad bytes");
        assert_eq!(evs, vec![LogEvent::Anchor("730".into(), 0, 2048)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_restarts_from_zero() {
        let dir = std::env::temp_dir().join(format!("pcb_rot_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("content_log.txt");
        std::fs::write(&f, "[t] AppID 730 update started : download 0/2048, x\n").expect("write");
        let (_, evs) = read_new_events(&f, 999_999);
        assert_eq!(evs.len(), 1, "a shrunk file must be re-read from the start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_rate_stops_advancing() {
        // A pause disables the tick while rate_bps keeps its last value. Without
        // expiry, resuming integrates the whole paused duration in one step: at a
        // measured 434 Mbps, 268 seconds saturates a 14.5 GB download.
        let mut e = est_with_anchor();
        e.apply(LogEvent::Rate(1_000_000.0));
        // Backdate the rate beyond the staleness window.
        e.rate_at = Some(Instant::now() - std::time::Duration::from_secs(120));
        e.last_advance = Instant::now() - std::time::Duration::from_secs(120);
        e.advance();
        assert!(
            (e.percent_for("730").expect("tracking") - 0.0).abs() < f64::EPSILON,
            "a stale rate must not integrate"
        );
    }

    #[test]
    fn advance_step_is_capped() {
        // Even with a live rate, one enormous wall-clock gap must not be
        // integrated wholesale (PC sleep, MQTT reconnect after hours).
        let mut e = DownloadEstimator::new(Path::new("C:\\Steam"));
        e.apply(LogEvent::Anchor("730".into(), 0, 1_000_000));
        e.apply(LogEvent::Rate(1000.0)); // 1000 B/s
        e.last_advance = Instant::now() - std::time::Duration::from_secs(3600);
        e.advance();
        // 3600s at 1000 B/s would be 3.6 MB, far past the 1 MB total. The cap
        // keeps it to a few seconds' worth.
        let pct = e.percent_for("730").expect("tracking");
        assert!(pct < 1.0, "expected a capped step, got {pct}");
    }

    #[test]
    fn missing_log_yields_no_events() {
        let (off, evs) = read_new_events(Path::new("/definitely/not/here.txt"), 42);
        assert_eq!(off, 42);
        assert!(evs.is_empty());
    }
}
