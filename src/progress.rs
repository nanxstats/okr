//! Interactive progress rendering for long-running CLI workflows.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

const TICK_INTERVAL: Duration = Duration::from_millis(100);
const TICK_STRINGS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Debug)]
pub(crate) struct SyncProgress {
    inner: Arc<ProgressState>,
}

#[derive(Debug)]
struct ProgressState {
    multi: MultiProgress,
    overall: ProgressBar,
    suppressed: bool,
}

impl SyncProgress {
    pub(crate) fn new(entries: usize, quiet: bool, verbose: bool) -> Self {
        let suppressed = quiet || verbose;
        let total = entries.saturating_mul(2).saturating_add(3) as u64;
        let multi = MultiProgress::with_draw_target(draw_target(suppressed));
        let overall = multi.add(ProgressBar::with_draw_target(
            Some(total),
            draw_target(suppressed),
        ));
        overall.set_style(overall_style());
        overall.set_message("Starting synchronization...");

        Self {
            inner: Arc::new(ProgressState {
                multi,
                overall,
                suppressed,
            }),
        }
    }

    pub(crate) fn set_phase(&self, message: impl Into<String>) {
        self.inner.overall.set_message(message.into());
        self.inner.overall.tick();
    }

    pub(crate) fn entry(&self, action: &str, name: &str) -> EntryProgress {
        let progress = self.inner.multi.insert_before(
            &self.inner.overall,
            ProgressBar::with_draw_target(None, draw_target(self.inner.suppressed)),
        );
        progress.set_style(spinner_style());
        progress.set_message(format!("{action} {name}"));
        progress.enable_steady_tick(TICK_INTERVAL);
        EntryProgress { progress }
    }

    pub(crate) fn advance(&self, message: impl Into<String>) {
        self.inner.overall.set_message(message.into());
        self.inner.overall.inc(1);
    }

    pub(crate) fn download(&self, label: &str, length: Option<u64>) -> TransferProgress {
        let progress = self.inner.multi.insert_before(
            &self.inner.overall,
            ProgressBar::with_draw_target(length, draw_target(self.inner.suppressed)),
        );
        match length {
            Some(_) => progress.set_style(download_style()),
            None => {
                progress.set_style(download_spinner_style());
                progress.enable_steady_tick(TICK_INTERVAL);
            }
        }
        progress.set_message(label.to_owned());
        TransferProgress { progress }
    }

    pub(crate) fn finish(&self) {
        self.inner.overall.set_message("");
        self.inner.overall.finish_and_clear();
        let _ = self.inner.multi.clear();
    }
}

impl Drop for SyncProgress {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.finish();
        }
    }
}

pub(crate) struct EntryProgress {
    progress: ProgressBar,
}

impl EntryProgress {
    pub(crate) fn finish(&self) {
        self.progress.finish_and_clear();
    }
}

impl Drop for EntryProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(crate) struct TransferProgress {
    progress: ProgressBar,
}

impl TransferProgress {
    pub(crate) fn wrap_read<R: Read>(&self, reader: R) -> impl Read {
        self.progress.wrap_read(reader)
    }

    pub(crate) fn finish(&self) {
        self.progress.finish_and_clear();
    }
}

impl Drop for TransferProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

fn draw_target(suppressed: bool) -> ProgressDrawTarget {
    if suppressed {
        ProgressDrawTarget::hidden()
    } else {
        ProgressDrawTarget::stderr()
    }
}

fn overall_style() -> ProgressStyle {
    ProgressStyle::with_template("{bar:20.cyan/blue} [{pos}/{len}] {wide_msg:.dim}")
        .expect("static overall progress template is valid")
        .progress_chars("━╸ ")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {wide_msg:.dim}")
        .expect("static spinner progress template is valid")
        .tick_strings(TICK_STRINGS)
}

fn download_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{bar:24.cyan/blue} {binary_bytes:>7}/{binary_total_bytes:7} {wide_msg:.dim}",
    )
    .expect("static download progress template is valid")
    .progress_chars("━╸ ")
}

fn download_spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {binary_bytes:>7} {wide_msg:.dim}")
        .expect("static download spinner template is valid")
        .tick_strings(TICK_STRINGS)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::SyncProgress;

    #[test]
    fn overall_progress_counts_both_entry_phases_and_finalization() {
        let progress = SyncProgress::new(2, true, false);
        assert_eq!(progress.inner.overall.length(), Some(7));
        progress.advance("resolved one");
        progress.advance("resolved two");
        assert_eq!(progress.inner.overall.position(), 2);
        progress.finish();
    }

    #[test]
    fn quiet_and_verbose_modes_suppress_progress() {
        assert!(SyncProgress::new(1, true, false).inner.multi.is_hidden());
        assert!(SyncProgress::new(1, false, true).inner.multi.is_hidden());
    }

    #[test]
    fn transfer_progress_tracks_bytes_read() {
        let progress = SyncProgress::new(1, true, false);
        let transfer = progress.download("fixture", Some(7));
        let mut reader = transfer.wrap_read(Cursor::new(b"fixture"));
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();

        assert_eq!(bytes, b"fixture");
        assert_eq!(transfer.progress.position(), 7);
    }
}
