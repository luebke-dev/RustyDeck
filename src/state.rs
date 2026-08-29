//! Polling the state commands of the keys on the current page.
//!
//! The commands run on a thread of their own: a slow or hanging command must
//! never stall the key handling. Each poll reports which case of a key
//! currently matches; the daemon only redraws when that index changes.

use crate::config::{Command, StateSpec};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

/// What a key's state command reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateUpdate {
    pub key: u8,
    /// Index of the first matching case.
    pub case: usize,
    /// Output of the command, so templates can show it.
    pub stdout: String,
    pub exit: Option<i32>,
}

/// One key being watched.
struct Watch {
    key: u8,
    run: Command,
    interval: Duration,
    /// Conditions in configured order, so the first match wins.
    cases: Vec<CaseCondition>,
    next_poll: Instant,
    /// Last reported case and output — a repeat is not worth redrawing for.
    last: Option<(usize, String)>,
}

/// The condition half of a `StateCase`, copied out so the poller thread does
/// not need to borrow the configuration.
struct CaseCondition {
    contains: Option<String>,
    equals: Option<String>,
    exit: Option<i32>,
}

impl CaseCondition {
    fn matches(&self, stdout: &str, exit_code: Option<i32>) -> bool {
        if let Some(needle) = &self.contains
            && !stdout.contains(needle.as_str())
        {
            return false;
        }
        if let Some(expected) = &self.equals
            && stdout.trim() != expected.trim()
        {
            return false;
        }
        if let Some(expected) = self.exit
            && exit_code != Some(expected)
        {
            return false;
        }
        true
    }
}

/// Runs the state commands of one page and reports the results.
pub struct StatePoller {
    updates: Receiver<StateUpdate>,
    wake: Sender<()>,
    stop: Arc<AtomicBool>,
}

impl StatePoller {
    /// Start watching. `specs` pairs a key with its state configuration;
    /// an empty list gives a poller that simply never reports anything.
    pub fn start(specs: Vec<(u8, &StateSpec)>) -> Self {
        let (update_tx, updates) = channel();
        let (wake, wake_rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));

        let watches: Vec<Watch> = specs
            .into_iter()
            .map(|(key, spec)| Watch {
                key,
                run: spec.run.clone(),
                interval: Duration::from_secs_f32(spec.interval.max(0.1)),
                cases: spec
                    .cases
                    .iter()
                    .map(|case| CaseCondition {
                        contains: case.contains.clone(),
                        equals: case.equals.clone(),
                        exit: case.exit,
                    })
                    .collect(),
                // Poll every key once right away, so the page starts out right.
                next_poll: Instant::now(),
                last: None,
            })
            .collect();

        if !watches.is_empty() {
            let thread_stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("rustydeck-state".into())
                .spawn(move || poll_loop(watches, update_tx, wake_rx, thread_stop))
                .expect("spawning the state thread");
        }

        Self {
            updates,
            wake,
            stop,
        }
    }

    /// State changes since the last call.
    pub fn drain(&self) -> Vec<StateUpdate> {
        self.updates.try_iter().collect()
    }

    /// Poll every key at once — worth doing right after a key press, so the
    /// picture catches up with what the press just changed.
    pub fn refresh(&self) {
        let _ = self.wake.send(());
    }
}

impl Drop for StatePoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the thread so it notices the stop flag instead of sleeping on.
        let _ = self.wake.send(());
    }
}

fn poll_loop(
    mut watches: Vec<Watch>,
    updates: Sender<StateUpdate>,
    wake: Receiver<()>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }

        let now = Instant::now();
        for watch in watches.iter_mut().filter(|w| w.next_poll <= now) {
            watch.next_poll = now + watch.interval;

            let Some((stdout, exit_code)) = capture(&watch.run) else {
                continue;
            };
            let matched = watch
                .cases
                .iter()
                .position(|case| case.matches(&stdout, exit_code));

            let Some(case) = matched else {
                log::debug!(
                    "key {}: state `{}` matches no case",
                    watch.key,
                    stdout.trim()
                );
                continue;
            };

            let trimmed = stdout.trim();
            if watch
                .last
                .as_ref()
                .is_none_or(|(last_case, last_out)| *last_case != case || last_out != trimmed)
            {
                watch.last = Some((case, trimmed.to_string()));
                let update = StateUpdate {
                    key: watch.key,
                    case,
                    stdout: trimmed.to_string(),
                    exit: exit_code,
                };
                if updates.send(update).is_err() {
                    return; // the daemon dropped the poller
                }
            }
        }

        // Sleep until the next key is due, but wake early on request.
        let next = watches
            .iter()
            .map(|w| w.next_poll)
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        let nap = next.saturating_duration_since(Instant::now());

        match wake.recv_timeout(nap.max(Duration::from_millis(20))) {
            Ok(()) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                // A press just changed something; give it a moment to land.
                std::thread::sleep(Duration::from_millis(120));
                let now = Instant::now();
                for watch in &mut watches {
                    watch.next_poll = now;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Run a state command and collect its output. `None` means it could not be
/// started at all.
fn capture(command: &Command) -> Option<(String, Option<i32>)> {
    let mut cmd = match command {
        Command::Shell(line) => {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(line);
            c
        }
        Command::Argv(argv) => {
            let (program, args) = argv.split_first()?;
            let mut c = std::process::Command::new(program);
            c.args(args);
            c
        }
    };

    match cmd.stdin(std::process::Stdio::null()).output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            // Some tools report state on stderr; take both into account.
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Some((text, output.status.code()))
        }
        Err(e) => {
            log::warn!("state command {command:?} could not be run: {e}");
            None
        }
    }
}
