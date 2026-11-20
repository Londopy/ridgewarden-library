//! Free disk space, from `df -Pk` (POSIX output, in every Linux userland). Rust's standard
//! library has no portable way to ask, and the suite adds no crates for it.

use std::path::Path;

/// One file system as seen from a folder.
#[derive(Clone, Debug, PartialEq)]
pub struct Space {
    pub free: u64,
    /// where it is mounted as seen from here; a systemd unit's ReadWritePaths each show up
    /// as their own mount, even on one disk
    pub mount: String,
    /// the device number, which a bind mount shares with the disk under it
    pub dev: Option<u64>,
}

#[cfg(unix)]
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device(_: &Path) -> Option<u64> {
    None
}

/// The file system holding `path`, or None if `df` isn't there or said something
/// unexpected.
pub fn free(path: &Path) -> Option<Space> {
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let (free, mount) = parse_df(&String::from_utf8_lossy(&out.stdout))?;
    Some(Space {
        free,
        mount,
        dev: device(path),
    })
}

/// "Filesystem 1024-blocks Used Available Capacity Mounted on" then one line per file
/// system; a long device name can't wrap with -P.
pub fn parse_df(text: &str) -> Option<(u64, String)> {
    let line = text.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }
    let kb: u64 = cols[3].parse().ok()?;
    Some((kb * 1024, cols[5..].join(" ")))
}

/// Room for copying `size` bytes while `others` bytes of other copies are still on their
/// way. A copy lands in the core's blob store, is exported next to it, and is then moved
/// to the served folder: a rename on one mount, a full copy across mounts, even on one
/// disk. Returns why not, if not.
pub fn room(
    core: Option<Space>,
    app: Option<Space>,
    size: u64,
    others: u64,
    keep_free: u64,
) -> Result<(), String> {
    let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
    let need = |s: &Space, n: u64| {
        let want = n * (size + others) + keep_free;
        if s.free < want {
            Err(format!(
                "needs {} free on {}, has {}",
                gb(want),
                s.mount,
                gb(s.free)
            ))
        } else {
            Ok(())
        }
    };
    match (core, app) {
        (Some(c), Some(a)) => {
            let same_disk = match (c.dev, a.dev) {
                (Some(x), Some(y)) => x == y,
                _ => c.mount == a.mount,
            };
            match (same_disk, c.mount == a.mount) {
                (true, true) => need(&c, 2),
                (true, false) => need(&c, 3),
                (false, _) => need(&c, 2).and_then(|_| need(&a, 1)),
            }
        }
        (Some(c), None) => need(&c, 2),
        (None, Some(a)) => need(&a, 1),
        (None, None) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_df_output() {
        let out = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                   /dev/mmcblk0p2    29540652 10242876  18077776      37% /\n";
        assert_eq!(parse_df(out), Some((18_077_776 * 1024, "/".into())));
        assert_eq!(parse_df("df: /nope: No such file or directory\n"), None);
    }

    fn sp(free: u64, mount: &str, dev: u64) -> Option<Space> {
        Some(Space {
            free,
            mount: mount.into(),
            dev: Some(dev),
        })
    }

    #[test]
    fn room_counts_every_copy_on_the_disk_it_lands_on() {
        let gb = 1_000_000_000;
        // one mount: blob store and export, then a rename
        assert!(room(sp(25 * gb, "/", 1), sp(25 * gb, "/", 1), 10 * gb, 0, 2 * gb).is_ok());
        let err = room(sp(20 * gb, "/", 1), sp(20 * gb, "/", 1), 10 * gb, 0, 2 * gb).unwrap_err();
        assert!(err.contains("22.0 GB"), "{err}");
        // the packaged unit: two bind mounts of one disk, so the move is a copy
        let core = sp(25 * gb, "/", 1);
        let app = sp(25 * gb, "/var/lib/ridgewarden-library", 1);
        let err = room(core.clone(), app.clone(), 10 * gb, 0, 2 * gb).unwrap_err();
        assert!(err.contains("32.0 GB"), "{err}");
        // another copy still on its way counts too
        assert!(
            room(
                sp(49 * gb, "/", 1),
                sp(49 * gb, "/", 1),
                10 * gb,
                15 * gb,
                0
            )
            .is_err()
        );
        // separate disks
        let core = sp(21 * gb, "/", 1);
        assert!(room(core, sp(11 * gb, "/srv", 2), 10 * gb, 0, gb).is_ok());
        assert!(
            room(None, None, 10 * gb, 0, 0).is_ok(),
            "no df: the budget is all we have"
        );
    }
}
