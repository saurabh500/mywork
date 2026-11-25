use crate::{
    config::{Config, Ntfy},
    github::Source,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub created: DateTime<Utc>,
    pub draft: bool,
    pub head: String,
    pub activity: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub review: String,
    pub requested: Vec<String>,
}

impl PullRequest {
    pub fn key(&self) -> String {
        format!("{}#{}", self.repo.to_lowercase(), self.number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub key: String,
    pub kind: String,
    pub url: String,
}

impl Alert {
    pub fn message(&self) -> String {
        format!("{}: {}. {}", self.kind, self.key, self.url)
    }
}

pub trait Delivery {
    fn send(&self, alert: &Alert) -> Result<()>;
}

pub struct NtfyDelivery {
    client: reqwest::blocking::Client,
    url: reqwest::Url,
    token: Option<String>,
}

impl NtfyDelivery {
    pub fn new(config: &Ntfy) -> Result<Self> {
        if !config.enabled {
            bail!("ntfy delivery is disabled in configuration");
        }
        let topic = if let Some(name) = &config.topic_env {
            std::env::var(name)
                .with_context(|| format!("Set ntfy topic environment variable {name}"))?
        } else {
            config
                .topic
                .clone()
                .context("Configure ntfy topic or topic_env")?
        };
        if !crate::config::valid_topic(&topic) {
            bail!("ntfy topic must contain only letters, digits, hyphens, and underscores");
        }
        let token = config
            .token_env
            .as_ref()
            .map(|name| {
                let value = std::env::var(name)
                    .with_context(|| format!("Set ntfy token environment variable {name}"))?;
                if value.trim().is_empty() {
                    bail!("ntfy token must not be empty");
                }
                Ok(value)
            })
            .transpose()?;
        let url = reqwest::Url::parse(&format!(
            "{}/{}",
            config.server.trim_end_matches('/'),
            topic
        ))?;
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url,
            token,
        })
    }
}

impl Delivery for NtfyDelivery {
    fn send(&self, _alert: &Alert) -> Result<()> {
        let mut request = self
            .client
            .post(self.url.clone())
            .header("Title", "GitHub review watch")
            .body("A tracked pull request needs your attention. Open your local review dashboard.");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        // Strip the URL because it contains the secret topic.
        let response = request
            .send()
            .map_err(|error| error.without_url())
            .context("ntfy delivery failed")?;
        if !response.status().is_success() {
            bail!("ntfy delivery failed with HTTP {}", response.status());
        }
        Ok(())
    }
}

pub struct Store {
    pub connection: Connection,
    _lock: File,
}

#[derive(Debug)]
pub struct Row {
    pub pr: PullRequest,
    pub activity: DateTime<Utc>,
    pub pinned: bool,
    pub snooze_until: i64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lockfile");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .context("Open watcher lock; ensure the database directory exists")?;
        lock.try_lock_exclusive()
            .context("Another review-watch operation is running; retry after it finishes")?;
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS prs (
               key TEXT PRIMARY KEY, payload TEXT NOT NULL, head TEXT NOT NULL,
               activity INTEGER NOT NULL, quiet_sent INTEGER NOT NULL DEFAULT 0,
               pending TEXT, active INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
               snooze_until INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS scopes (key TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Self {
            connection,
            _lock: lock,
        })
    }

    pub fn rows(&self) -> Result<Vec<Row>> {
        let mut statement = self.connection.prepare(
            "SELECT payload,activity,pinned,snooze_until FROM prs WHERE active=1 ORDER BY pinned DESC,activity"
        )?;
        let raw = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        raw.map(|row| {
            let (payload, activity, pinned, snooze_until) = row?;
            Ok(Row {
                pr: serde_json::from_str(&payload)?,
                activity: DateTime::from_timestamp(activity, 0)
                    .context("Invalid stored activity time")?,
                pinned,
                snooze_until,
            })
        })
        .collect()
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO meta(key,value) VALUES (?1,?2)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn toggle_pin(&self, key: &str) -> Result<()> {
        self.connection
            .execute("UPDATE prs SET pinned=NOT pinned WHERE key=?1", [key])?;
        Ok(())
    }

    pub fn snooze(&self, key: &str, now: DateTime<Utc>) -> Result<()> {
        self.connection.execute(
            "UPDATE prs SET snooze_until=?2 WHERE key=?1",
            params![key, (now + Duration::hours(8)).timestamp()],
        )?;
        Ok(())
    }

    pub fn check(
        &mut self,
        config: &Config,
        source: &impl Source,
        delivery: Option<&dyn Delivery>,
        clock: impl FnOnce() -> DateTime<Utc>,
        dry_run: bool,
    ) -> Result<Vec<Alert>> {
        let fetched = source.fetch(config);
        let now = clock();
        let snapshot = match fetched {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if !dry_run {
                    self.set_meta("last_error", &format!("{now}: {error:#}"))?;
                }
                return Err(error);
            }
        };
        let transaction = self.connection.transaction()?;
        let alerts = reconcile(&transaction, config, snapshot, now)?;
        transaction.execute(
            "INSERT INTO meta(key,value) VALUES ('last_refresh',?1)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [now.to_rfc3339()],
        )?;
        transaction.execute("DELETE FROM meta WHERE key='last_error'", [])?;
        if dry_run {
            transaction.rollback()?;
            return Ok(alerts);
        }
        transaction.commit()?;
        if let Some(delivery) = delivery {
            let mut failures = Vec::new();
            for alert in &alerts {
                match delivery.send(alert) {
                    Ok(()) => {
                        if alert.kind == "Quiet PR" {
                            self.connection.execute(
                                "UPDATE prs SET quiet_sent=1 WHERE key=?1",
                                [&alert.key],
                            )?;
                        } else {
                            self.connection.execute(
                                "UPDATE prs SET pending=NULL WHERE key=?1",
                                [&alert.key],
                            )?;
                        }
                    }
                    Err(error) => failures.push(format!("{}: {error:#}", alert.key)),
                }
            }
            if !failures.is_empty() {
                let message = failures.join("; ");
                self.set_meta("last_error", &format!("{now}: {message}"))?;
                bail!("Notification failure (will retry): {message}");
            }
        }
        Ok(alerts)
    }
}

fn scope(repo: &str, author: &str) -> String {
    format!("{}:{}", repo.to_lowercase(), author.to_lowercase())
}

fn reconcile(
    connection: &Connection,
    config: &Config,
    snapshot: Vec<PullRequest>,
    now: DateTime<Utc>,
) -> Result<Vec<Alert>> {
    connection.execute("UPDATE prs SET active=0", [])?;
    let mut alerts = Vec::new();
    for pr in snapshot {
        if !config.watches(&pr.repo, &pr.author) {
            bail!("Snapshot contains an unwatched PR");
        }
        let key = pr.key();
        let previous: Option<(String, String, i64, bool, Option<String>, i64)> = connection.query_row(
            "SELECT payload,head,activity,quiet_sent,pending,snooze_until FROM prs WHERE key=?1", [&key],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))
        ).optional()?;
        let initialized = connection
            .query_row(
                "SELECT 1 FROM scopes WHERE key=?1",
                [scope(&pr.repo, &pr.author)],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        // First sight cannot establish the latest push time, so start conservatively now.
        let mut activity = pr.activity.timestamp().max(now.timestamp());
        let mut quiet_sent = false;
        let mut pending = if initialized && !pr.draft {
            Some("New PR".to_owned())
        } else {
            None
        };
        let mut snoozed = 0;
        if let Some((payload, head, last, sent, old_pending, until)) = previous {
            let old: PullRequest = serde_json::from_str(&payload)?;
            activity = pr.activity.timestamp().max(last);
            if head != pr.head {
                activity = activity.max(now.timestamp());
            }
            quiet_sent = sent && activity == last;
            pending = if !pr.draft && (old.draft || pr.ready_at > old.ready_at) {
                Some("Ready for review".into())
            } else {
                old_pending
            };
            snoozed = until;
        }
        if pr.draft {
            pending = None;
        }
        connection.execute(
            "INSERT INTO prs(key,payload,head,activity,quiet_sent,pending,active)
             VALUES (?1,?2,?3,?4,?5,?6,1)
             ON CONFLICT(key) DO UPDATE SET payload=excluded.payload,head=excluded.head,
             activity=excluded.activity,quiet_sent=excluded.quiet_sent,pending=excluded.pending,active=1",
            params![key,serde_json::to_string(&pr)?,pr.head,activity,quiet_sent,pending]
        )?;
        if pr.draft || snoozed > now.timestamp() {
            continue;
        }
        if let Some(kind) = pending {
            alerts.push(Alert {
                key,
                kind,
                url: pr.url,
            });
        } else if !quiet_sent
            && now.timestamp().saturating_sub(activity) >= i64::from(config.quiet_hours) * 3600
        {
            alerts.push(Alert {
                key,
                kind: "Quiet PR".into(),
                url: pr.url,
            });
        }
    }
    for repo in &config.repositories {
        for author in &config.authors {
            connection.execute(
                "INSERT OR IGNORE INTO scopes(key) VALUES (?1)",
                [scope(repo, author)],
            )?;
        }
    }
    connection.execute("UPDATE prs SET pending=NULL WHERE active=0", [])?;
    Ok(alerts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    impl Store {
        fn check_at(
            &mut self,
            config: &Config,
            source: &impl Source,
            delivery: Option<&dyn Delivery>,
            now: DateTime<Utc>,
            dry_run: bool,
        ) -> Result<Vec<Alert>> {
            self.check(config, source, delivery, || now, dry_run)
        }
    }

    struct Mock(RefCell<Vec<PullRequest>>);
    impl Source for Mock {
        fn fetch(&self, _: &Config) -> Result<Vec<PullRequest>> {
            Ok(self.0.borrow().clone())
        }
    }
    struct Sender {
        fail: Cell<bool>,
        sent: RefCell<Vec<Alert>>,
    }
    impl Delivery for Sender {
        fn send(&self, alert: &Alert) -> Result<()> {
            if self.fail.get() {
                bail!("offline");
            }
            self.sent.borrow_mut().push(alert.clone());
            Ok(())
        }
    }
    fn setup() -> (tempfile::TempDir, Config, Mock, Sender, DateTime<Utc>) {
        let dir = tempfile::tempdir().unwrap();
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let config = Config {
            repositories: vec!["org/repo".into()],
            authors: vec!["alice".into()],
            quiet_hours: 8,
            database: dir.path().join("state.sqlite3"),
            ntfy: None,
        };
        let pr = PullRequest {
            repo: "org/repo".into(),
            number: 1,
            title: "Private title".into(),
            url: "https://github.com/org/repo/pull/1".into(),
            author: "alice".into(),
            created: now,
            draft: false,
            head: "aaa".into(),
            activity: now,
            ready_at: None,
            review: "PENDING".into(),
            requested: vec![],
        };
        (
            dir,
            config,
            Mock(RefCell::new(vec![pr])),
            Sender {
                fail: Cell::new(false),
                sent: RefCell::new(vec![]),
            },
            now,
        )
    }
    #[test]
    fn threshold_first_sync_restart_and_rearm() -> Result<()> {
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), now, false)?
                .is_empty()
        );
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(8) - Duration::seconds(1),
                    false
                )?
                .is_empty()
        );
        assert_eq!(
            store.check_at(
                &cfg,
                &source,
                Some(&sender),
                now + Duration::hours(8),
                false
            )?[0]
                .kind,
            "Quiet PR"
        );
        drop(store);
        let mut store = Store::open(&cfg.database)?;
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(9),
                    false
                )?
                .is_empty()
        );
        source.0.borrow_mut()[0].head = "bbb".into();
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(10),
                    false
                )?
                .is_empty()
        );
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(18) - Duration::seconds(1),
                    false
                )?
                .is_empty()
        );
        assert_eq!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(18),
                    false
                )?
                .len(),
            1
        );
        source.0.borrow_mut()[0].activity = now + Duration::hours(19);
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(19),
                    false
                )?
                .is_empty()
        );
        assert_eq!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(27),
                    false
                )?
                .len(),
            1
        );
        assert_eq!(sender.sent.borrow().len(), 3);
        Ok(())
    }
    #[test]
    fn new_ready_failure_retry_and_no_private_title() -> Result<()> {
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        store.check_at(&cfg, &source, Some(&sender), now, false)?;
        source.0.borrow_mut()[0].number = 2;
        sender.fail.set(true);
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), now, false)
                .is_err()
        );
        drop(store);
        sender.fail.set(false);
        let mut store = Store::open(&cfg.database)?;
        let alerts = store.check_at(&cfg, &source, Some(&sender), now, false)?;
        assert_eq!(alerts[0].kind, "New PR");
        assert!(!alerts[0].message().contains("Private title"));
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), now, false)?
                .is_empty()
        );
        source.0.borrow_mut()[0].draft = true;
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(9),
                    false
                )?
                .is_empty()
        );
        {
            let mut prs = source.0.borrow_mut();
            prs[0].draft = false;
            prs[0].activity = now + Duration::hours(10);
            prs[0].ready_at = Some(now + Duration::hours(10));
        }
        assert_eq!(
            store.check_at(
                &cfg,
                &source,
                Some(&sender),
                now + Duration::hours(10),
                false
            )?[0]
                .kind,
            "Ready for review"
        );
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(10),
                    false
                )?
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn quiet_failure_retry_draft_closed_snooze_and_lock() -> Result<()> {
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        assert!(Store::open(&cfg.database).is_err());
        store.check_at(&cfg, &source, Some(&sender), now, false)?;
        store.toggle_pin("org/repo#1")?;
        store.snooze("org/repo#1", now + Duration::hours(7))?;
        drop(store);
        let mut store = Store::open(&cfg.database)?;
        assert!(store.rows()?[0].pinned);
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(8),
                    false
                )?
                .is_empty()
        );
        sender.fail.set(true);
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(15),
                    false
                )
                .is_err()
        );
        sender.fail.set(false);
        assert_eq!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(15),
                    false
                )?
                .len(),
            1
        );
        source.0.borrow_mut()[0].draft = true;
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(30),
                    false
                )?
                .is_empty()
        );
        source.0.borrow_mut().clear();
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(40),
                    false
                )?
                .is_empty()
        );
        assert!(store.rows()?.is_empty());
        Ok(())
    }
    #[test]
    fn dry_run_does_not_consume_baseline_or_alerts() -> Result<()> {
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        let later = now + Duration::hours(8);
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), later, true)?
                .is_empty()
        );
        assert!(store.rows()?.is_empty());
        assert!(store.meta("last_refresh")?.is_none());
        assert!(sender.sent.borrow().is_empty());
        store.check_at(&cfg, &source, Some(&sender), now, false)?;
        assert_eq!(
            store
                .check_at(&cfg, &source, Some(&sender), later, true)?
                .len(),
            1
        );
        assert!(sender.sent.borrow().is_empty());
        assert_eq!(
            store
                .check_at(&cfg, &source, Some(&sender), later, false)?
                .len(),
            1
        );
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), later, false)?
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn old_first_seen_pr_is_conservative_and_baselines_are_per_author() -> Result<()> {
        let (_dir, mut cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        source.0.borrow_mut()[0].created = now - Duration::days(30);
        source.0.borrow_mut()[0].activity = now - Duration::days(30);
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), now, false)?
                .is_empty()
        );
        assert_eq!(store.rows()?[0].activity, now);
        cfg.authors.push("bob".into());
        source.0.borrow_mut()[0].author = "bob".into();
        source.0.borrow_mut()[0].number = 2;
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), now, false)?
                .is_empty()
        );
        source.0.borrow_mut()[0].number = 3;
        assert_eq!(
            store.check_at(&cfg, &source, Some(&sender), now, false)?[0].kind,
            "New PR"
        );
        Ok(())
    }
    #[test]
    fn ready_event_between_polls_notifies_once_and_uses_event_time() -> Result<()> {
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        store.check_at(&cfg, &source, Some(&sender), now, false)?;
        {
            let mut prs = source.0.borrow_mut();
            prs[0].ready_at = Some(now + Duration::hours(1));
            prs[0].activity = now + Duration::hours(1);
        }
        assert_eq!(
            store.check_at(
                &cfg,
                &source,
                Some(&sender),
                now + Duration::hours(2),
                false
            )?[0]
                .kind,
            "Ready for review"
        );
        assert_eq!(store.rows()?[0].activity, now + Duration::hours(1));
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(2),
                    false
                )?
                .is_empty()
        );
        assert_eq!(
            store.check_at(
                &cfg,
                &source,
                Some(&sender),
                now + Duration::hours(9),
                false
            )?[0]
                .kind,
            "Quiet PR"
        );
        Ok(())
    }
    #[test]
    fn failed_snapshot_sends_nothing_and_preserves_state() -> Result<()> {
        struct Broken;
        impl Source for Broken {
            fn fetch(&self, _: &Config) -> Result<Vec<PullRequest>> {
                bail!("partial GraphQL result");
            }
        }
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        store.check_at(&cfg, &source, Some(&sender), now, false)?;
        assert!(
            store
                .check_at(
                    &cfg,
                    &Broken,
                    Some(&sender),
                    now + Duration::hours(9),
                    false
                )
                .is_err()
        );
        assert!(sender.sent.borrow().is_empty());
        assert_eq!(store.rows()?.len(), 1);
        assert!(
            store
                .meta("last_error")?
                .unwrap()
                .contains("partial GraphQL")
        );
        Ok(())
    }
    #[test]
    fn pending_new_alert_is_cancelled_when_pr_closes() -> Result<()> {
        let (_dir, cfg, source, sender, now) = setup();
        let mut store = Store::open(&cfg.database)?;
        let pr = source.0.borrow_mut().pop().unwrap();
        store.check_at(&cfg, &source, Some(&sender), now, false)?;
        source.0.borrow_mut().push(pr.clone());
        sender.fail.set(true);
        assert!(
            store
                .check_at(&cfg, &source, Some(&sender), now, false)
                .is_err()
        );
        source.0.borrow_mut().clear();
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(1),
                    false
                )?
                .is_empty()
        );
        sender.fail.set(false);
        let mut reopened = pr;
        reopened.activity = now + Duration::hours(2);
        source.0.borrow_mut().push(reopened);
        assert!(
            store
                .check_at(
                    &cfg,
                    &source,
                    Some(&sender),
                    now + Duration::hours(2),
                    false
                )?
                .is_empty()
        );
        assert!(sender.sent.borrow().is_empty());
        Ok(())
    }
    #[test]
    fn ntfy_http_payload_is_generic_and_errors_are_retryable() -> Result<()> {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            time::{Duration as StdDuration, Instant},
        };
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> Result<Vec<String>> {
            let mut requests = Vec::new();
            for status in ["200 OK", "503 Service Unavailable"] {
                let deadline = Instant::now() + StdDuration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(StdDuration::from_millis(5))
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                stream.set_read_timeout(Some(StdDuration::from_secs(5)))?;
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = stream.read(&mut buffer)?;
                    if count == 0 {
                        bail!("Incomplete HTTP request");
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .context("Missing content length")?
                            .trim()
                            .parse()?;
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes)?);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )?;
            }
            Ok(requests)
        });
        let delivery = NtfyDelivery {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(StdDuration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url: format!("http://{address}/local-test-topic").parse()?,
            token: None,
        };
        let alert = Alert {
            key: "SECRET-ORG/SECRET-REPO#123".into(),
            kind: "New PR".into(),
            url: "https://github.com/SECRET-ORG/SECRET-REPO/pull/123".into(),
        };
        delivery.send(&alert)?;
        let failure = delivery.send(&alert).unwrap_err();
        assert!(failure.to_string().contains("503"));
        let requests = server.join().expect("HTTP test server panicked")?;
        for request in requests {
            assert!(request.contains("A tracked pull request needs your attention."));
            assert!(!request.contains("SECRET"));
            assert!(!request.split("\r\n\r\n").nth(1).unwrap().contains("123"));
            assert!(!request.contains("https://github.com"));
            let headers = request.split("\r\n\r\n").next().unwrap().to_lowercase();
            assert!(!headers.contains("click:"));
            assert!(!headers.contains("actions:"));
        }
        Ok(())
    }
}
