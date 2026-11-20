//! kiwix-serve, run on the ZIM files this site currently serves. Reading ZIM files needs
//! zstd and xz, which this app doesn't carry, so Wikipedia is served by Kiwix's own server.
//! When a new version takes over, kiwix-serve is restarted on it. If it won't run on the
//! new files, it goes back to the last set that worked, and those files are kept until a
//! new set has stayed up for a minute.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ridgewarden_client::util::{now_ms, short_time};

use crate::config::KiwixCfg;

/// How long kiwix-serve must stay up on a set of files before it counts as working.
pub const SETTLE: Duration = Duration::from_secs(60);
/// How long to stay on the old files after the new ones failed, before trying again.
const RETRY: Duration = Duration::from_secs(600);
const LOG_MAX: u64 = 10_000_000;

#[derive(Default)]
pub struct Kiwix {
    state: Mutex<State>,
    status: Mutex<String>,
    /// when kiwix-serve last answered over HTTP, and what went wrong since
    pub probe: Mutex<(Option<i64>, Option<String>)>,
}

#[derive(Default)]
struct State {
    run: Option<Running>,
    /// the last set of files kiwix-serve ran on for SETTLE
    good: Vec<PathBuf>,
    /// a set it would not run on, and when that was found
    failed: Option<(Vec<PathBuf>, Instant)>,
}

struct Running {
    child: Child,
    zims: Vec<PathBuf>,
    started: Instant,
}

/// The command with `{zims}` replaced by one argument per file.
pub fn expand(cmd: &[String], zims: &[PathBuf]) -> Vec<String> {
    let mut out = vec![];
    for a in cmd {
        if a == "{zims}" {
            out.extend(zims.iter().map(|z| z.to_string_lossy().into_owned()));
        } else {
            out.push(a.clone());
        }
    }
    out
}

impl State {
    /// The files to run on: the wanted ones, unless they just failed and the last set
    /// that worked is still on disk.
    fn target(&self, want: &[PathBuf]) -> Vec<PathBuf> {
        let failed = self
            .failed
            .as_ref()
            .is_some_and(|(f, at)| f == want && at.elapsed() < RETRY);
        if failed && !self.good.is_empty() && self.good.iter().all(|p| p.exists()) {
            self.good.clone()
        } else {
            want.to_vec()
        }
    }
}

impl Kiwix {
    pub fn status(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, s: String) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// Files kiwix-serve runs on now or may go back to; they must not be deleted.
    pub fn in_use(&self) -> Vec<PathBuf> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = st.good.clone();
        v.extend(st.run.iter().flat_map(|r| r.zims.clone()));
        v
    }

    /// Has kiwix-serve stayed up on exactly these files?
    pub fn settled(&self, zims: &[PathBuf]) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.good == zims && !zims.is_empty()
    }

    /// Keep kiwix-serve running on `want`: start it, restart it on a new set of files,
    /// restart it if it died, or fall back to the last set that worked.
    pub fn check(&self, cfg: &KiwixCfg, want: Vec<PathBuf>, log: &Path) {
        if cfg.command.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let seen = st
            .run
            .as_mut()
            .map(|r| (r.child.try_wait(), r.zims.clone(), r.started.elapsed()));
        if let Some((res, zims, up)) = seen {
            match res {
                Ok(None) => {
                    if up >= SETTLE {
                        if st.failed.as_ref().is_some_and(|(f, _)| *f == zims) {
                            st.failed = None;
                        }
                        st.good = zims.clone();
                    }
                    if zims == st.target(&want) {
                        return;
                    }
                }
                Ok(Some(code)) if up < SETTLE => {
                    if st.good.is_empty() || zims == st.good {
                        self.set_status(format!(
                            "kiwix-serve stopped ({code}) right after starting; see {}",
                            log.display()
                        ));
                        // tried again once SETTLE has passed
                        return;
                    }
                    tracing::warn!("kiwix-serve would not run on the new files ({code})");
                    st.failed = Some((zims, Instant::now()));
                }
                Ok(Some(code)) => tracing::warn!("kiwix-serve stopped ({code}); starting it again"),
                Err(e) => tracing::warn!("checking on kiwix-serve: {e}"),
            }
            if let Some(mut r) = st.run.take() {
                let _ = r.child.kill();
                let _ = r.child.wait();
            }
        }
        let set = st.target(&want);
        if set.is_empty() {
            st.good.clear();
            self.set_status("waiting for a ZIM file to arrive".into());
            return;
        }
        match start(cfg, &set, log) {
            Ok(child) => {
                tracing::info!("kiwix-serve started on {} file(s)", set.len());
                self.set_status(if set == want {
                    format!(
                        "running on {} file(s) since {}",
                        set.len(),
                        short_time(now_ms())
                    )
                } else {
                    format!(
                        "kiwix-serve would not run on the newest files, so it is serving the \
                         previous ones; it tries the new ones again in 10 minutes (see {})",
                        log.display()
                    )
                });
                st.run = Some(Running {
                    child,
                    zims: set,
                    started: Instant::now(),
                });
            }
            Err(e) => {
                tracing::warn!("{e:#}");
                self.set_status(format!("{e:#}"));
            }
        }
    }

    pub fn stop(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut r) = st.run.take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }

    pub fn note_probe(&self, r: Result<()>) {
        let mut p = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Ok(()) => *p = (Some(now_ms()), None),
            Err(e) => p.1 = Some(format!("{e:#}").chars().take(200).collect()),
        }
    }
}

fn start(cfg: &KiwixCfg, zims: &[PathBuf], log: &Path) -> Result<Child> {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
    let args = expand(&cfg.command, zims);
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .with_context(|| format!("starting {}", args[0]))
}

/// Ask kiwix-serve for its front page, the way a visitor would.
pub async fn probe(http: &reqwest::Client, url: &str) -> Result<()> {
    http.get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zims_become_arguments() {
        let cmd: Vec<String> = ["kiwix-serve", "--port=8796", "{zims}"]
            .map(String::from)
            .to_vec();
        let z = vec![PathBuf::from("/c/a.zim"), PathBuf::from("/c/b.zim")];
        assert_eq!(
            expand(&cmd, &z),
            ["kiwix-serve", "--port=8796", "/c/a.zim", "/c/b.zim"]
        );
    }

    #[test]
    fn a_missing_program_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let k = Kiwix::default();
        let cfg = KiwixCfg {
            command: vec!["ridgewarden-no-such-kiwix".into(), "{zims}".into()],
            url: String::new(),
        };
        k.check(&cfg, vec![], &d.path().join("k.log"));
        assert!(k.status().contains("waiting"));
        k.check(&cfg, vec![d.path().join("a.zim")], &d.path().join("k.log"));
        assert!(
            k.status().contains("starting ridgewarden-no-such-kiwix"),
            "{}",
            k.status()
        );
    }

    /// A stand-in for kiwix-serve: stays up unless one of its files is named bad.zim.
    fn fake(dir: &Path) -> KiwixCfg {
        let command = if cfg!(windows) {
            let s = dir.join("fake.ps1");
            std::fs::write(
                &s,
                "if ($args -match 'bad.zim') { exit 3 }; Start-Sleep 600",
            )
            .unwrap();
            [
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]
            .iter()
            .map(|a| a.to_string())
            .chain([s.to_string_lossy().into_owned(), "{zims}".into()])
            .collect()
        } else {
            let s = dir.join("fake.sh");
            std::fs::write(
                &s,
                "case \"$*\" in *bad.zim*) exit 3;; esac\nexec sleep 600\n",
            )
            .unwrap();
            vec![
                "sh".into(),
                s.to_string_lossy().into_owned(),
                "{zims}".into(),
            ]
        };
        KiwixCfg {
            command,
            url: String::new(),
        }
    }

    fn age(k: &Kiwix) {
        if let Some(r) = k.state.lock().unwrap().run.as_mut() {
            r.started -= SETTLE;
        }
    }

    #[test]
    fn falls_back_to_the_files_that_worked_and_keeps_them() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("k.log");
        let (old, bad) = (d.path().join("old.zim"), d.path().join("bad.zim"));
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&bad, b"x").unwrap();
        let (k, cfg) = (Kiwix::default(), fake(d.path()));
        k.check(&cfg, vec![old.clone()], &log);
        age(&k);
        k.check(&cfg, vec![old.clone()], &log);
        assert!(k.status().starts_with("running"), "{}", k.status());
        // a new version arrives that kiwix-serve won't run on
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            k.check(&cfg, vec![bad.clone()], &log);
            if k.status().contains("previous") || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(k.status().contains("previous"), "{}", k.status());
        assert!(
            k.in_use().contains(&old),
            "the old file must not be deleted yet"
        );
        // a good new version replaces it once it has stayed up
        let new = d.path().join("new.zim");
        std::fs::write(&new, b"x").unwrap();
        k.check(&cfg, vec![new.clone()], &log);
        assert!(k.in_use().contains(&old), "not settled yet");
        age(&k);
        k.check(&cfg, vec![new.clone()], &log);
        assert!(!k.in_use().contains(&old));
        k.stop();
    }
}
