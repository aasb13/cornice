//! Loading a config file and keeping it in sync with what's on disk.
//!
//! Both binaries want the same behaviour from a TOML file in their XDG config
//! directory, and both used to want it in slightly different ways:
//!
//! * **First run** — write a fully commented template listing every option, so
//!   the file is always a complete, self-documenting description of the version
//!   that wrote it.
//! * **Automatic** — watch the file and re-read it when it changes, on an
//!   interval (see [`ReloadConfig`]). Cheaper and more reliable than an inotify
//!   subscription: it survives the file being replaced by a rename, which is
//!   what most editors do on save.
//! * **Manual** — catch `SIGHUP` so `<binary> --reload` (or `kill -HUP`) forces
//!   a re-read regardless of whether the watcher is enabled.
//!
//! Two different failure policies apply, and the difference is deliberate:
//!
//! * At **startup**, a config that doesn't parse is a hard error. There is no
//!   previous config to fall back to, and a bar or wallpaper that silently came
//!   up with the wrong settings is worse than one that refused to start.
//! * On **reload**, a broken config is logged and ignored: the last good one
//!   keeps running. A typo while editing would otherwise kill your wallpaper or
//!   bar and leave you with nothing on screen.
//!
//! [`Reload`] is where both live, so neither binary has to remember the rule.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::Watcher;

/// Build the commented config template a missing file is created from.
///
/// Generated from the caller's `Default` impl rather than hand-written, so the
/// template cannot drift out of sync with the code — a new option appears here
/// the moment someone adds it, and a renamed one stops being advertised at the
/// same moment.
pub fn commented_template<T: Serialize + Default>(header: &str) -> String {
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

/// A config file that can be read, validated and used.
///
/// Implemented by each binary's `Config` rather than wrapped in a generic
/// struct here, so validation stays next to the fields it checks — and so the
/// "unknown keys are an error" rule (`deny_unknown_fields`) is declared once,
/// on the type, instead of being applied by a helper that has to be trusted.
pub trait Config: serde::de::DeserializeOwned + Serialize + Default {
    /// The `[reload]` table. Required rather than conventional: watching and
    /// the poll interval are owned here, and a config that didn't expose them
    /// couldn't have its own reload settings applied.
    fn reload(&self) -> &ReloadConfig;

    /// Reject a config that parses but can't be used — an unknown source,
    /// inverted ranges, an empty glyph set.
    ///
    /// Called after parsing and before the config is put into service, so these
    /// surface as a startup error rather than a blank wallpaper hours later.
    /// The default accepts everything, for a config with nothing to check.
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// `[reload]` — how the config is re-read while running.
///
/// Identical in every binary, which is why it lives here rather than in either
/// binary's config module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadConfig {
    /// Re-read the config when the file changes on disk. `SIGHUP` (`kill -HUP`
    /// or `<binary> --reload`) reloads regardless of this setting.
    #[serde(default = "default_true")]
    pub watch: bool,

    /// How often to check the config file for changes, in milliseconds. Polling
    /// is a single `stat`; the file is only re-parsed when its mtime or size
    /// actually moved, so this costs nothing in practice.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_true() -> bool {
    true
}

fn default_interval_ms() -> u64 {
    500
}

impl Default for ReloadConfig {
    fn default() -> Self {
        ReloadConfig {
            watch: default_true(),
            interval_ms: default_interval_ms(),
        }
    }
}

impl ReloadConfig {
    /// The poll interval, clamped to a range that is neither "re-parse every
    /// frame" nor "effectively never". Absurd values are clamped rather than
    /// rejected, so a typo can't stop the binary from reloading at all.
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.clamp(50, 60_000))
    }

    pub fn validate(&self) -> Result<()> {
        if self.interval_ms == 0 {
            bail!("reload.interval_ms must be greater than 0");
        }
        Ok(())
    }
}

/// A config file plus the machinery to keep it current.
///
/// Owns the path, the poll interval, whether watching is on, and the config
/// currently in force — so `[reload] watch` and `[reload] interval_ms` are
/// ordinary settings that take effect on the next edit, not values frozen at
/// startup.
pub struct Reload<T: Config> {
    watcher: Watcher,
    path: PathBuf,
    settings: ReloadConfig,
    current: T,
}

impl<T: Config> Reload<T> {
    /// Where `app`'s config lives: `--config` if given, else
    /// `<XDG config dir>/<app>/config.toml`.
    pub fn default_path(app: &str, explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        let dir = dirs::config_dir().context("could not determine config directory")?;
        Ok(dir.join(app).join("config.toml"))
    }

    /// Read the config, writing a commented template first if the file isn't
    /// there yet.
    ///
    /// A parse or validation failure here is fatal, unlike on reload: there is
    /// no previous config to keep running, and refusing to start is better than
    /// starting with settings the user didn't ask for.
    pub fn open(app: &str, header: &str, explicit: Option<&Path>) -> Result<Self> {
        let path = Self::default_path(app, explicit)?;

        let (current, settings) = match std::fs::read_to_string(&path) {
            Ok(text) => {
                log::info!("loaded config from {}", path.display());
                let cfg: T = toml::from_str(&text)
                    .with_context(|| format!("failed to parse config file {}", path.display()))?;
                cfg.validate()?;
                let settings = cfg.reload().clone();
                (cfg, settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no config found at {}, creating default", path.display());
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).context("failed to create config directory")?;
                }
                std::fs::write(&path, commented_template::<T>(header))
                    .context("failed to write default config file")?;
                (T::default(), ReloadConfig::default())
            }
            Err(e) => bail!("failed to read config file {}: {e}", path.display()),
        };

        let watcher = Watcher::new(path.clone(), settings.interval());
        Ok(Reload {
            watcher,
            path,
            settings,
            current,
        })
    }

    /// The config currently in force.
    pub fn config(&self) -> &T {
        &self.current
    }

    /// The poll interval in force, which a config edit can change.
    ///
    /// Callers driving their own timer read this per tick rather than capturing
    /// it, so an edit to `interval_ms` takes effect without the timer being
    /// rebuilt.
    pub fn interval(&self) -> Duration {
        self.watcher.interval()
    }

    /// Re-read if the file changed on disk.
    ///
    /// `None` means "nothing to do": either nothing changed, watching is off,
    /// or the file is broken — which is logged and leaves the previous config
    /// running. Callers therefore never have to handle an error from the render
    /// path, which is the whole point: a half-saved file must not be able to
    /// take the process down.
    ///
    /// The file is still *read* when watching is off, because `[reload]` is
    /// about this process rather than about the config's content — see
    /// [`Reload::reread`].
    pub fn poll(&mut self) -> Option<&T> {
        if !self.watcher.changed() {
            return None;
        }
        self.reread(true)
    }

    /// Re-read now, bypassing both the poll interval and the `watch` setting.
    /// This is the `SIGHUP` path.
    pub fn force(&mut self) -> Option<&T> {
        self.reread(false)
    }

    /// Read and validate. `None` means the file is unusable — logged, with the
    /// previous config left in force.
    ///
    /// Every failure path in the crate converges here, which is what makes
    /// "a broken edit never takes the process down" a property of one function
    /// rather than of each caller's error handling.
    ///
    /// `honour_watch` distinguishes the two callers, and is why the file is read
    /// even when watching is off: `[reload] watch` has to stay reversible. If
    /// turning watching off also stopped the file being read, nothing would be
    /// left to notice the config asking for it back, and `watch = false` would
    /// be a one-way door that only a restart could undo.
    fn reread(&mut self, honour_watch: bool) -> Option<&T> {
        let cfg = match read_config::<T>(&self.path) {
            Ok(cfg) => cfg,
            Err(e) => {
                log::error!("config reload failed, keeping the previous config: {e:#}");
                return None;
            }
        };

        // The reload table always applies; the rest of the config only does when
        // watching is on. Bracketing the edit with both settings rather than
        // judging it by one is what keeps `watch` lag-free in both directions:
        // the edit that turns watching off is itself honoured, and so is the one
        // that turns it back on. Using only the new setting would drop the
        // first of those two, since it is off by the time it is read.
        let was_watching = self.settings.watch;
        let now_watching = self.adopt_settings(&cfg);
        if honour_watch && !(was_watching || now_watching) {
            return None;
        }

        self.current = cfg;
        Some(&self.current)
    }

    /// Install the `[reload]` table from a freshly-read config and re-base the
    /// watcher on it. Returns whether the file wants the rest of the config
    /// adopted.
    fn adopt_settings(&mut self, cfg: &T) -> bool {
        self.settings = cfg.reload().clone();
        self.watcher.set_interval(self.settings.interval());
        self.settings.watch
    }
}

/// Read and validate a config file, with both failure modes reported against
/// the path so the message names the file that is wrong.
fn read_config<T: Config>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let cfg: T = toml::from_str(&text)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const APP: &str = "reload-test";
    const HEADER: &str = "# test\n";

    /// Stand-in for a binary's config: one value to prove it round-trips, a
    /// `[reload]` table to prove the settings are live, and a validation rule to
    /// prove `validate` is actually called before anything is adopted.
    ///
    /// `Default` is derived so that it agrees with `#[serde(default)]` — an
    /// all-comments template deserializes as an empty document, so anything with
    /// a hand-written `Default` would silently disagree with what the template
    /// round-trips to.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        #[serde(default)]
        color: String,
        #[serde(default)]
        forbidden: bool,
        #[serde(default)]
        reload: ReloadConfig,
    }

    impl Config for Sample {
        fn validate(&self) -> Result<()> {
            if self.forbidden {
                bail!("forbidden is not allowed");
            }
            self.reload.validate()
        }

        fn reload(&self) -> &ReloadConfig {
            &self.reload
        }
    }

    /// A fresh temp directory per test, since these all write real files.
    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rwreload-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.join("config.toml")
    }

    fn open(path: &Path) -> Reload<Sample> {
        Reload::open(APP, HEADER, Some(path)).unwrap()
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        // The watcher compares mtime *and* size, so a same-length rewrite can
        // slip through on a coarse clock. Give the filesystem a distinct mtime.
        let later = std::fs::metadata(path).unwrap().modified().unwrap() + Duration::from_secs(1);
        filetime_set(path, later);
    }

    fn filetime_set(path: &Path, time: std::time::SystemTime) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(time).unwrap();
    }

    #[test]
    fn open_creates_a_template_when_there_is_no_file() {
        let path = tmp("create");
        let r = open(&path);

        assert!(path.is_file(), "the template was not written");
        // And the template is a valid config, since it is all comments.
        let reread: Sample = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread, Sample::default());
        assert_eq!(r.config(), &Sample::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Startup and reload disagree on purpose: there is no previous config to
    /// fall back to at startup, so refusing to start beats running settings the
    /// user didn't ask for.
    #[test]
    fn open_refuses_a_broken_config_rather_than_guessing() {
        let path = tmp("startup-broken");
        std::fs::write(&path, "color = ").unwrap();
        let err = match Reload::<Sample>::open(APP, HEADER, Some(&path)) {
            Ok(_) => panic!("a config that does not parse must not load at startup"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("parse"),
            "unhelpful error for a broken startup config: {err:#}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The invariant the whole crate exists for. A reload that fails to parse,
    /// or fails validation, must leave the running config exactly as it was.
    #[test]
    fn a_broken_edit_leaves_the_previous_config_in_force() {
        let path = tmp("broken-edit");
        std::fs::write(&path, "color = \"green\"").unwrap();
        let mut r = open(&path);
        assert_eq!(r.config().color, "green");

        for broken in ["color = ", "forbidden = true\n", "nope = 1\n"] {
            write(&path, broken);
            assert!(
                r.poll().is_none(),
                "a broken config ({broken:?}) must not be adopted"
            );
            assert_eq!(
                r.config().color,
                "green",
                "the last good config must still be in force after {broken:?}"
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_good_edit_is_adopted() {
        let path = tmp("good-edit");
        std::fs::write(&path, "color = \"green\"").unwrap();
        let mut r = open(&path);

        write(&path, "color = \"blue\"");
        let updated = r.poll().expect("a valid edit must be adopted");
        assert_eq!(updated.color, "blue");
        assert_eq!(r.config().color, "blue", "and become what is in force");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `[reload] watch` is a setting, so turning it off has to take effect on the
    /// next edit — and turning it *back* on has to resume watching. The second
    /// direction is the one that is easy to get wrong: if switching watching off
    /// also stopped the file being read, nothing would be left to notice the
    /// config asking for it back, and `watch = false` would need a restart to
    /// undo.
    #[test]
    fn watch_can_be_turned_off_and_back_on_at_runtime() {
        let path = tmp("watch-toggle");
        write(&path, "color = \"green\"\nreload.watch = true");
        let mut r = open(&path);

        write(&path, "color = \"blue\"\nreload.watch = false");
        assert!(r.poll().is_some(), "the edit turning watch off must land");

        write(&path, "color = \"red\"\nreload.watch = false");
        assert!(
            r.poll().is_none(),
            "an edit while watch is off must not be adopted"
        );
        assert_eq!(r.config().color, "blue", "content must be left alone");

        write(&path, "color = \"red\"\nreload.watch = true");
        assert!(
            r.poll().is_some(),
            "turning watch back on must resume watching"
        );
        assert_eq!(r.config().color, "red");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Same for the interval: it gates polling, so it is only observable through
    /// the watcher, and a value captured at startup would pin the cadence until
    /// a restart.
    #[test]
    fn the_poll_interval_can_be_changed_at_runtime() {
        let path = tmp("interval");
        std::fs::write(&path, "color = \"green\"\nreload.interval_ms = 60000").unwrap();
        let mut r = open(&path);
        assert_eq!(r.interval(), Duration::from_secs(60));

        write(&path, "color = \"green\"\nreload.interval_ms = 50");
        assert!(r.poll().is_some());
        assert_eq!(r.interval(), Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `SIGHUP` has to work regardless of `watch` — that is the entire reason it
    /// exists, for a user who turned the poller off but still wants a colour
    /// change to apply without waiting out an interval.
    #[test]
    fn force_reloads_even_when_watching_is_off() {
        let path = tmp("force");
        std::fs::write(&path, "color = \"green\"\nreload.watch = false").unwrap();
        let mut r = open(&path);
        assert!(r.poll().is_none(), "watching is off, so nothing is polled");

        std::fs::write(&path, "color = \"blue\"\nreload.watch = false").unwrap();
        assert!(
            r.force().is_some(),
            "SIGHUP must reload even with watching off"
        );
        assert_eq!(r.config().color, "blue");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn force_also_survives_a_broken_file() {
        let path = tmp("force-broken");
        std::fs::write(&path, "color = \"green\"").unwrap();
        let mut r = open(&path);

        std::fs::write(&path, "color = ").unwrap();
        assert!(r.force().is_none());
        assert_eq!(r.config().color, "green");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
