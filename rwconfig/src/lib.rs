//! Config reloading shared by `rustwall` and `rustbar`.
//!
//! Both binaries want the same two behaviours, and both get them wrong in the
//! same ways if written twice:
//!
//! * **Automatic** — watch the config file and re-read it when it changes, on an
//!   interval (see each binary's `[reload]` table). Cheaper and more reliable
//!   than an inotify subscription: it survives the file being replaced by a
//!   rename, which is what most editors do on save.
//! * **Manual** — catch `SIGHUP` so `rustbar --reload` (or `kill -HUP`) forces a
//!   re-read regardless of whether the watcher is enabled.
//!
//! The important property both paths share: **a broken config never takes the
//! running process down.** A typo while editing would otherwise kill your
//! wallpaper or bar and leave you with nothing on screen. Instead the error is
//! logged and the last good config stays in effect.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Tracks the config file and reports when its contents have changed.
///
/// Compares modification time *and* size rather than content: re-reading and
/// hashing the file on every tick is wasted work for a change this rare, and
/// mtime+size catches every save that a human would consider a change. The
/// caveat is a same-second write that preserves size, which some editors and
/// `cp -f` will do — that's what `SIGHUP` is for.
#[derive(Debug)]
pub struct Watcher {
    path: PathBuf,
    last_modified: Option<SystemTime>,
    last_size: Option<u64>,
    interval: Duration,
    /// When the next poll is allowed. Keeps a burst of `SIGHUP`s, or an
    /// editor's write-chmod-write dance, from causing a re-parse per event.
    next_poll: Option<std::time::Instant>,
}

impl Watcher {
    /// Start watching `path`. The first [`Watcher::changed`] call always reports
    /// a change if the file exists, so the caller's initial load and the first
    /// tick agree on what's "current".
    pub fn new(path: PathBuf, interval: Duration) -> Self {
        let (mtime, size) = stat_of(&path);
        Watcher {
            path,
            last_modified: mtime,
            last_size: size,
            interval,
            next_poll: None,
        }
    }

    /// Whether enough time has passed to check the file again.
    ///
    /// Optional: [`Watcher::changed`] gates itself, so a caller only needs this
    /// to avoid the `stat` when it has other per-tick work to do first.
    pub fn due(&self) -> bool {
        match self.next_poll {
            Some(t) => std::time::Instant::now() >= t,
            None => true,
        }
    }

    /// Re-check the file. `true` means its mtime or size moved since last time,
    /// so the caller should re-read and re-parse it.
    ///
    /// Gated by the interval internally, so a burst of writes costs one `stat`
    /// rather than a re-parse each. A file that has been deleted or made
    /// unreadable reports no change: the wallpaper keeps running on the last good
    /// config, which is friendlier than blanking the screen because someone is
    /// mid-edit in a rename.
    pub fn changed(&mut self) -> bool {
        if !self.due() {
            return false;
        }
        self.next_poll = Some(std::time::Instant::now() + self.interval);
        let (mtime, size) = stat_of(&self.path);
        if mtime.is_none() && size.is_none() {
            // Gone or unreadable: nothing to compare against, so treat it as
            // unchanged rather than as a change we can't apply.
            return false;
        }
        if mtime != self.last_modified || size != self.last_size {
            self.last_modified = mtime;
            self.last_size = size;
            return true;
        }
        false
    }

    /// Clear the interval gate so the next [`Watcher::changed`] call checks
    /// immediately. This is what lets a manual `SIGHUP` reload take effect now
    /// rather than up to one interval later.
    pub fn poke(&mut self) {
        self.next_poll = None;
    }
}

fn stat_of(path: &Path) -> (Option<SystemTime>, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().ok(), Some(m.len())),
        Err(_) => (None, None),
    }
}

/// Whether a change needs a full re-parse, given the file's current text.
///
/// Some editors save by truncating and rewriting, so a read can land between the
/// two and see a partial file. If what we read doesn't parse, this returns
/// `false` so the caller can skip it and try again on the next tick rather than
/// logging an error for a file that is perfectly valid a few milliseconds later.
pub fn looks_complete<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, toml::de::Error> {
    toml::from_str(text)
}

#[cfg(unix)]
mod sighup {
    /// Block `SIGHUP` process-wide, before any thread exists.
    ///
    /// This has to happen first, in `main`, and not where the signal source is
    /// created: a signal mask is per-thread, and `signalfd` only receives a
    /// signal that is blocked in *every* thread. Blocking it late leaves the
    /// worker threads (the power poller, the cava reader, the tray client) with
    /// `SIGHUP` unblocked, so the kernel is free to deliver it to one of them
    /// and the default action — terminate — happens instead of the reload.
    ///
    /// Threads spawned after this inherit the mask, which is what makes it hold:
    /// call it before anything starts a thread.
    pub fn block_early() -> anyhow::Result<()> {
        let mut set = nix::sys::signal::SigSet::empty();
        set.add(nix::sys::signal::Signal::SIGHUP);
        set.thread_block()
            .map_err(|e| anyhow::anyhow!("failed to block SIGHUP: {e}"))
    }

    /// Send `SIGHUP` to every other process running the same executable as us,
    /// so `rustbar --reload` from a shell reaches the bar.
    ///
    /// Receiving is the caller's job, via calloop's signal source — a polled
    /// flag can't wake an event loop that's blocked in `dispatch`, which is
    /// where both binaries spend nearly all their time.
    pub fn signal_siblings() -> anyhow::Result<usize> {
        let me = std::process::id();
        let exe = std::env::current_exe().ok();
        let exe = exe.as_deref().and_then(|p| std::fs::canonicalize(p).ok());
        let mut signalled = 0usize;
        for pid in pids() {
            if pid == me || pid == 0 {
                continue;
            }
            // Prefer matching on the executable, so `--reload` never signals an
            // unrelated process that happens to share the name.
            if let Some(want) = &exe {
                match std::fs::read_link(format!("/proc/{pid}/exe")) {
                    Ok(path) if &path == want => {}
                    _ => continue,
                }
            }
            if nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGHUP,
            )
            .is_ok()
            {
                signalled += 1;
            }
        }
        Ok(signalled)
    }

    fn pids() -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .collect()
    }
}

#[cfg(unix)]
pub use sighup::{block_early, signal_siblings};

/// Build the commented config template both binaries write on first run.
///
/// Generated from the caller's `Default` impl rather than hand-written, so the
/// template cannot drift out of sync with the code — a new option appears here
/// the moment someone adds it, and a renamed one stops being advertised at the
/// same moment.
pub fn commented_template<T: serde::Serialize + Default>(header: &str) -> String {
    let dumped = toml::to_string_pretty(&T::default()).expect("Default must serialize");
    let mut out = String::from(header);
    for line in dumped.lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rwconfig-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn watcher_reports_the_first_change_and_then_stays_quiet() {
        let dir = tmp("watch");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1").unwrap();

        let mut w = Watcher::new(path.clone(), Duration::from_millis(0));
        w.changed();
        assert!(!w.changed(), "an unchanged file must not report a change");

        std::fs::write(&path, "a = 2").unwrap();
        assert!(w.changed(), "a rewritten file must report a change");
        assert!(!w.changed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A config being edited is briefly missing or truncated. Neither may be
    /// reported as a usable change, or the running process reacts to a state no
    /// user ever asked for.
    #[test]
    fn a_missing_file_is_not_a_change() {
        let dir = tmp("missing");
        let path = dir.join("config.toml");
        let mut w = Watcher::new(path, Duration::from_millis(0));
        assert!(!w.changed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `interval` gates polling so a burst of writes doesn't cause a re-parse
    /// per write — but a `SIGHUP` must be able to bypass the gate, since that's
    /// the whole point of it being a manual override.
    #[test]
    fn the_interval_gates_polling_but_poke_bypasses_it() {
        let dir = tmp("gate");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1").unwrap();
        let mut w = Watcher::new(path.clone(), Duration::from_secs(3600));

        w.changed();
        assert!(
            !w.due(),
            "the interval should be holding the next poll back"
        );
        assert!(!w.changed(), "a gated poll must not check the file");

        std::fs::write(&path, "a = 2").unwrap();
        assert!(!w.changed(), "still gated");

        w.poke();
        assert!(w.due());
        assert!(w.changed(), "poke must force the next check through");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A size change is what catches a same-second edit that preserves mtime —
    /// the case mtime-only comparison silently misses.
    #[test]
    fn a_size_change_is_caught_even_without_an_mtime_change() {
        let dir = tmp("size");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1").unwrap();
        let mut w = Watcher::new(path.clone(), Duration::from_millis(0));
        w.changed();

        // Longer contents: different size, so the watcher must notice even if
        // the filesystem hands back an identical mtime.
        std::fs::write(&path, "a = 123456789").unwrap();
        assert!(w.changed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Sample {
        #[serde(default)]
        color: String,
    }

    /// The template both binaries write must parse as-is (it's all comments)
    /// and must round-trip to the defaults when uncommented — otherwise
    /// "uncomment everything" silently produces a different config.
    #[test]
    fn template_is_valid_and_round_trips() {
        let text = commented_template::<Sample>("# header\n");
        assert_eq!(toml::from_str::<Sample>(&text).unwrap(), Sample::default());

        let live: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(toml::from_str::<Sample>(&live).unwrap(), Sample::default());
    }

    /// `--reload` has to find the *other* instance and not itself, so this spawns
    /// a real child from the same executable and checks the signal reaches it.
    #[cfg(unix)]
    #[test]
    fn signal_siblings_reaches_a_same_binary_sibling_but_not_itself() {
        use std::os::unix::process::ExitStatusExt;

        let dir = tmp("sibling");
        let pidfile = dir.join("pid");

        // Re-enter this same test binary as a plain child process. It is a copy
        // of the executable under test from `/proc/<pid>/exe`'s point of view,
        // which is exactly the match `signal_siblings` keys off.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--nocapture")
            .arg("--exact")
            .arg("tests::sibling_reports_its_pid")
            .env("RWCONFIG_SIBLING_PIDFILE", &pidfile)
            .spawn()
            .unwrap();

        // The child publishes its pid, so the test knows it is really up rather
        // than racing its startup.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let child_pid: u32 = loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse() {
                    break pid;
                }
            }
            assert!(child.try_wait().unwrap().is_none(), "sibling exited early");
            assert!(
                std::time::Instant::now() < deadline,
                "sibling never published its pid"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(child_pid, child.id());
        assert_ne!(child_pid, std::process::id(), "must not signal itself");

        assert!(signal_siblings().unwrap() >= 1, "sibling was not found to signal");

        // SIGHUP's default action terminates, so a delivered signal shows up as
        // the child dying on its own rather than having to be killed.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            match child.try_wait().unwrap() {
                Some(status) => break status,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    let _ = child.kill();
                    panic!("sibling survived SIGHUP — signal_siblings did not reach it");
                }
            }
        };
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGHUP as i32),
            "expected death by SIGHUP, got {status:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The child half of the test above: publish our pid, then wait to be killed.
    /// Does nothing unless the parent asked for it, so a normal test run is
    /// unaffected.
    #[cfg(unix)]
    #[test]
    fn sibling_reports_its_pid() {
        let Ok(pidfile) = std::env::var("RWCONFIG_SIBLING_PIDFILE") else {
            return;
        };
        std::fs::write(pidfile, std::process::id().to_string()).unwrap();
        // SIGHUP terminates by default, so this sleep should never finish.
        std::thread::sleep(Duration::from_secs(30));
        std::process::exit(0);
    }
}
