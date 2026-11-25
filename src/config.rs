use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repositories: Vec<String>,
    pub authors: Vec<String>,
    #[serde(default = "eight")]
    pub quiet_hours: u32,
    #[serde(default = "database")]
    pub database: PathBuf,
    pub ntfy: Option<Ntfy>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ntfy {
    #[serde(default)]
    pub enabled: bool,
    pub server: String,
    pub topic_env: Option<String>,
    pub topic: Option<String>,
    pub token_env: Option<String>,
}

fn eight() -> u32 {
    8
}
fn database() -> PathBuf {
    "review-watch.sqlite3".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Self = toml::from_str(
            &std::fs::read_to_string(path)
                .with_context(|| format!("Read configuration {}", path.display()))?,
        )?;
        if config.repositories.is_empty() || config.authors.is_empty() || config.quiet_hours == 0 {
            bail!("repositories and authors must be nonempty; quiet_hours must be positive");
        }
        for repo in &config.repositories {
            let parts: Vec<_> = repo.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|part| !valid_name(part)) {
                bail!("Invalid repository {repo}; expected owner/repository");
            }
        }
        if config.authors.iter().any(|author| !valid_name(author)) {
            bail!("authors must contain GitHub logins, not display names");
        }
        config
            .repositories
            .iter_mut()
            .for_each(|repo| repo.make_ascii_lowercase());
        config.repositories.sort();
        config.repositories.dedup();
        config
            .authors
            .iter_mut()
            .for_each(|author| author.make_ascii_lowercase());
        config.authors.sort();
        config.authors.dedup();
        if config.database.is_relative() {
            config.database = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&config.database);
        }
        if let Some(ntfy) = &config.ntfy {
            let url = reqwest::Url::parse(&ntfy.server).context("Invalid ntfy server URL")?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || ntfy.topic_env.is_some() == ntfy.topic.is_some()
                || ntfy.topic_env.as_ref().is_some_and(|name| name.is_empty())
                || ntfy.topic.as_ref().is_some_and(|topic| !valid_topic(topic))
            {
                bail!(
                    "ntfy requires an HTTPS server without credentials/query/fragment and exactly one valid topic or topic_env"
                );
            }
        }
        Ok(config)
    }

    pub fn watches(&self, repo: &str, author: &str) -> bool {
        self.repositories
            .iter()
            .any(|r| r.eq_ignore_ascii_case(repo))
            && self.authors.iter().any(|a| a.eq_ignore_ascii_case(author))
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

pub fn valid_topic(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_config_and_resolves_database_relative_to_it() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("config.toml");
        let text = "repositories=['Org/Repo','org/repo']\nauthors=['Alice','alice']\n";
        std::fs::write(&file, text)?;
        let config = Config::load(&file)?;
        assert_eq!(config.repositories, vec!["org/repo"]);
        assert_eq!(config.authors, vec!["alice"]);
        assert_eq!(config.database, dir.path().join("review-watch.sqlite3"));
        assert_eq!(config.quiet_hours, 8);
        assert!(config.ntfy.is_none());
        for invalid in [
            "repositories=[]\nauthors=['alice']",
            "repositories=['org/repo']\nauthors=['alice']\nquiet_hours=0",
            "repositories=['org/repo']\nauthors=['alice']\nunknown=true",
            "repositories=['org/repo']\nauthors=['alice']\n[ntfy]\nserver='http://ntfy.sh'\ntopic_env='TOPIC'",
        ] {
            std::fs::write(&file, invalid)?;
            assert!(Config::load(&file).is_err());
        }
        std::fs::write(
            &file,
            format!("{text}[ntfy]\nserver='https://ntfy.sh'\ntopic='safe-test-topic'\n"),
        )?;
        let configured = Config::load(&file)?;
        let ntfy = configured.ntfy.as_ref().unwrap();
        assert!(!ntfy.enabled);
        assert!(crate::watcher::NtfyDelivery::new(ntfy).is_err());
        Ok(())
    }
}
