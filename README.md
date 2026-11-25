# Review watch

A small Rust application for keeping up with teammates' GitHub pull requests:

- `check`: fetch once, save a SQLite snapshot, and deliver new/ready/quiet alerts.
- `tui`: a ratatui dashboard with the same watcher and a five-minute refresh.
- GitHub CLI authentication; no GitHub tokens in the configuration.
- Optional ntfy notifications. Every title/body is fully generic: **no PR title,
  author, repository name, number, URLs, or comment text** leaves in the message.

## Start on Windows

Install a current stable Rust toolchain, its Windows C++ build prerequisites, and
[GitHub CLI](https://cli.github.com/). Use your existing GitHub access; this app
does not request new permissions. SSO-protected repositories must already work
with your GitHub CLI login.

```powershell
Set-Location C:\work\github-dashboard
gh auth login
if (-not (Test-Path .\config.toml)) { Copy-Item .\config.example.toml .\config.toml }
# Edit repositories and authors in config.toml before continuing.
cargo build --release --locked
.\target\release\review-watch.exe --config .\config.toml check --dry-run
.\target\release\review-watch.exe --config .\config.toml check
.\target\release\review-watch.exe --config .\config.toml tui
```

The database path is relative to the **configuration file**, not the working
directory. Its parent directory must exist. Use the same database for scheduled
checks and the dashboard. Repository and author matching is case-insensitive.
All configured authors are watched in all configured repositories.
Set `assignee = "saurabh500"` to also watch open PRs assigned to that GitHub
login, regardless of author, within the configured repositories. This is an
either/or match, not a requirement that both author and assignee match. It uses
PR assignees, not requested reviewers. Omit `assignee` for author-only tracking;
set `authors = []` for assignee-only tracking. Matching is case-insensitive.
Restart the dashboard after changing the configuration.
Set `hide_drafts = true` to omit drafts from the dashboard (default: `false`).
Drafts are still tracked internally so draft-to-ready notifications keep working;
they remain in the saved snapshot and never alert while draft.

The initial real `check` establishes the baseline. A dry run fetches real GitHub
data and prints proposed messages, but sends nothing and rolls back PR state,
baseline, delivery state and refresh metadata. It may create an empty SQLite
database/schema and a lock file. Without ntfy, real checks still save activity
and leave notifications pending; they do not claim delivery.

## Enable ntfy explicitly

Uncomment the `[ntfy]` section in your local configuration, set `enabled = true`
only when ready to publish, and set its environment variables. Delivery defaults
to disabled even when an ntfy section exists. Use a private, hard-to-guess topic and preferably an authenticated
server. HTTPS is required; redirects are refused. No server/topic is enabled by
default.

```powershell
$env:REVIEW_WATCH_NTFY_TOPIC = "REPLACE-WITH-A-LONG-RANDOM-PRIVATE-TOPIC"
$env:REVIEW_WATCH_NTFY_TOKEN = "YOUR-SERVER-TOKEN"
# Configure token_env only if your ntfy server requires a token.
.\target\release\review-watch.exe --config .\config.toml check --dry-run
# This next command really publishes once configured:
.\target\release\review-watch.exe --config .\config.toml check
```

Subscribe to the configured topic in your ntfy client. The fixed notification
body is "A tracked pull request needs your attention. Open your local review
dashboard." No click/action headers or other PR metadata are attached. A public
unauthenticated topic can still receive forged messages from anyone who learns
its name; it is not an authenticated private channel.

Protect the local database: it contains PR titles and review metadata. Token
values are read from the environment, not saved to SQLite. Instead of
`topic_env`, you may set `topic = "YOUR-RANDOM-TOPIC"` in the excluded local
configuration; exactly one is required. Do not share that file. Neither the local
configuration nor SQLite/lock files are included by Cargo packaging. Avoid
putting credentials in server URLs.

When enabling ntfy after running without it, pending new/ready alerts for PRs that
are still open and ready will be sent. Review them with a dry run first.

## Notification rules

The threshold is **at least eight elapsed hours**, configurable with
`quiet_hours`. Polling means notification can arrive one polling interval later,
plus API/delivery time.

- New non-draft PR: once, after a successful baseline for that repository/author
  pair. Existing PRs on that pair's first sync do not generate new-PR messages.
- Ready for review: once for each newly observed ready event or observed
  draft-to-ready transition. Events between polls are found in the timeline.
- Quiet PR: once per inactivity period, including across restarts. New
  meaningful activity resets that timer and permits a later quiet alert.
- Closed/merged PRs disappear from the open snapshot; drafts never alert.
  Undelivered alerts are not sent for closed/merged/draft PRs.
- A new/ready alert takes precedence over a quiet alert in the same check.
  If already quiet, the quiet alert can follow on the next check.
- Snooze delays all alerts for eight hours. Pin only changes dashboard order.

Meaningful activity is PR creation, reopening, ready-for-review, a changed head
SHA, a human issue comment, a submitted human review, or a published human inline
review comment. GitHub `User` authors count as humans; `Bot` authors do not.
CI, labels, bot comments, comment edits, and generic `updatedAt` changes do not
reset the clock. Deleted authors cannot be classified and their comments are
ignored. Automation using an ordinary human account cannot be distinguished
from that human.

**First-observation limit:** GitHub commit author/committer timestamps are not
push times. On first seeing a PR, this app conservatively starts its quiet timer
at observation time, even if it is already old. Later human/lifecycle activity
uses GitHub's event times; a newly observed head SHA uses the completed fetch
time. Consequently an existing idle PR needs eight hours of observation before
its first quiet alert. Multiple pushes between polls collapse to the observed
head; a head changed and then restored entirely between polls cannot be detected.
Activity deleted before fetching is likewise unavailable. Keep the system clock
correct. Removing the database loses baselines, pins, snoozes and deduplication.

All open PR pages, relevant timeline pages, review-thread pages, and each
thread's comment pages are fetched. Author filtering happens locally, avoiding
GitHub search's result cap. A repository with many PRs/comments can be slow and
consume significant API quota. Review-request display supports up to 100
requests; larger lists fail explicitly rather than silently truncate.
Assignee lists likewise support up to 100 assignees and fail explicitly if larger.
Assigned PRs use the same notification rules; an unlisted author's first observed
sync establishes that author's baseline in that repository.

**Failures and duplicates:** A failed or partial GitHub snapshot sends no alerts
and preserves the preceding snapshot. All repositories must fetch successfully
before state changes or delivery. Delivery failures return a nonzero exit code
and remain pending for the next successful check. Success is saved only after
ntfy returns HTTP success. An exclusive OS file lock covers fetch, reconciliation,
send and persistence, so overlapping checks using the same database fail clearly
instead of routinely duplicating alerts.

This is **not exactly-once delivery**: a successful publish followed by a crash
before SQLite records success, or a lost HTTP response after the server accepted
the message, can cause a duplicate. GitHub pages also are not an atomic snapshot:
activity or closure during a multi-request fetch may only be reflected next
poll. ntfy has a 30-second request timeout. Each GitHub subprocess request has a
30-second timeout, and all GitHub requests in one refresh share a four-minute
time budget. A timeout fails the snapshot without sending stale alerts.

## Dashboard

Run `review-watch tui` in a real terminal, preferably at least 120 columns wide.
It shows author, repository/number, title, PR age, meaningful inactivity, draft/
review decision, requested reviewers, refresh status and errors. Cached rows
remain visible with an error when refresh fails.

| Key | Action |
| --- | --- |
| Up/Down or k/j | Select a PR |
| r | Refresh now (ignored while a refresh is already running) |
| o or Enter | Open the selected PR with `gh browse` |
| p | Toggle a persistent pin |
| s | Snooze the selected PR for eight hours |
| q, Esc, Ctrl+C | Quit and restore the terminal |

Refreshes run in a single background worker. The dashboard keeps responding to
selection, redraw and quit during a fetch, shows elapsed fetch seconds, and keeps
previously displayed rows visible, including the saved snapshot at startup.
Repeated `r` presses do not start or queue
duplicate refreshes. Quitting cancels the active GitHub subprocess.
The dashboard releases the database lock between refreshes/actions. A concurrent
scheduled check can temporarily prevent refresh or a pin/snooze action; its
error is displayed. The next manual or timed refresh retries.

## Poll while the dashboard is closed

Create a Windows Task Scheduler task yourself; nothing in this project installs
a task or service.

1. Run as the same Windows account used for `gh auth login`; choose whether it
   should run when you are logged out.
2. Set a trigger repeating every **5 minutes**, indefinitely.
3. Program: `C:\work\github-dashboard\target\release\review-watch.exe`.
4. Arguments: `--config C:\work\github-dashboard\config.toml check`.
5. Start in: `C:\work\github-dashboard`.
6. Under Settings, choose **Do not start a new instance** when already running.

Make sure `gh` is in the task account's PATH, the task can access the SQLite
directory, and ntfy environment variables are available to that process.
PowerShell `$env:` assignments only affect that shell and its children; they
do not configure Task Scheduler. Provision secrets through your own protected
environment/wrapper rather than putting them in task arguments. Use Task
Scheduler's last-run result and capture stdout/stderr with a protected wrapper
if you need historical logs. The database/dashboard preserve the latest refresh
and latest fetch/delivery error, not a full log history.

## Development

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

Tests use temporary SQLite databases, queued GraphQL responses, fake delivery,
and a loopback-only fake HTTP server that checks generic message bytes and
delivery failure handling. They do not publish to ntfy or require GitHub access.
Live read-only GitHub fetching and terminal rendering have also been exercised.
Real ntfy delivery from this binary and Task Scheduler operation still need an
operator check with the intended environment.
