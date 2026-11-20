//! The running app: its config, its database, its line to the core and to the internet.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;
use tokio::sync::Notify;

use ridgewarden_client::mesh::Mesh;
use tokio::sync::Semaphore;

use crate::catalog::{Items, Sites};
use crate::config::Config;
use crate::mule::Mule;
use crate::nws::{self, Alerts};

pub const PROJECTIONS: &[&dyn Projection] = &[&Items, &Sites, &Alerts, &Mule, &Mesh];

/// Bytes of file data all requests together may hold in memory at once, in KiB permits.
pub const READ_BUDGET_KIB: usize = 48 * 1024;

/// This site's own state, kept out of the projections so a feed rebuild can't lose it.
const LOCAL_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS held (
    item      TEXT PRIMARY KEY,
    kind      TEXT NOT NULL,
    version   TEXT NOT NULL,
    root      TEXT NOT NULL,
    path      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    verified  TEXT NOT NULL,
    event_ts  INTEGER NOT NULL,
    since_ms  INTEGER NOT NULL,
    ext       TEXT NOT NULL,
    sha256    TEXT NOT NULL,
    checked_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pending (
    item     TEXT PRIMARY KEY,
    version  TEXT NOT NULL,
    root     TEXT NOT NULL,
    state    TEXT NOT NULL,
    detail   TEXT,
    since_ms INTEGER NOT NULL,
    size     INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_ms  INTEGER NOT NULL DEFAULT 0,
    -- the peers the core was last told to fetch from first, nearest first (JSON)
    sources  TEXT
);
-- blobs this site asked its core for, let go once nothing here needs them
CREATE TABLE IF NOT EXISTS wanted (
    root TEXT PRIMARY KEY,
    ms   INTEGER NOT NULL
);
-- replaced copies, deleted once nothing (kiwix-serve) may still need them
CREATE TABLE IF NOT EXISTS retired (
    path     TEXT PRIMARY KEY,
    kind     TEXT NOT NULL,
    since_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sources (
    item       TEXT PRIMARY KEY,
    ident      TEXT,
    checked_ms INTEGER NOT NULL DEFAULT 0,
    error      TEXT,
    outbox     TEXT,
    -- an operator's file seen last round, published once it stops changing
    candidate  TEXT,
    -- failed tries in a row, for the backoff
    fails      INTEGER NOT NULL DEFAULT 0
);
"#;

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// downloads from the internet: long transfers, so a stall timeout, not a total one
    pub web: reqwest::Client,
    /// the core's blob list
    pub local: reqwest::Client,
    pub nws_http: reqwest::Client,
    pub nws: Mutex<nws::State>,
    /// something this site holds or knows changed; send a heartbeat now
    pub changed: Notify,
    /// the set of ZIM files changed; restart kiwix-serve
    pub zims_changed: Notify,
    pub kiwix: crate::kiwix::Kiwix,
    /// file reads held in memory by the web server, in KiB
    pub reads: Arc<Semaphore>,
    who: Mutex<Option<Whoami>>,
}

/// Add columns an older database of this app lacks.
fn add_missing(db: &Connection, cols: &[(&str, &str, &str)]) -> Result<()> {
    for (table, col, decl) in cols {
        let has: bool = db.query_row(
            &format!("SELECT count(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
            [col],
            |r| r.get(0),
        )?;
        if !has {
            db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
        }
    }
    Ok(())
}

pub fn agent() -> String {
    concat!("ridgewarden-library/", env!("CARGO_PKG_VERSION")).to_string()
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let token = cfg
            .core
            .token_file
            .exists()
            .then_some(cfg.core.token_file.as_path());
        if token.is_none() {
            tracing::warn!(
                "core token {} not found; this site can mirror nothing and publish nothing",
                cfg.core.token_file.display()
            );
        }
        let core = CoreClient::new(&cfg.core.url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(LOCAL_SCHEMA)?;
        add_missing(
            &db,
            &[
                ("pending", "sources", "TEXT"),
                ("sites", "core_url", "TEXT"),
            ],
        )?;
        std::fs::create_dir_all(cfg.content_dir())
            .with_context(|| format!("creating {}", cfg.content_dir().display()))?;
        let web = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .user_agent(agent())
            .build()?;
        let local = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()?;
        let nws_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(30))
            .user_agent(format!("{} {}", agent(), cfg.nws.user_agent.trim()))
            .build()?;
        Ok(Self {
            cfg,
            core,
            db: Mutex::new(db),
            web,
            local,
            nws_http,
            nws: Mutex::new(nws::State::default()),
            changed: Notify::new(),
            zims_changed: Notify::new(),
            kiwix: crate::kiwix::Kiwix::default(),
            reads: Arc::new(Semaphore::new(READ_BUDGET_KIB)),
            who: Mutex::new(None),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The core's key and node, cached once the node is known.
    pub async fn whoami(&self) -> Result<Whoami> {
        if let Some(w) = self.who.lock().unwrap().clone()
            && !w.node.is_empty()
        {
            return Ok(w);
        }
        let w = self.core.whoami().await?;
        *self.who.lock().unwrap() = Some(w.clone());
        Ok(w)
    }

    pub fn nws_state(&self) -> MutexGuard<'_, nws::State> {
        self.nws.lock().unwrap_or_else(|e| e.into_inner())
    }
}
