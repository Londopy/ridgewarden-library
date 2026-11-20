//! `library.toml`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::content::{Kind, valid_item};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// friendly site names: node name -> label, e.g. "tst-cuesta" = "Cuesta Peak"
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
    /// Content this site fetches from the internet and publishes to the mesh. Only a site
    /// with internet (a fetcher) lists any.
    #[serde(default, rename = "item")]
    pub items: Vec<ItemCfg>,
    #[serde(default)]
    pub mirror: MirrorCfg,
    #[serde(default)]
    pub nws: NwsCfg,
    #[serde(default)]
    pub kiwix: KiwixCfg,
    #[serde(default)]
    pub mule: MuleCfg,
    /// pages warn that a copy is old once it was fetched more than this many days ago
    #[serde(default = "d_stale")]
    pub stale_days: u64,
    /// re-read every served file this often to catch a failing disk
    #[serde(default = "d_scrub")]
    pub scrub_days: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to publish content, alerts and this site's holdings
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    /// the core's import folder (`<core data_dir>/imports`); this app writes files there
    /// for the core to publish
    #[serde(default = "d_imports")]
    pub imports_dir: PathBuf,
    /// how other sites' cores reach this core, e.g. "http://tst-paso-pi.local.mesh:8790" (the
    /// core's own public_url); sent in heartbeats so nearby sites copy from this one
    #[serde(default)]
    pub public_url: Option<String>,
}

/// One piece of content to fetch: from a URL, by running a command, or from a file the
/// operator puts in place. Exactly one of `url`, `command` and `file`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ItemCfg {
    /// stable name for this item across versions, e.g. "wikipedia-en-mini"
    pub id: String,
    pub kind: Kind,
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
    /// the publisher's checksum file ("<hash>  <name>"); `{name}` is the downloaded file's
    /// name, e.g. "https://download.kiwix.org/zim/wikipedia/{name}.sha256"
    #[serde(default)]
    pub sha256_url: Option<String>,
    /// a command that writes the file to `{out}`; `{build}` is the newest Protomaps daily build
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// refuse anything bigger; there is no default, so nobody downloads 100 GB by accident
    pub max_mb: u64,
    /// how often to look for a new version
    #[serde(default = "d_check_days")]
    pub check_days: u64,
    /// give up on a command that runs longer than this
    #[serde(default = "d_timeout_min")]
    pub timeout_min: u64,
    /// blob fetch priority at other sites, 0 (first) to 9; alerts never wait on content
    #[serde(default = "d_priority")]
    pub priority: u8,
    /// which site tiers copy it (see mirror.tier); empty means every site
    #[serde(default)]
    pub tiers: Vec<String>,
    /// items in one group are alternatives (Wikipedia mini and nopic): a site holds only
    /// the biggest one its tier and limits allow
    #[serde(default)]
    pub group: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct MirrorCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// item ids to copy here; empty means every item a trusted station publishes
    #[serde(default)]
    pub items: Vec<String>,
    /// never copy an item bigger than this; the copy needs about twice this much disk
    #[serde(default = "d_mirror_max")]
    pub max_mb: u64,
    /// everything this site serves and is copying together stays under this
    #[serde(default = "d_total")]
    pub total_mb: u64,
    /// never start a copy that would leave less free space than this
    #[serde(default = "d_keep_free")]
    pub keep_free_mb: u64,
    /// this site's tier, e.g. "hub" or "small"; items name the tiers that copy them
    #[serde(default)]
    pub tier: String,
    /// wait for a library site on the best path to a copy to fetch first, so each radio
    /// hop carries the content once
    #[serde(default = "d_true")]
    pub fanout: bool,
    /// ...but no longer than this after the version was published
    #[serde(default = "d_fanout_wait")]
    pub fanout_wait_min: u64,
    /// how many different trusted fetchers must have published the same file before it
    /// is copied here
    #[serde(default = "d_one")]
    pub confirmations: u32,
}

/// Telling the data-mule app about new versions, so cars carry them to other islands.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct MuleCfg {
    /// this site's island, as the mule app names it
    #[serde(default)]
    pub island: String,
    /// islands to send each new version to; empty means none
    #[serde(default)]
    pub to: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct NwsCfg {
    /// this site has internet and may fetch alerts; only one such site per partition does
    #[serde(default)]
    pub enabled: bool,
    /// one of area (state or marine area, "CA"), zone ("CAZ034") or point ("35.28,-120.66")
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub zone: String,
    #[serde(default)]
    pub point: String,
    /// required by api.weather.gov: who you are and how to reach you
    #[serde(default)]
    pub user_agent: String,
    #[serde(default = "d_poll")]
    pub poll_s: u64,
    /// a fetcher whose heartbeat is older than this has gone quiet; the next one takes over
    #[serde(default = "d_quiet")]
    pub quiet_s: u64,
    /// the NWS alerts endpoint; change it only to test against a local copy
    #[serde(default = "d_nws_api")]
    pub api_url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct KiwixCfg {
    /// start kiwix-serve on the current ZIM files; `{zims}` becomes their paths
    #[serde(default)]
    pub command: Vec<String>,
    /// where people reach kiwix-serve, linked from the pages
    #[serde(default)]
    pub url: String,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-library")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8795".parse().unwrap()
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_imports() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/imports")
}
fn d_check_days() -> u64 {
    30
}
fn d_timeout_min() -> u64 {
    240
}
fn d_priority() -> u8 {
    8
}
fn d_true() -> bool {
    true
}
fn d_mirror_max() -> u64 {
    16_000
}
fn d_poll() -> u64 {
    60
}
fn d_nws_api() -> String {
    "https://api.weather.gov/alerts/active".into()
}
fn d_quiet() -> u64 {
    900
}
fn d_stale() -> u64 {
    180
}
fn d_scrub() -> u64 {
    30
}
fn d_total() -> u64 {
    40_000
}
fn d_keep_free() -> u64 {
    2_000
}
fn d_fanout_wait() -> u64 {
    180
}
fn d_one() -> u32 {
    1
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
            imports_dir: d_imports(),
            public_url: None,
        }
    }
}

impl Default for MirrorCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            items: vec![],
            max_mb: d_mirror_max(),
            total_mb: d_total(),
            keep_free_mb: d_keep_free(),
            tier: String::new(),
            fanout: true,
            fanout_wait_min: d_fanout_wait(),
            confirmations: 1,
        }
    }
}

impl Default for NwsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            area: String::new(),
            zone: String::new(),
            point: String::new(),
            user_agent: String::new(),
            poll_s: d_poll(),
            quiet_s: d_quiet(),
            api_url: d_nws_api(),
        }
    }
}

impl NwsCfg {
    /// The query for /alerts/active, e.g. ("area", "CA").
    pub fn query(&self) -> Option<(&'static str, &str)> {
        [
            ("area", self.area.as_str()),
            ("zone", self.zone.as_str()),
            ("point", self.point.as_str()),
        ]
        .into_iter()
        .find(|(_, v)| !v.is_empty())
    }

    /// What the heartbeat says this site watches, so only fetchers for the same place
    /// stand in for each other.
    pub fn place(&self) -> String {
        self.query()
            .map(|(k, v)| format!("{k}={v}"))
            .unwrap_or_default()
    }

    fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.user_agent.trim().is_empty() {
            bail!(
                "nws.user_agent is required: api.weather.gov refuses requests without a User-Agent \
                 and asks for contact details in it, e.g. \"(mesh library, ops@example.org)\""
            );
        }
        let set = [&self.area, &self.zone, &self.point]
            .iter()
            .filter(|v| !v.is_empty())
            .count();
        if set != 1 {
            bail!("set exactly one of nws.area, nws.zone and nws.point");
        }
        let ok = |s: &str, extra: &[u8]| {
            s.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || extra.contains(&b))
        };
        if !ok(&self.area, b"") || !ok(&self.zone, b",") || !point_ok(&self.point) {
            bail!(
                "nws.area looks like \"CA\", nws.zone like \"CAZ034\" and nws.point like \"35.28,-120.66\""
            );
        }
        if self.poll_s < 30 {
            bail!("nws.poll_s must be at least 30 (be polite to api.weather.gov)");
        }
        if self.quiet_s < 2 * crate::HEARTBEAT_S {
            bail!(
                "nws.quiet_s must be at least {} (two heartbeats)",
                2 * crate::HEARTBEAT_S
            );
        }
        Ok(())
    }
}

fn point_ok(p: &str) -> bool {
    if p.is_empty() {
        return true;
    }
    let mut parts = p.split(',');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    matches!((a.parse::<f64>(), b.parse::<f64>()), (Ok(lat), Ok(lon))
        if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
}

impl ItemCfg {
    fn validate(&self) -> Result<()> {
        if !valid_item(&self.id) {
            bail!(
                "item id {:?} must be 1-40 lowercase letters, digits and hyphens",
                self.id
            );
        }
        let sources = [
            self.url.is_some(),
            !self.command.is_empty(),
            self.file.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if sources != 1 {
            bail!("item {}: set exactly one of url, command and file", self.id);
        }
        if let Some(u) = &self.url
            && !u.starts_with("https://")
            && !u.starts_with("http://")
        {
            bail!("item {}: url must be http(s)", self.id);
        }
        if self.sha256_url.is_some() && self.url.is_none() {
            bail!("item {}: sha256_url goes with url", self.id);
        }
        if let Some(u) = &self.sha256_url
            && !u.starts_with("https://")
            && !u.starts_with("http://")
        {
            bail!("item {}: sha256_url must be http(s)", self.id);
        }
        if !self.command.is_empty() && !self.command.iter().any(|a| a.contains("{out}")) {
            bail!("item {}: the command must write to {{out}}", self.id);
        }
        if self.max_mb == 0 {
            bail!("item {}: max_mb must be at least 1", self.id);
        }
        if self.check_days == 0 || self.timeout_min == 0 {
            bail!(
                "item {}: check_days and timeout_min must be at least 1",
                self.id
            );
        }
        if self.priority > 9 {
            bail!("item {}: priority is 0 to 9", self.id);
        }
        if let Some(t) = self.tiers.iter().find(|t| !valid_item(t)) {
            bail!("item {}: tier {t:?} is not a tier name", self.id);
        }
        if !self.group.is_empty() && !valid_item(&self.group) {
            bail!(
                "item {}: group {:?} is not a group name",
                self.id,
                self.group
            );
        }
        if self.kind == Kind::Doc {
            let name = match (&self.url, &self.file) {
                (Some(u), _) => u.rsplit('/').next().unwrap_or(""),
                (_, Some(f)) => f.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                _ => bail!("item {}: a document comes from a url or a file", self.id),
            };
            self.kind
                .ext_for(name)
                .with_context(|| format!("item {}", self.id))?;
        }
        Ok(())
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_mb * 1_000_000
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        for (i, it) in self.items.iter().enumerate() {
            it.validate()?;
            if self.items[..i].iter().any(|o| o.id == it.id) {
                bail!("item id {} is listed twice", it.id);
            }
        }
        if let Some(bad) = self.mirror.items.iter().find(|i| !valid_item(i)) {
            bail!("mirror.items: {bad:?} is not an item id");
        }
        if !self.kiwix.command.is_empty() && !self.kiwix.command.iter().any(|a| a == "{zims}") {
            bail!("kiwix.command needs a \"{{zims}}\" argument for the ZIM files");
        }
        if let Some(u) = &self.core.public_url
            && crate::catalog::peer_url(u).is_none()
        {
            bail!("core.public_url looks like \"http://tst-paso-pi.local.mesh:8790\", not {u:?}");
        }
        if !self.mirror.tier.is_empty() && !valid_item(&self.mirror.tier) {
            bail!("mirror.tier {:?} is not a tier name", self.mirror.tier);
        }
        if self.mirror.confirmations == 0 {
            bail!("mirror.confirmations must be at least 1");
        }
        if self.scrub_days == 0 || self.stale_days == 0 {
            bail!("scrub_days and stale_days must be at least 1");
        }
        self.mule.validate()?;
        self.nws.validate()
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    pub fn content_dir(&self) -> PathBuf {
        self.data_dir.join("content")
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| ridgewarden_client::util::norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }

    /// Why this site doesn't copy an item, or None if it does (limits on the total and on
    /// free disk are checked when the copy starts).
    pub fn refuses(&self, item: &str, size: u64, tiers: &[String]) -> Option<String> {
        if !self.mirror.enabled {
            return Some("this site doesn't mirror".into());
        }
        if !self.mirror.items.is_empty() && !self.mirror.items.iter().any(|i| i == item) {
            return Some("not in mirror.items".into());
        }
        if !tiers.is_empty() && !tiers.contains(&self.mirror.tier) {
            return Some(format!("only for {} sites", tiers.join(", ")));
        }
        if size > self.mirror.max_mb * 1_000_000 {
            return Some(format!(
                "bigger than mirror.max_mb = {}",
                self.mirror.max_mb
            ));
        }
        if size > self.mirror.total_mb * 1_000_000 {
            return Some(format!(
                "bigger than mirror.total_mb = {}",
                self.mirror.total_mb
            ));
        }
        None
    }
}

impl MuleCfg {
    fn validate(&self) -> Result<()> {
        let island = |i: &str| ridgewarden_client::util::valid_node(i);
        if self.to.is_empty() {
            return Ok(());
        }
        if !island(&self.island) {
            bail!("mule.island must be set (the island name the mule app uses) to send to mule.to");
        }
        if let Some(bad) = self.to.iter().find(|t| !island(t) || **t == self.island) {
            bail!("mule.to: {bad:?} is not another island's name");
        }
        Ok(())
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-library configuration
data_dir = "/var/lib/ridgewarden-library"
listen = "0.0.0.0:8795"
# pages warn that a copy is old once it was fetched more than this many days ago
stale_days = 180
# re-read every served file this often, to catch a failing disk before someone needs it
scrub_days = 30

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
# the core's import folder; this app puts files there for the core to publish
imports_dir = "/var/lib/ridgewarden/imports"
# how other sites reach this core (its public_url), so sites near this one copy from it
# public_url = "http://tst-cuesta-pi.local.mesh:8790"

[site_names]
# "tst-cuesta" = "Cuesta Peak"

# Copy content other sites publish. Each copy needs about twice its size in disk:
# once in the core's blob store and once as the file that is served.
[mirror]
enabled = true
# item ids to copy here; empty means everything published by a station the core trusts
items = []
# never copy one item bigger than this...
max_mb = 16000
# ...or let everything served and being copied here go past this
total_mb = 40000
# and never start a copy that would leave less free disk than this
keep_free_mb = 2000
# this site's tier; items can be limited to tiers, e.g. Wikipedia without pictures to "hub"
tier = ""
# wait while a library site on the best path to a copy is still fetching, then have the
# core fetch from the nearest sites that hold it first, so each radio hop carries it once;
# after fanout_wait_min, copy from wherever
fanout = true
fanout_wait_min = 180
# how many different trusted fetchers must have published the same file first
confirmations = 1

# Only a site with internet (a fetcher) lists items. Nothing is downloaded unless it is
# listed here, and nothing bigger than its max_mb. Start small: these are test-sized.
#
# [[item]]
# id = "wikipedia-en-test"
# kind = "zim"
# title = "Wikipedia (English, one-article test file)"
# # this short name redirects to the newest dated file
# url = "https://download.kiwix.org/zim/wikipedia_en_ray-charles_mini.zim"
# sha256_url = "https://download.kiwix.org/zim/wikipedia/{name}.sha256"
# max_mb = 50
# check_days = 30
# # the real thing once the test works, English Wikipedia mini (about 13 GB):
# # url = "https://download.kiwix.org/zim/wikipedia_en_all_mini.zim"
# # max_mb = 15000
# # group = "wikipedia-en"
#
# # without pictures (about 49 GB), for hubs only; hubs hold it instead of mini
# [[item]]
# id = "wikipedia-en-nopic"
# kind = "zim"
# title = "Wikipedia (English, no pictures)"
# url = "https://download.kiwix.org/zim/wikipedia_en_all_nopic.zim"
# sha256_url = "https://download.kiwix.org/zim/wikipedia/{name}.sha256"
# max_mb = 55000
# check_days = 60
# tiers = ["hub"]
# group = "wikipedia-en"
#
# [[item]]
# id = "map-region"
# kind = "pmtiles"
# title = "Regional map"
# # needs the pmtiles CLI (github.com/protomaps/go-pmtiles); it reads only the region
# # from the newest daily build. Keep the box and zoom small to start.
# command = ["pmtiles", "extract", "{build}", "{out}", "--bbox=-120.70,35.25,-120.60,35.32", "--maxzoom=12"]
# max_mb = 50
# check_days = 30
#
# # or a file you made yourself, published again whenever it changes:
# [[item]]
# id = "map-own"
# kind = "pmtiles"
# title = "Regional map"
# file = "/var/lib/ridgewarden-library/region.pmtiles"
# max_mb = 2000
#
# # forms, plans and phone lists served agencies ask for (pdf, txt, csv, md, odt, ods,
# # docx, xlsx, png, jpg); anyone on the mesh can read them
# [[item]]
# id = "shelter-phone-list"
# kind = "doc"
# title = "Shelter phone list"
# file = "/var/lib/ridgewarden-library/docs/shelter-phones.csv"
# max_mb = 5

# NWS alerts. Turn on at every site with internet; one of them fetches at a time.
[nws]
enabled = false
# one of: area = "CA", zone = "CAZ034", point = "35.28,-120.66"
area = ""
# required by api.weather.gov: say who you are and how to reach you
user_agent = ""
poll_s = 60
quiet_s = 900

# Wikipedia is served by kiwix-serve (kiwix.org). Either let this app run it on the
# current ZIM files, restarting it when a new version takes over...
[kiwix]
# if kiwix-serve won't start on a new file, the app goes back to the files that worked
# command = ["kiwix-serve", "--port=8796", "{zims}"]
# ...and/or give the address people use to reach it:
# url = "http://tst-cuesta-pi.local.mesh:8796/"

# Send each new version this site fetches to other islands with the data-mule app
# (ridgewarden-mule), which plans and loads it onto cars per trip. Don't add "library/"
# to a car core's [blobs] auto_want.
[mule]
# island = "north"
# to = ["south"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Config {
        toml::from_str(EXAMPLE).unwrap()
    }

    #[test]
    fn example_parses() {
        let c = example();
        c.validate().unwrap();
        assert!(
            c.items.is_empty(),
            "nothing downloads until the operator says so"
        );
    }

    #[test]
    fn commented_items_parse_once_uncommented() {
        let mut blocks = vec![];
        let mut lines = EXAMPLE.lines();
        while let Some(l) = lines.next() {
            if l != "# [[item]]" {
                continue;
            }
            let mut block = String::from("[[item]]\n");
            for l in lines.by_ref().take_while(|l| l.starts_with("# ")) {
                if !l.starts_with("# #") {
                    block.push_str(&l[2..]);
                    block.push('\n');
                }
            }
            blocks.push(block);
        }
        assert_eq!(blocks.len(), 5);
        for b in blocks {
            let c: Config = toml::from_str(&b).unwrap();
            c.validate().unwrap();
        }
    }

    #[test]
    fn nws_needs_a_user_agent() {
        let mut c = example();
        c.nws.enabled = true;
        c.nws.area = "CA".into();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("user_agent"), "{err}");
        c.nws.user_agent = "(test library, ops@example.org)".into();
        c.validate().unwrap();
        c.nws.point = "35.28,-120.66".into();
        assert!(c.validate().is_err(), "only one of area, zone, point");
        c.nws.area.clear();
        c.validate().unwrap();
        c.nws.point = "35.28;-120.66".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn items_need_one_source_and_a_size_cap() {
        let base = r#"
[[item]]
id = "x"
kind = "zim"
title = "X"
url = "https://example.org/x.zim"
max_mb = 10
"#;
        let c: Config = toml::from_str(base).unwrap();
        c.validate().unwrap();
        let no_cap = base.replace("max_mb = 10\n", "");
        assert!(toml::from_str::<Config>(&no_cap).is_err());
        let two = format!("{base}file = \"/tmp/x.zim\"\n");
        let c: Config = toml::from_str(&two).unwrap();
        assert!(c.validate().is_err());
        let bad_id = base.replace("id = \"x\"", "id = \"../x\"");
        let c: Config = toml::from_str(&bad_id).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn mirror_filter() {
        let mut c = example();
        assert_eq!(c.refuses("anything", 1_000, &[]), None);
        assert!(c.refuses("anything", 20_000_000_000, &[]).is_some());
        let hub = vec!["hub".to_string()];
        assert!(c.refuses("nopic", 1, &hub).unwrap().contains("hub"));
        c.mirror.tier = "hub".into();
        assert_eq!(c.refuses("nopic", 1, &hub), None);
        c.mirror.items = vec!["maps".into()];
        assert!(c.refuses("maps", 1, &[]).is_none() && c.refuses("wiki", 1, &[]).is_some());
        c.mirror.enabled = false;
        assert!(c.refuses("maps", 1, &[]).is_some());
    }

    #[test]
    fn mule_needs_an_island() {
        let mut c = example();
        c.mule.to = vec!["south".into()];
        assert!(c.validate().is_err());
        c.mule.island = "north".into();
        c.validate().unwrap();
        c.mule.to = vec!["north".into()];
        assert!(c.validate().is_err(), "sending to yourself");
    }
}
