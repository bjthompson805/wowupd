//! How a long run tells its caller what it is doing: notes, and progress with speed and time left.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    /// Copying a Battle.net store into wowupd's own.
    Copying,
}

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    /// 0 when unknown.
    pub total: u64,
    /// Bytes per second over the last few seconds.
    pub rate: f64,
    pub finished: bool,
}

impl Progress {
    pub fn fraction(&self) -> f64 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64).min(1.0) }
    }

    pub fn left(&self) -> Option<Duration> {
        (self.total > self.done && self.rate > 1.0).then(|| Duration::from_secs_f64((self.total - self.done) as f64 / self.rate))
    }

    /// "1.23 / 2.64 GiB · 18.4 MiB/s · 1m 12s left"
    pub fn describe(&self) -> String {
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        let mut s = if self.total > 0 { format!("{:.2} / {:.2} GiB", gib(self.done), gib(self.total)) } else { format!("{:.2} GiB", gib(self.done)) };
        if !self.finished {
            s += &format!(" · {:.1} MiB/s", self.rate / (1u64 << 20) as f64);
            if let Some(left) = self.left() {
                s += &format!(" · {} left", duration(left));
            }
        }
        s
    }
}

pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

pub enum Event {
    Note(String),
    Progress(Progress),
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

/// Turns a running byte count into progress events with a recent rate, at most a few per second.
pub struct Meter {
    phase: Phase,
    total: u64,
    samples: VecDeque<(Instant, u64)>,
    last_sent: Option<Instant>,
}

const WINDOW: Duration = Duration::from_secs(5);
const INTERVAL: Duration = Duration::from_millis(500);

impl Meter {
    pub fn new(phase: Phase, total: u64) -> Meter {
        Meter { phase, total, samples: VecDeque::from([(Instant::now(), 0)]), last_sent: None }
    }

    fn progress(&mut self, done: u64, finished: bool) -> Progress {
        let now = Instant::now();
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > WINDOW {
            self.samples.pop_front();
        }
        let (t0, d0) = self.samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        let rate = if dt > 0.0 { (done.saturating_sub(d0)) as f64 / dt } else { 0.0 };
        Progress { phase: self.phase, done, total: self.total, rate, finished }
    }

    /// Sends progress if enough time has passed since the last.
    pub fn update(&mut self, done: u64, report: &Reporter) {
        if self.last_sent.is_some_and(|t| t.elapsed() < INTERVAL) {
            return;
        }
        self.last_sent = Some(Instant::now());
        let p = self.progress(done, false);
        report(Event::Progress(p));
    }

    pub fn finish(&mut self, done: u64, report: &Reporter) {
        let p = self.progress(done, true);
        report(Event::Progress(p));
    }
}
