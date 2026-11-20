//! The fetcher: get each configured item from the internet (or a command, or the operator's
//! own file), check it, publish it as a blob through the core, and log a `library.item`
//! event describing it.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::header;
use ridgewarden_client::util::{ms_to_rfc3339, now_ms};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::app::App;
use crate::config::ItemCfg;
use crate::content::{blob_name, check_file, file_name, http_date_day, version_in_name};
use crate::mirror::{Held, copy_file, install, move_file};

pub const BUILDS: &str = "https://build-metadata.protomaps.dev/builds.json";
const DAY: i64 = 86_400_000;
/// a failed fetch is tried again after this, doubling with each failure in a row up to a
/// day, so a publisher and a lagging mirror that disagree don't cost a download every
/// quarter hour
const RETRY_MS: i64 = 15 * 60_000;
/// an operator's file must have stopped changing this long before it is published
const SETTLED_MS: i64 = 2 * 60_000;

/// A file ready to publish.
#[derive(Debug)]
pub struct Got {
    pub path: PathBuf,
    /// the file name the version is known by, which gives a document its type
    pub name: String,
    pub version: String,
    pub ident: String,
    pub source: String,
    pub sha256: String,
    pub hash_from: Option<String>,
    /// the operator's own file: copy it, don't take it
    pub keep: bool,
}

/// What a URL points at now, after redirects.
#[derive(Debug, Clone)]
pub struct Remote {
    /// where the redirects ended; the download goes here, so the name, size and hash
    /// belong to the file actually fetched
    pub url: String,
    pub name: String,
    pub size: Option<u64>,
    pub last_modified: Option<String>,
}

impl Remote {
    /// Same name and size means the same file; a dated Kiwix name changes with every
    /// release. Undated names also need the date. Mirrors differ in ETags, so not those.
    pub fn ident(&self) -> String {
        let mut s = format!("{} {}", self.name, self.size.unwrap_or(0));
        if version_in_name(&self.name).is_none() {
            s.push(' ');
            s.push_str(self.last_modified.as_deref().unwrap_or(""));
        }
        s
    }

    pub fn version(&self) -> String {
        version_in_name(&self.name)
            .or_else(|| self.last_modified.as_deref().and_then(http_date_day))
            .unwrap_or_else(|| ms_to_rfc3339(now_ms())[..10].to_string())
    }
}

#[derive(Default)]
struct Source {
    ident: Option<String>,
    checked_ms: i64,
    outbox: Option<String>,
    candidate: Option<String>,
    fails: i64,
}

fn source(app: &App, item: &str) -> Result<Source> {
    Ok(app
        .db()
        .query_row(
            "SELECT ident, checked_ms, outbox, candidate, fails FROM sources WHERE item = ?1",
            [item],
            |r| {
                Ok(Source {
                    ident: r.get(0)?,
                    checked_ms: r.get(1)?,
                    outbox: r.get(2)?,
                    candidate: r.get(3)?,
                    fails: r.get(4)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

fn checked(app: &App, item: &str, error: Option<&str>) -> Result<()> {
    app.db().execute(
        "INSERT INTO sources (item, checked_ms, error, fails) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item) DO UPDATE SET checked_ms = excluded.checked_ms, error = excluded.error,
             fails = CASE WHEN excluded.error IS NULL THEN 0 ELSE sources.fails + 1 END",
        params![item, now_ms(), error, i64::from(error.is_some())],
    )?;
    Ok(())
}

/// Is it time to look for a new version? An operator's file is checked every round (a
/// stat is cheap); the rest every check_days, or sooner again after a failure.
pub fn due(it: &ItemCfg, checked_ms: i64, fails: i64, now: i64) -> bool {
    if fails > 0 {
        return now - checked_ms >= (RETRY_MS << (fails - 1).min(7)).min(DAY);
    }
    it.file.is_some() || checked_ms == 0 || now - checked_ms >= it.check_days as i64 * DAY
}

/// One pass over the configured items, one at a time.
pub async fn round(app: &App) -> Result<()> {
    for it in &app.cfg.items {
        let src = source(app, &it.id)?;
        if let Some(ev) = &src.outbox
            && let Err(e) = flush(app, &it.id, ev).await
        {
            tracing::warn!("sending what {} published: {e:#}", it.id);
            continue;
        }
        if !due(it, src.checked_ms, src.fails, now_ms()) {
            continue;
        }
        let r = match acquire(app, it, &src).await {
            Ok(Some(g)) => publish(app, it, g).await,
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => checked(app, &it.id, None)?,
            Err(e) => {
                tracing::warn!("fetching {}: {e:#}", it.id);
                checked(app, &it.id, Some(&format!("{e:#}")))?;
            }
        }
    }
    Ok(())
}

/// Get a new version of `it` if there is one. None if it hasn't changed.
async fn acquire(app: &App, it: &ItemCfg, src: &Source) -> Result<Option<Got>> {
    let dl = app.cfg.downloads_dir();
    std::fs::create_dir_all(&dl).with_context(|| format!("creating {}", dl.display()))?;
    let last = src.ident.as_deref();
    let got = if let Some(url) = &it.url {
        by_url(app, it, url, &dl, last).await?
    } else if let Some(f) = &it.file {
        let seen = by_file(it, f, last, src.candidate.as_deref(), now_ms())?;
        if let File::Changing(ident) = &seen {
            app.db().execute(
                "INSERT INTO sources (item, candidate) VALUES (?1, ?2)
                 ON CONFLICT(item) DO UPDATE SET candidate = excluded.candidate",
                params![it.id, ident],
            )?;
        }
        match seen {
            File::Ready(g) => Some(g),
            _ => None,
        }
    } else {
        by_command(&app.web, it, &dl, last).await?
    };
    let Some(mut g) = got else { return Ok(None) };
    let ext = it.kind.ext_for(&g.name)?;
    let (path, kind, want) = (
        g.path.clone(),
        it.kind,
        g.hash_from.as_ref().map(|_| g.sha256.clone()),
    );
    let sha =
        tokio::task::spawn_blocking(move || check_file(&path, kind, &ext, want.as_deref())).await?;
    match sha {
        Ok(s) => g.sha256 = s,
        Err(e) => {
            // a damaged or wrong download: start over next time rather than resume it
            if !g.keep {
                let _ = std::fs::remove_file(&g.path);
            }
            return Err(e);
        }
    }
    Ok(Some(g))
}

async fn by_url(
    app: &App,
    it: &ItemCfg,
    url: &str,
    dl: &Path,
    last: Option<&str>,
) -> Result<Option<Got>> {
    let http = &app.web;
    let remote = resolve(http, url).await?;
    let ident = remote.ident();
    if last == Some(ident.as_str()) {
        return Ok(None);
    }
    if remote.size.is_some_and(|s| s > it.max_bytes()) {
        bail!(
            "{} is {} MB, more than max_mb = {}",
            remote.name,
            remote.size.unwrap_or(0) / 1_000_000,
            it.max_mb
        );
    }
    let part = dl.join(format!("{}.part", it.id));
    if let Some(size) = remote.size {
        // a resumed download needs room only for what is still to come
        let have = std::fs::metadata(&part).map_or(0, |m| m.len());
        let (core, here) = (app.cfg.core.imports_dir.clone(), dl.to_path_buf());
        let (c, h) = tokio::task::spawn_blocking(move || {
            (crate::disk::free(&core), crate::disk::free(&here))
        })
        .await?;
        let keep_free = app.cfg.mirror.keep_free_mb * 1_000_000;
        if let Err(why) = crate::disk::room(c, h, size.saturating_sub(have), 0, keep_free) {
            bail!("not downloading {}: {why}", remote.name);
        }
    }
    download(http, &remote.url, &remote, &part, &ident, it.max_bytes()).await?;
    let (sha256, hash_from) = match &it.sha256_url {
        Some(t) => {
            let u = t.replace("{name}", &remote.name);
            (publisher_sha256(http, &u, &remote.name).await?, Some(u))
        }
        None => (String::new(), None),
    };
    Ok(Some(Got {
        path: part,
        name: remote.name.clone(),
        version: remote.version(),
        ident,
        source: url.chars().take(300).collect(),
        sha256,
        hash_from,
        keep: false,
    }))
}

/// Follow redirects to find the file's real name, size and date.
pub async fn resolve(http: &reqwest::Client, url: &str) -> Result<Remote> {
    let r = http
        .head(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?;
    let name = r
        .url()
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|n| !n.is_empty())
        .with_context(|| format!("{url} has no file name"))?
        .to_string();
    let h = r.headers();
    Ok(Remote {
        url: r.url().to_string(),
        name,
        size: h
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse().ok()),
        last_modified: h
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    })
}

/// Download `url` into `part`, picking up where an earlier attempt for the same file
/// stopped. `ident` names the file; a part left from a different one starts over.
pub async fn download(
    http: &reqwest::Client,
    url: &str,
    remote: &Remote,
    part: &Path,
    ident: &str,
    max: u64,
) -> Result<()> {
    let tag = part.with_extension("ident");
    let resume = std::fs::read_to_string(&tag).is_ok_and(|t| t == ident);
    let mut have = if resume {
        std::fs::metadata(part).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    if have == 0 {
        std::fs::write(&tag, ident)?;
        std::fs::File::create(part)?;
    }
    if remote.size == Some(have) && have > 0 {
        return Ok(());
    }
    let mut req = http.get(url);
    if have > 0 {
        req = req.header(header::RANGE, format!("bytes={have}-"));
        // only resume if it is still the same file; otherwise the server sends it whole
        if let Some(lm) = &remote.last_modified {
            req = req.header(header::IF_RANGE, lm);
        }
    }
    let mut r = req.send().await?;
    let mut f = std::fs::OpenOptions::new().write(true).open(part)?;
    match r.status().as_u16() {
        206 => {
            let range = r
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !range.starts_with(&format!("bytes {have}-")) {
                bail!("server resumed at the wrong place ({range:?}); starting over next round");
            }
            f.seek(SeekFrom::Start(have))?;
            tracing::info!("resuming {} at {} MB", remote.name, have / 1_000_000);
        }
        200 => {
            f.set_len(0)?;
            have = 0;
            tracing::info!("downloading {}", remote.name);
        }
        _ => {
            let status = r.status();
            if status.as_u16() == 416 {
                std::fs::remove_file(&tag)?;
            }
            bail!("downloading {url}: HTTP {status}");
        }
    }
    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
    while let Some(chunk) = r.chunk().await? {
        have += chunk.len() as u64;
        if have > max {
            drop(w);
            std::fs::remove_file(part)?;
            bail!("{} grew past max_mb; stopped", remote.name);
        }
        w.write_all(&chunk)?;
    }
    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    if let Some(size) = remote.size
        && have != size
    {
        bail!("download stopped at {have} of {size} bytes; it picks up from there next round");
    }
    Ok(())
}

/// Read a checksum file in the `sha256sum` format Kiwix publishes: "<hex>  <name>".
pub fn parse_sha256(text: &str, name: &str) -> Result<String> {
    let mut w = text.split_whitespace();
    let hash = w
        .next()
        .map(str::to_ascii_lowercase)
        .filter(|h| crate::catalog::is_hash(h))
        .context("checksum file does not start with a SHA-256")?;
    if let Some(n) = w.next()
        && n.trim_start_matches('*') != name
    {
        bail!("checksum file is for {n}, not {name}");
    }
    Ok(hash)
}

async fn publisher_sha256(http: &reqwest::Client, url: &str, name: &str) -> Result<String> {
    let r = http
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("fetching the publisher's checksum {url}"))?;
    let body = r.bytes().await?;
    let text = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    parse_sha256(&text, name).with_context(|| format!("reading {url}"))
}

/// What an operator's file looks like this round.
#[derive(Debug)]
pub enum File {
    /// the same as what was published last
    Unchanged,
    /// new or changed since the last round (this is how it looks now); it is published
    /// once it looks the same two rounds running and hasn't been touched for two minutes,
    /// so a file still being copied in is never published half-written
    Changing(String),
    Ready(Got),
}

pub fn by_file(
    it: &ItemCfg,
    f: &Path,
    last: Option<&str>,
    candidate: Option<&str>,
    now: i64,
) -> Result<File> {
    let m =
        std::fs::metadata(f).with_context(|| format!("waiting for the file {}", f.display()))?;
    let mtime = m
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let ident = format!("{} {mtime}", m.len());
    if last == Some(ident.as_str()) {
        return Ok(File::Unchanged);
    }
    if m.len() > it.max_bytes() {
        bail!("{} is bigger than max_mb = {}", f.display(), it.max_mb);
    }
    if candidate != Some(ident.as_str()) || now - mtime < SETTLED_MS {
        return Ok(File::Changing(ident));
    }
    Ok(File::Ready(Got {
        path: f.to_path_buf(),
        name: f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        version: ms_to_rfc3339(mtime)[..10].to_string(),
        ident,
        source: "a file supplied by the site's operator".into(),
        sha256: String::new(),
        hash_from: None,
        keep: true,
    }))
}

/// The newest Protomaps daily build from their build list: (url, "2026-11-18").
pub fn latest_build(list: &Value) -> Option<(String, String)> {
    let key = list
        .as_array()?
        .iter()
        .filter_map(|b| b.get("key")?.as_str())
        .filter(|k| {
            k.len() == 16 && k.ends_with(".pmtiles") && k[..8].bytes().all(|c| c.is_ascii_digit())
        })
        .max()?;
    Some((
        format!("https://build.protomaps.com/{key}"),
        format!("{}-{}-{}", &key[..4], &key[4..6], &key[6..8]),
    ))
}

/// The command with `{build}` and `{out}` filled in.
pub fn expand(cmd: &[String], build: Option<&str>, out: &str) -> Vec<String> {
    cmd.iter()
        .map(|a| {
            a.replace("{build}", build.unwrap_or(""))
                .replace("{out}", out)
        })
        .collect()
}

async fn by_command(
    http: &reqwest::Client,
    it: &ItemCfg,
    dl: &Path,
    last: Option<&str>,
) -> Result<Option<Got>> {
    let build = if it.command.iter().any(|a| a.contains("{build}")) {
        let list: Value = http
            .get(BUILDS)
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Some(latest_build(&list).context("no daily builds in the Protomaps build list")?)
    } else {
        None
    };
    let shown = expand(&it.command, build.as_ref().map(|b| b.0.as_str()), "{out}").join(" ");
    // the same build cut the same way is the same file; a command without a build
    // reruns every check_days
    let ident = match &build {
        Some(_) => shown.clone(),
        None => format!("{shown} {}", now_ms()),
    };
    if last == Some(ident.as_str()) {
        return Ok(None);
    }
    let out = dl.join(format!("{}.{}", it.id, it.kind.ext_for("")?));
    let _ = std::fs::remove_file(&out);
    let args = expand(
        &it.command,
        build.as_ref().map(|b| b.0.as_str()),
        &out.to_string_lossy(),
    );
    run(
        &args,
        Duration::from_secs(it.timeout_min * 60),
        &dl.join(format!("{}.log", it.id)),
        Some((&out, it.max_bytes())),
    )
    .await?;
    let size = std::fs::metadata(&out)
        .with_context(|| format!("{} did not write {}", args[0], out.display()))?
        .len();
    if size > it.max_bytes() {
        let _ = std::fs::remove_file(&out);
        bail!(
            "{} wrote {} MB, more than max_mb = {}",
            args[0],
            size / 1_000_000,
            it.max_mb
        );
    }
    Ok(Some(Got {
        name: out
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        path: out,
        version: build
            .map(|b| b.1)
            .unwrap_or_else(|| ms_to_rfc3339(now_ms())[..10].to_string()),
        ident,
        source: shown.chars().take(300).collect(),
        sha256: String::new(),
        hash_from: None,
        keep: false,
    }))
}

/// Run a command to completion, giving up after `limit`, or as soon as the file it writes
/// grows past a cap. Its output goes to `log`.
pub async fn run(
    args: &[String],
    limit: Duration,
    log: &Path,
    cap: Option<(&Path, u64)>,
) -> Result<()> {
    let out = std::fs::File::create(log)?;
    let mut child = std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .with_context(|| format!("starting {}", args[0]))?;
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait()? {
            if st.success() {
                return Ok(());
            }
            bail!("{} failed ({st}): {}", args[0], tail(log));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} ran longer than {} min and was stopped",
                args[0],
                limit.as_secs() / 60
            );
        }
        if let Some((out, max)) = cap
            && std::fs::metadata(out).is_ok_and(|m| m.len() > max)
        {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(out);
            bail!("{} wrote more than max_mb and was stopped", args[0]);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn tail(log: &Path) -> String {
    let mut s = String::new();
    let _ = std::fs::File::open(log).and_then(|mut f| f.read_to_string(&mut s));
    let s = s.trim();
    let start = s.char_indices().rev().nth(300).map_or(0, |(i, _)| i);
    s[start..].to_string()
}

async fn publish(app: &App, it: &ItemCfg, g: Got) -> Result<()> {
    let ext = it.kind.ext_for(&g.name)?;
    let imports = &app.cfg.core.imports_dir;
    std::fs::create_dir_all(imports).with_context(|| format!("creating {}", imports.display()))?;
    let import_name = format!("{}_{}.{ext}", it.id, g.version);
    let import = imports.join(&import_name);
    let (from, to) = (g.path.clone(), import.clone());
    tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_file(&to);
        std::fs::hard_link(&from, &to).or_else(|_| copy_file(&from, &to))
    })
    .await??;
    let added = app
        .core
        .blob_add(&import_name, &blob_name(&it.id, &g.version), it.priority)
        .await;
    let _ = std::fs::remove_file(&import);
    let root = added?;
    if !crate::catalog::is_hash(&root) {
        bail!("the core answered with {root:?} instead of a blob id");
    }
    let now = now_ms();
    let unchanged = crate::mirror::held_one(&app.db(), &it.id)?.is_some_and(|h| h.root == root);
    if unchanged {
        // the operator touched the file but its content is the same
        app.db().execute(
            "UPDATE sources SET ident = ?2, checked_ms = ?3, candidate = NULL WHERE item = ?1",
            params![it.id, g.ident, now],
        )?;
        if !g.keep {
            let _ = std::fs::remove_file(&g.path);
        }
        return Ok(());
    }
    let size = std::fs::metadata(&g.path)?.len();
    let dest = app
        .cfg
        .content_dir()
        .join(file_name(&it.id, &g.version, &root, &ext));
    let (from, to, keep) = (g.path.clone(), dest.clone(), g.keep);
    tokio::task::spawn_blocking(move || {
        if keep {
            copy_file(&from, &to)
        } else {
            move_file(&from, &to)
        }
    })
    .await??;
    if !g.keep {
        let _ = std::fs::remove_file(g.path.with_extension("ident"));
    }
    let ev = json!({
        "item": it.id,
        "kind": it.kind.name(),
        "ext": ext,
        "title": it.title,
        "version": g.version,
        "size": size,
        "root": root,
        "source": g.source,
        "sha256": g.sha256,
        "hash_from": g.hash_from,
        "tiers": it.tiers,
        "group": it.group,
        "fetched": ms_to_rfc3339(now),
    });
    let mut outbox = vec![json!(["library.item", ev])];
    if !app.cfg.mule.to.is_empty() {
        let call = app.whoami().await?.call;
        let item = crate::catalog::parse_item(&ev).context("the item event does not parse")?;
        for island in &app.cfg.mule.to {
            let m = crate::mule::message(&item, &call, &app.cfg.mule.island, island);
            outbox.push(json!(["mule.msg", m]));
        }
    }
    // saved before it is sent, so a core that is down right now gets it next round
    let outbox = Value::Array(outbox).to_string();
    app.db().execute(
        "INSERT INTO sources (item, ident, checked_ms, outbox) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item) DO UPDATE SET ident = excluded.ident, outbox = excluded.outbox,
             candidate = NULL",
        params![it.id, g.ident, now, outbox],
    )?;
    install(
        app,
        &Held {
            item: it.id.clone(),
            kind: it.kind,
            ext,
            version: g.version.clone(),
            root,
            path: dest,
            size,
            sha256: g.sha256.clone(),
            verified: if g.hash_from.is_some() {
                "publisher"
            } else {
                "fetcher"
            }
            .into(),
            event_ts: now,
            since_ms: now,
            checked_ms: now,
        },
        false,
    )
    .await?;
    tracing::info!("published {} {} ({} bytes)", it.id, g.version, size);
    flush(app, &it.id, &outbox).await
}

/// Log the events a publish left to send, dropping each from the outbox once the core has
/// it, so a core that was down gets the rest next round and nothing is sent twice.
async fn flush(app: &App, item: &str, outbox: &str) -> Result<()> {
    let mut left: Vec<Value> = serde_json::from_str(outbox)?;
    while let Some(e) = left.first() {
        let (Some(kind), Some(payload)) = (e[0].as_str(), e.get(1)) else {
            left.remove(0);
            continue;
        };
        app.core.append(kind, payload.clone()).await?;
        left.remove(0);
        let rest = (!left.is_empty()).then(|| Value::Array(left.clone()).to_string());
        app.db().execute(
            "UPDATE sources SET outbox = ?2 WHERE item = ?1",
            params![item, rest],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn data() -> Vec<u8> {
        let mut d = vec![0x5a, 0x49, 0x4d, 0x04];
        d.extend((0..200_000u32).map(|i| (i * 7 % 251) as u8));
        d
    }

    /// A file server that honours Range (or not) and redirects a short name to a dated one.
    async fn server(ranges: bool) -> String {
        let honour = Arc::new(AtomicBool::new(ranges));
        let app = axum::Router::new()
            .route(
                "/wiki_en_mini.zim",
                axum::routing::get(|| async {
                    axum::response::Redirect::temporary("/files/wiki_en_mini_2026-08.zim")
                }),
            )
            .route(
                "/files/wiki_en_mini_2026-08.zim",
                axum::routing::get(move |h: HeaderMap| {
                    let honour = honour.load(Ordering::Relaxed);
                    async move {
                        let d = data();
                        let lm = [(header::LAST_MODIFIED, "Wed, 18 Nov 2026 08:55:27 GMT")];
                        let from =
                            h.get(header::RANGE)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|r| {
                                    r.strip_prefix("bytes=")?
                                        .strip_suffix('-')?
                                        .parse::<usize>()
                                        .ok()
                                });
                        match from.filter(|_| honour) {
                            Some(n) => (
                                StatusCode::PARTIAL_CONTENT,
                                lm,
                                [(
                                    header::CONTENT_RANGE,
                                    format!("bytes {n}-{}/{}", d.len() - 1, d.len()),
                                )],
                                d[n..].to_vec(),
                            )
                                .into_response(),
                            None => (lm, d).into_response(),
                        }
                    }
                }),
            )
            .route(
                "/files/wiki_en_mini_2026-08.zim.sha256",
                axum::routing::get(|| async {
                    format!(
                        "{}  wiki_en_mini_2026-08.zim\n",
                        crate::sha256::hex_of(&data())
                    )
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        base
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn resolves_redirects_to_the_dated_file() {
        let base = server(true).await;
        let r = resolve(&http(), &format!("{base}/wiki_en_mini.zim"))
            .await
            .unwrap();
        assert_eq!(r.name, "wiki_en_mini_2026-08.zim");
        assert_eq!(r.size, Some(data().len() as u64));
        assert_eq!(r.version(), "2026-08");
        assert_eq!(
            r.ident(),
            format!("wiki_en_mini_2026-08.zim {}", data().len())
        );
    }

    #[tokio::test]
    async fn resumes_a_partial_download() {
        let base = server(true).await;
        let url = format!("{base}/wiki_en_mini.zim");
        let d = tempfile::tempdir().unwrap();
        let part = d.path().join("w.part");
        let r = resolve(&http(), &url).await.unwrap();
        // an earlier attempt got the first 70 000 bytes
        std::fs::write(&part, &data()[..70_000]).unwrap();
        std::fs::write(part.with_extension("ident"), r.ident()).unwrap();
        download(&http(), &url, &r, &part, &r.ident(), 10_000_000)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), data());
    }

    #[tokio::test]
    async fn a_server_that_ignores_range_gets_a_clean_restart() {
        let base = server(false).await;
        let url = format!("{base}/wiki_en_mini.zim");
        let d = tempfile::tempdir().unwrap();
        let part = d.path().join("w.part");
        let r = resolve(&http(), &url).await.unwrap();
        std::fs::write(&part, b"garbage from before").unwrap();
        std::fs::write(part.with_extension("ident"), r.ident()).unwrap();
        download(&http(), &url, &r, &part, &r.ident(), 10_000_000)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), data());
    }

    #[tokio::test]
    async fn a_part_from_another_file_is_not_resumed() {
        let base = server(true).await;
        let url = format!("{base}/wiki_en_mini.zim");
        let d = tempfile::tempdir().unwrap();
        let part = d.path().join("w.part");
        let r = resolve(&http(), &url).await.unwrap();
        std::fs::write(&part, b"the start of last month's file").unwrap();
        std::fs::write(part.with_extension("ident"), "wiki_en_mini_2026-07.zim 5").unwrap();
        download(&http(), &url, &r, &part, &r.ident(), 10_000_000)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), data());
    }

    #[tokio::test]
    async fn refuses_more_than_max_mb() {
        let base = server(true).await;
        let url = format!("{base}/wiki_en_mini.zim");
        let d = tempfile::tempdir().unwrap();
        let part = d.path().join("w.part");
        let mut r = resolve(&http(), &url).await.unwrap();
        // a server that doesn't say how big the file is still can't push past the cap
        r.size = None;
        let err = download(&http(), &url, &r, &part, "x", 100_000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max_mb"), "{err}");
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn publisher_checksum_is_read_and_matched() {
        let base = server(true).await;
        let sum = publisher_sha256(
            &http(),
            &format!("{base}/files/wiki_en_mini_2026-08.zim.sha256"),
            "wiki_en_mini_2026-08.zim",
        )
        .await
        .unwrap();
        assert_eq!(sum, crate::sha256::hex_of(&data()));
        assert!(parse_sha256(&format!("{sum}  other.zim"), "wiki_en_mini_2026-08.zim").is_err());
        assert!(parse_sha256("<html>", "x").is_err());
        assert_eq!(
            parse_sha256(&format!("{}\n", sum.to_uppercase()), "x").unwrap(),
            sum
        );
    }

    #[test]
    fn newest_protomaps_build() {
        let list = serde_json::json!([
            {"key": "20261117.pmtiles", "size": 1, "b3sum": "aa"},
            {"key": "20261118.pmtiles", "size": 1, "b3sum": "bb"},
            {"key": "v4.pmtiles"},
            {"key": "20261116.pmtiles"}
        ]);
        let (url, version) = latest_build(&list).unwrap();
        assert_eq!(url, "https://build.protomaps.com/20261118.pmtiles");
        assert_eq!(version, "2026-11-18");
        assert!(latest_build(&serde_json::json!([])).is_none());
    }

    #[test]
    fn command_placeholders() {
        let cmd: Vec<String> = ["pmtiles", "extract", "{build}", "{out}", "--maxzoom=12"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            expand(&cmd, Some("https://b/x.pmtiles"), "/d/out.pmtiles"),
            [
                "pmtiles",
                "extract",
                "https://b/x.pmtiles",
                "/d/out.pmtiles",
                "--maxzoom=12"
            ]
        );
    }

    #[test]
    fn when_to_look_again() {
        let it: ItemCfg = toml::from_str(
            "id = \"w\"\nkind = \"zim\"\ntitle = \"W\"\nurl = \"https://x/w.zim\"\nmax_mb = 1\ncheck_days = 7",
        )
        .unwrap();
        let now = 100 * DAY;
        assert!(due(&it, 0, 0, now));
        assert!(!due(&it, now - DAY, 0, now));
        assert!(due(&it, now - 7 * DAY, 0, now));
        assert!(due(&it, now - RETRY_MS, 1, now));
        assert!(!due(&it, now - 60_000, 1, now));
        // each failure in a row doubles the wait, up to a day
        assert!(!due(&it, now - RETRY_MS, 3, now));
        assert!(due(&it, now - 4 * RETRY_MS, 3, now));
        assert!(due(&it, now - DAY, 40, now));
        assert!(!due(&it, now - DAY + 60_000, 40, now));
        let mut own = it.clone();
        own.url = None;
        own.file = Some("/srv/region.pmtiles".into());
        assert!(due(&own, now - 60_000, 0, now));
        assert!(
            !due(&own, now - 60_000, 1, now),
            "a missing file is not reported every minute"
        );
    }

    #[tokio::test]
    async fn commands_that_fail_say_why() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("x.log");
        let err = run(
            &["ridgewarden-no-such-command".to_string()],
            Duration::from_secs(5),
            &log,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("starting ridgewarden-no-such-command")
        );
    }

    #[tokio::test]
    async fn a_command_that_writes_too_much_is_stopped_while_it_runs() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("out.pmtiles");
        // keeps writing until stopped
        let cmd: Vec<String> = if cfg!(windows) {
            let s = format!(
                "$f = [IO.File]::OpenWrite('{}'); $b = New-Object byte[] 65536; while ($true) {{ $f.Write($b, 0, $b.Length); $f.Flush() }}",
                out.display()
            );
            vec![
                "powershell".into(),
                "-NoProfile".into(),
                "-Command".into(),
                s,
            ]
        } else {
            vec![
                "sh".into(),
                "-c".into(),
                format!("exec cat /dev/zero > '{}'", out.display()),
            ]
        };
        let started = Instant::now();
        let err = run(
            &cmd,
            Duration::from_secs(60),
            &d.path().join("x.log"),
            Some((&out, 1_000_000)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("more than max_mb"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!out.exists());
    }

    #[test]
    fn an_operator_file_waits_until_it_stops_changing() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("region.pmtiles");
        std::fs::write(&f, b"PMTiles\x03xyz").unwrap();
        let it: ItemCfg = toml::from_str(&format!(
            "id = \"m\"\nkind = \"pmtiles\"\ntitle = \"M\"\nfile = '{}'\nmax_mb = 1",
            f.display()
        ))
        .unwrap();
        let mtime = std::fs::metadata(&f)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // first sight: remember how it looks
        let File::Changing(ident) = by_file(&it, &f, None, None, mtime + 5 * 60_000).unwrap()
        else {
            panic!("published on first sight")
        };
        // same size and time a round later, but touched under two minutes ago: still waiting
        assert!(matches!(
            by_file(&it, &f, None, Some(&ident), mtime + 60_000).unwrap(),
            File::Changing(_)
        ));
        let File::Ready(g) = by_file(&it, &f, None, Some(&ident), mtime + 5 * 60_000).unwrap()
        else {
            panic!("a settled file should be published")
        };
        assert!(g.keep && g.name == "region.pmtiles");
        // once published, the same file is left alone
        assert!(matches!(
            by_file(&it, &f, Some(&ident), Some(&ident), mtime + 9 * 60_000).unwrap(),
            File::Unchanged
        ));
    }
}
