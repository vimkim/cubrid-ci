use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::time::Instant;

use crate::gha_collect::{CollectProgress, ProgressEvent};

pub struct TerminalProgress {
    enabled: bool,
    started: Instant,
    active: Mutex<bool>,
}

impl TerminalProgress {
    pub fn new(allow: bool) -> Self {
        Self {
            enabled: allow && io::stderr().is_terminal(),
            started: Instant::now(),
            active: Mutex::new(false),
        }
    }

    fn draw(&self, completed: usize, total: usize, detail: &str) {
        if !self.enabled {
            return;
        }
        let width = 20;
        let filled = if total == 0 {
            0
        } else {
            width * completed.min(total) / total
        };
        let bar = format!("{}{}", "#".repeat(filled), "-".repeat(width - filled));
        let elapsed = self.started.elapsed().as_secs();
        let minutes = elapsed / 60;
        let seconds = elapsed % 60;
        let mut stderr = io::stderr().lock();
        let _ = write!(
            stderr,
            "\r\x1b[2KCollecting [{bar}] {completed}/{total} · {detail} · {minutes:02}:{seconds:02}"
        );
        let _ = stderr.flush();
        if let Ok(mut active) = self.active.lock() {
            *active = true;
        }
    }

    fn clear(&self) {
        if !self.enabled {
            return;
        }
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[2K");
        let _ = stderr.flush();
        if let Ok(mut active) = self.active.lock() {
            *active = false;
        }
    }
}

impl CollectProgress for TerminalProgress {
    fn report(&self, event: ProgressEvent) {
        match event {
            ProgressEvent::Starting { total } => self.draw(0, total, "reading status"),
            ProgressEvent::StatusReady { total } => self.draw(0, total, "pinning executions"),
            ProgressEvent::Collecting { total } => self.draw(0, total, "collecting suites"),
            ProgressEvent::SuiteFinished {
                suite,
                completed,
                total,
            } => self.draw(completed, total, suite.job_name()),
            ProgressEvent::Verifying { total } => self.draw(total, total, "verifying PR head"),
            ProgressEvent::Finished => self.clear(),
        }
    }
}

impl Drop for TerminalProgress {
    fn drop(&mut self) {
        if self.active.lock().is_ok_and(|active| *active) {
            self.clear();
        }
    }
}
