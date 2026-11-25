use crate::{config::Config, watcher::PullRequest};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    io::Read,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub trait Source {
    fn fetch(&self, config: &Config) -> Result<Vec<PullRequest>>;
}

pub struct Github<C = Gh>(pub C);

pub struct Gh {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Gh {
    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(240),
            cancelled,
        }
    }
}

impl Default for Github<Gh> {
    fn default() -> Self {
        Self(Gh::new(Arc::default()))
    }
}

pub trait Graphql {
    fn query(&self, query: &str, variables: Value) -> Result<Value>;
}

const PR_FIELDS: &str = r#"
id number title url createdAt isDraft headRefOid reviewDecision
author { login }
reviewRequests(first:100) {
  totalCount nodes { requestedReviewer { ... on User { login } ... on Team { name } } }
}
"#;

impl Graphql for Gh {
    fn query(&self, query: &str, variables: Value) -> Result<Value> {
        let mut command = Command::new("gh");
        command.args(["api", "graphql", "-f", &format!("query={query}")]);
        for (key, value) in variables.as_object().context("Expected query variables")? {
            if !value.is_null() {
                command.args([
                    "-f",
                    &format!(
                        "{key}={}",
                        value.as_str().context("Expected a string query variable")?
                    ),
                ]);
            }
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("GitHub refresh exceeded its four-minute time budget; retry later");
        }
        let output = bounded_output(
            &mut command,
            remaining.min(Duration::from_secs(30)),
            &self.cancelled,
        )
        .context("Run gh; install GitHub CLI and run gh auth login")?;
        if !output.status.success() {
            bail!(
                "GitHub CLI failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let response: Value =
            serde_json::from_slice(&output.stdout).context("Invalid GitHub JSON")?;
        Ok(response)
    }
}

fn bounded_output(
    command: &mut Command,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<Output> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("GitHub refresh cancelled");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("Missing child stdout")?;
    let stderr = child.stderr.take().context("Missing child stderr")?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let started = Instant::now();
    let (status, stopped) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, None);
        }
        let reason = if cancelled.load(Ordering::Relaxed) {
            Some("GitHub refresh cancelled")
        } else if started.elapsed() >= timeout {
            Some("GitHub request timed out")
        } else {
            None
        };
        if let Some(reason) = reason {
            child.kill().context("Stop interrupted GitHub request")?;
            break (child.wait()?, Some(reason));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("GitHub stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("GitHub stderr reader panicked"))??;
    if let Some(reason) = stopped {
        bail!("{reason}");
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

impl<C: Graphql> Github<C> {
    fn activity(
        &self,
        id: &str,
        created: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, Option<DateTime<Utc>>)> {
        let mut latest = created;
        let mut ready_at = None;
        // Discussion and lifecycle events have server event times, unlike commit author dates.
        let query = r#"query($id:ID!,$cursor:String) {
          node(id:$id) { ... on PullRequest {
            timelineItems(first:100,after:$cursor,itemTypes:[ISSUE_COMMENT,PULL_REQUEST_REVIEW,READY_FOR_REVIEW_EVENT,REOPENED_EVENT]) {
              pageInfo { hasNextPage endCursor }
              nodes {
                __typename
                ... on IssueComment { createdAt author { __typename } }
                ... on PullRequestReview { submittedAt author { __typename } }
                ... on ReadyForReviewEvent { createdAt }
                ... on ReopenedEvent { createdAt }
              }
            }
          } }
        }"#;
        self.pages(query, json!({"id":id}), "/node/timelineItems", |node| {
            let typ = text(node, "__typename")?;
            if typ == "ReadyForReviewEvent" {
                ready_at = Some(ready_at.unwrap_or(created).max(time(node, "createdAt")?));
            }
            if matches!(typ, "ReadyForReviewEvent" | "ReopenedEvent") || human(node)? {
                let field = if typ == "PullRequestReview" {
                    "submittedAt"
                } else {
                    "createdAt"
                };
                if !node
                    .get(field)
                    .context("Missing activity timestamp")?
                    .is_null()
                {
                    latest = latest.max(time(node, field)?);
                }
            }
            Ok(())
        })?;
        let query = r#"query($id:ID!,$cursor:String) {
          node(id:$id) { ... on PullRequest {
            reviewThreads(first:100,after:$cursor) {
              pageInfo { hasNextPage endCursor }
              nodes { id }
            }
          } }
        }"#;
        let mut threads = Vec::new();
        self.pages(query, json!({"id":id}), "/node/reviewThreads", |node| {
            threads.push(text(node, "id")?.to_owned());
            Ok(())
        })?;
        let comments = r#"query($id:ID!,$cursor:String) {
          node(id:$id) { ... on PullRequestReviewThread {
            comments(first:100,after:$cursor) {
              pageInfo { hasNextPage endCursor }
              nodes { createdAt publishedAt author { __typename } }
            }
          } }
        }"#;
        for thread in threads {
            self.pages(comments, json!({"id":thread}), "/node/comments", |node| {
                if human(node)?
                    && !node
                        .get("publishedAt")
                        .context("Missing comment publication time")?
                        .is_null()
                {
                    latest = latest.max(time(node, "publishedAt")?);
                }
                Ok(())
            })?;
        }
        Ok((latest, ready_at))
    }

    fn pages(
        &self,
        query: &str,
        mut vars: Value,
        path: &str,
        mut visit: impl FnMut(&Value) -> Result<()>,
    ) -> Result<()> {
        let mut previous = None;
        loop {
            let data = checked_response(self.0.query(query, vars.clone())?)?;
            let connection = data
                .pointer(path)
                .context("Missing GitHub connection; refusing partial snapshot")?;
            for node in connection["nodes"]
                .as_array()
                .context("Missing GitHub nodes")?
            {
                if node.is_null() {
                    bail!("Null GitHub node; refusing partial snapshot");
                }
                visit(node)?;
            }
            match next_cursor(connection, previous.as_deref())? {
                None => return Ok(()),
                Some(cursor) => {
                    vars["cursor"] = json!(cursor);
                    previous = Some(cursor);
                }
            }
        }
    }
}

impl<C: Graphql> Source for Github<C> {
    fn fetch(&self, config: &Config) -> Result<Vec<PullRequest>> {
        let mut result = Vec::new();
        let query = format!(
            r#"query($owner:String!,$name:String!,$cursor:String) {{
          repository(owner:$owner,name:$name) {{
            pullRequests(first:100,after:$cursor,states:OPEN,orderBy:{{field:CREATED_AT,direction:DESC}}) {{
              pageInfo {{ hasNextPage endCursor }}
              nodes {{ {PR_FIELDS} }}
            }}
          }}
        }}"#
        );
        for repo in &config.repositories {
            let (owner, name) = repo.split_once('/').context("Invalid repository")?;
            self.pages(
                &query,
                json!({"owner":owner,"name":name}),
                "/repository/pullRequests",
                |node| {
                    let author = node
                        .pointer("/author/login")
                        .and_then(Value::as_str)
                        .context("Missing PR author")?;
                    if !config.watches(repo, author) {
                        return Ok(());
                    }
                    let created = time(node, "createdAt")?;
                    let draft = node["isDraft"].as_bool().context("Missing draft status")?;
                    let requests = node
                        .pointer("/reviewRequests/nodes")
                        .and_then(Value::as_array)
                        .context("Missing review requests")?;
                    if node
                        .pointer("/reviewRequests/totalCount")
                        .and_then(Value::as_u64)
                        .context("Missing request count")?
                        > 100
                    {
                        bail!("More than 100 review requests; refusing truncated snapshot");
                    }
                    let (activity, ready_at) = if draft {
                        (created, None)
                    } else {
                        self.activity(text(node, "id")?, created)?
                    };
                    result.push(PullRequest {
                        repo: repo.clone(),
                        number: node["number"].as_u64().context("Missing PR number")?,
                        title: text(node, "title")?.into(),
                        url: text(node, "url")?.into(),
                        author: author.into(),
                        created,
                        draft,
                        head: text(node, "headRefOid")?.into(),
                        activity,
                        ready_at,
                        review: node["reviewDecision"].as_str().unwrap_or("PENDING").into(),
                        requested: requests
                            .iter()
                            .map(|r| {
                                r.pointer("/requestedReviewer/login")
                                    .or_else(|| r.pointer("/requestedReviewer/name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown")
                                    .to_owned()
                            })
                            .collect(),
                    });
                    Ok(())
                },
            )
            .with_context(|| format!("Fetch {repo}"))?;
        }
        Ok(result)
    }
}

fn checked_response(response: Value) -> Result<Value> {
    if response
        .get("errors")
        .is_some_and(|e| e.as_array().is_none_or(|a| !a.is_empty()))
    {
        bail!("GitHub GraphQL errors: {}", response["errors"]);
    }
    if !response["data"].is_object() {
        bail!("Missing GitHub data");
    }
    Ok(response["data"].clone())
}

fn human(node: &Value) -> Result<bool> {
    let author = node
        .get("author")
        .context("Missing comment/review author")?;
    if author.is_null() {
        return Ok(false);
    }
    Ok(text(author, "__typename")? == "User")
}

fn next_cursor(connection: &Value, previous: Option<&str>) -> Result<Option<String>> {
    let more = connection
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .context("Missing pagination status")?;
    if !more {
        return Ok(None);
    }
    let cursor = connection
        .pointer("/pageInfo/endCursor")
        .and_then(Value::as_str)
        .context("Missing next cursor")?;
    if cursor.is_empty() || Some(cursor) == previous {
        bail!("GitHub pagination did not advance");
    }
    Ok(Some(cursor.into()))
}

fn text<'a>(node: &'a Value, field: &str) -> Result<&'a str> {
    node[field]
        .as_str()
        .with_context(|| format!("Missing GitHub field {field}"))
}
fn time(node: &Value, field: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(text(node, field)?)?.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    #[test]
    fn subprocess_timeout_cancels_silent_child_but_not_healthy_child() -> Result<()> {
        let cancelled = AtomicBool::new(false);
        let directory = tempfile::tempdir()?;
        let executable = std::env::current_exe()?;
        for attempt in 0..2 {
            let marker = directory.path().join(format!("ready-{attempt}"));
            let mut command = Command::new(&executable);
            command
                .args([
                    "--ignored",
                    "--exact",
                    "github::tests::subprocess_fixture",
                    "--nocapture",
                ])
                .env("REVIEW_WATCH_TEST_MARKER", &marker)
                .env("REVIEW_WATCH_TEST_SILENT", "1");
            let started = Instant::now();
            let result = bounded_output(&mut command, Duration::from_secs(1), &cancelled);
            assert!(result.unwrap_err().to_string().contains("timed out"));
            assert!(
                marker.exists(),
                "Child never reached its silent state; only startup was tested"
            );
            assert!(started.elapsed() < Duration::from_secs(5));
        }
        let mut healthy = Command::new(&executable);
        healthy
            .args([
                "--ignored",
                "--exact",
                "github::tests::subprocess_fixture",
                "--nocapture",
            ])
            .env("REVIEW_WATCH_TEST_MARKER", directory.path().join("healthy"));
        let output = bounded_output(&mut healthy, Duration::from_secs(10), &cancelled)?;
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)?.contains("healthy"));
        cancelled.store(true, Ordering::Relaxed);
        let mut never_spawn = Command::new("nonexistent-must-not-run");
        assert!(
            bounded_output(&mut never_spawn, Duration::from_secs(10), &cancelled)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        let expired = Gh {
            deadline: Instant::now(),
            cancelled: Arc::default(),
        };
        assert!(
            expired
                .query("", json!({}))
                .unwrap_err()
                .to_string()
                .contains("time budget")
        );
        assert!(
            expired
                .query("", json!({}))
                .unwrap_err()
                .to_string()
                .contains("time budget")
        );
        Ok(())
    }

    #[test]
    #[ignore = "Child process fixture, invoked only by timeout test"]
    fn subprocess_fixture() {
        let marker = std::env::var_os("REVIEW_WATCH_TEST_MARKER").expect("Missing fixture marker");
        std::fs::write(marker, "ready").unwrap();
        if std::env::var_os("REVIEW_WATCH_TEST_SILENT").is_some() {
            std::thread::sleep(Duration::from_secs(10));
        }
        println!("healthy");
    }

    struct MockGraph(RefCell<VecDeque<(Option<&'static str>, Value)>>);
    impl Graphql for MockGraph {
        fn query(&self, _: &str, variables: Value) -> Result<Value> {
            let (cursor, response) = self
                .0
                .borrow_mut()
                .pop_front()
                .context("Unexpected extra query")?;
            assert_eq!(variables.get("cursor").and_then(Value::as_str), cursor);
            Ok(response)
        }
    }

    fn connection(nodes: Vec<Value>, next: Option<&str>) -> Value {
        json!({"nodes":nodes,"pageInfo":{"hasNextPage":next.is_some(),"endCursor":next}})
    }

    fn config() -> Config {
        Config {
            repositories: vec!["org/repo".into()],
            authors: vec!["alice".into()],
            quiet_hours: 8,
            database: "unused.sqlite3".into(),
            ntfy: None,
        }
    }

    #[test]
    fn paginates_all_activity_and_ignores_bot_noise() -> Result<()> {
        let prs = connection(
            vec![json!({
                "id":"pr1","number":1,"title":"Title","url":"https://github.com/org/repo/pull/1",
                "createdAt":"2026-01-01T00:00:00Z","isDraft":false,"headRefOid":"head",
                "reviewDecision":null,"author":{"login":"alice"},
                "reviewRequests":{"totalCount":1,"nodes":[{"requestedReviewer":{"login":"reviewer"}}]}
            })],
            None,
        );
        let graph = Github(MockGraph(RefCell::new(VecDeque::from([
            (
                None,
                json!({"data":{"repository":{"pullRequests":connection(vec![json!({"author":{"login":"bob"}})],Some("prs2"))}}}),
            ),
            (
                Some("prs2"),
                json!({"data":{"repository":{"pullRequests":prs}}}),
            ),
            (
                None,
                json!({"data":{"node":{"timelineItems":connection(vec![
                json!({"__typename":"IssueComment","createdAt":"2026-01-01T01:00:00Z","author":{"__typename":"User"}}),
                json!({"__typename":"IssueComment","createdAt":"2026-01-09T01:00:00Z","author":{"__typename":"Bot"}}),
                json!({"__typename":"ReadyForReviewEvent","createdAt":"2026-01-01T02:00:00Z"}),
                json!({"__typename":"ReopenedEvent","createdAt":"2026-01-01T03:00:00Z"})
            ],Some("timeline2"))}}}),
            ),
            (
                Some("timeline2"),
                json!({"data":{"node":{"timelineItems":connection(vec![
                json!({"__typename":"PullRequestReview","submittedAt":"2026-01-01T04:00:00Z","author":{"__typename":"User"}}),
                json!({"__typename":"PullRequestReview","submittedAt":"2026-01-10T00:00:00Z","author":{"__typename":"Bot"}}),
                json!({"__typename":"PullRequestReview","submittedAt":null,"author":{"__typename":"User"}})
            ],None)}}}),
            ),
            (
                None,
                json!({"data":{"node":{"reviewThreads":connection(vec![json!({"id":"thread1"})],Some("threads2"))}}}),
            ),
            (
                Some("threads2"),
                json!({"data":{"node":{"reviewThreads":connection(vec![json!({"id":"thread2"})],None)}}}),
            ),
            (
                None,
                json!({"data":{"node":{"comments":connection(vec![
                json!({"publishedAt":"2026-01-12T00:00:00Z","author":{"__typename":"Bot"}}),
                json!({"publishedAt":null,"createdAt":"2026-01-13T00:00:00Z","author":{"__typename":"User"}})
            ],Some("comments2"))}}}),
            ),
            (
                Some("comments2"),
                json!({"data":{"node":{"comments":connection(vec![
                json!({"publishedAt":"2026-01-01T05:00:00Z","author":{"__typename":"User"}})
            ],None)}}}),
            ),
            (
                None,
                json!({"data":{"node":{"comments":connection(vec![],None)}}}),
            ),
        ]))));
        let result = graph.fetch(&config())?;
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].activity,
            DateTime::parse_from_rfc3339("2026-01-01T05:00:00Z")?
        );
        assert_eq!(
            result[0].ready_at,
            Some(DateTime::parse_from_rfc3339("2026-01-01T02:00:00Z")?.with_timezone(&Utc))
        );
        assert_eq!(result[0].requested, vec!["reviewer"]);
        assert!(graph.0.0.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn later_page_failure_rejects_entire_snapshot() {
        let graph = Github(MockGraph(RefCell::new(VecDeque::from([
            (
                None,
                json!({"data":{"repository":{"pullRequests":connection(vec![],Some("second"))}}}),
            ),
            (
                Some("second"),
                json!({"data":{"repository":{"pullRequests":connection(vec![],None)}},"errors":[{"message":"rate limited"}]}),
            ),
        ]))));
        assert!(graph.fetch(&config()).is_err());
        let missing = Github(MockGraph(RefCell::new(VecDeque::from([(
            None,
            json!({"data":{"repository":null}}),
        )]))));
        assert!(missing.fetch(&config()).is_err());
    }

    #[test]
    fn rejects_partial_and_malformed_responses() {
        assert!(
            checked_response(json!({"data":{"repository":{}},"errors":[{"message":"denied"}]}))
                .is_err()
        );
        assert!(checked_response(json!({"data":null})).is_err());
        assert!(checked_response(json!({"data":{}})).is_ok());
        assert!(
            next_cursor(
                &json!({"pageInfo":{"hasNextPage":true,"endCursor":"same"}}),
                Some("same")
            )
            .is_err()
        );
        assert!(next_cursor(&json!({"pageInfo":{"hasNextPage":true}}), None).is_err());
        assert_eq!(
            next_cursor(&json!({"pageInfo":{"hasNextPage":false}}), None).unwrap(),
            None
        );
    }
}
