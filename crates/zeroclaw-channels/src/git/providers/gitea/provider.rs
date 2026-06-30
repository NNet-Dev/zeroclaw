//! `GiteaProvider` — Gitea/Forgejo implementation of [`GitProvider`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::api::GiteaApi;
use super::mapping;
use crate::git::poll::PollStream;
use crate::git::traits::{FetchPage, GitProvider, ReactionTarget, SelfIdentity};
use crate::git::types::{GitChannelError, IssueRef, RepoRef};

pub struct GiteaProvider {
    api: GiteaApi,
    access_token: String,
    identity: parking_lot::Mutex<Option<SelfIdentity>>,
}

impl GiteaProvider {
    pub fn new(
        api_base_url: Option<String>,
        access_token: String,
        proxy_url: Option<String>,
    ) -> Self {
        Self {
            api: GiteaApi::new(api_base_url, proxy_url),
            access_token,
            identity: parking_lot::Mutex::new(None),
        }
    }

    fn token(&self) -> Result<&str, GitChannelError> {
        if self.access_token.trim().is_empty() {
            return Err(GitChannelError::Config(
                "gitea/forgejo provider requires channels.git.<alias>.access_token".into(),
            ));
        }
        Ok(self.access_token.as_str())
    }

    async fn fetch_issues(
        &self,
        token: &str,
        repo: &RepoRef,
        since: DateTime<Utc>,
    ) -> Result<FetchPage, GitChannelError> {
        let mut events = Vec::new();
        let mut advance_to: Option<DateTime<Utc>> = None;
        for issue in self.api.list_issues_since(token, repo, since).await? {
            events.push(mapping::from_issue_opened(&issue, repo));
            if let Some(transition) = mapping::from_pull_transition(&issue, repo) {
                events.push(transition);
            }
            advance_to = Some(advance_to.map_or(issue.created_at, |m| m.max(issue.created_at)));
        }
        Ok(FetchPage {
            events,
            advance_to,
            etag: None,
            not_modified: false,
        })
    }

    async fn fetch_comments(
        &self,
        token: &str,
        repo: &RepoRef,
        since: DateTime<Utc>,
    ) -> Result<FetchPage, GitChannelError> {
        let mut events = Vec::new();
        let mut advance_to: Option<DateTime<Utc>> = None;
        for comment in self
            .api
            .list_issue_comments_since(token, repo, since)
            .await?
        {
            if let Some(event) = mapping::from_comment(&comment, repo) {
                advance_to =
                    Some(advance_to.map_or(comment.created_at, |m| m.max(comment.created_at)));
                events.push(event);
            }
        }
        Ok(FetchPage {
            events,
            advance_to,
            etag: None,
            not_modified: false,
        })
    }

    async fn fetch_releases(
        &self,
        token: &str,
        repo: &RepoRef,
        since: DateTime<Utc>,
    ) -> Result<FetchPage, GitChannelError> {
        let mut events = Vec::new();
        let mut advance_to: Option<DateTime<Utc>> = None;
        for release in self.api.list_releases(token, repo).await? {
            let Some(published_at) = release.published_at else {
                continue;
            };
            if published_at < since {
                continue;
            }
            if let Some(event) = mapping::from_release(&release, repo) {
                advance_to = Some(advance_to.map_or(published_at, |m| m.max(published_at)));
                events.push(event);
            }
        }
        Ok(FetchPage {
            events,
            advance_to,
            etag: None,
            not_modified: false,
        })
    }
}

#[async_trait]
impl GitProvider for GiteaProvider {
    fn name(&self) -> &'static str {
        "gitea"
    }

    async fn self_identity(&self) -> Result<SelfIdentity, GitChannelError> {
        if let Some(id) = self.identity.lock().as_ref() {
            return Ok(SelfIdentity {
                mention_handle: id.mention_handle.clone(),
                bot_login: id.bot_login.clone(),
            });
        }
        let user = self.api.current_user(self.token()?).await?;
        let login = user.login();
        let id = SelfIdentity {
            mention_handle: login.clone(),
            bot_login: login,
        };
        *self.identity.lock() = Some(SelfIdentity {
            mention_handle: id.mention_handle.clone(),
            bot_login: id.bot_login.clone(),
        });
        Ok(id)
    }

    async fn discover_repos(&self) -> Result<Vec<RepoRef>, GitChannelError> {
        self.api.list_user_repos(self.token()?).await
    }

    async fn fetch(
        &self,
        repo: &RepoRef,
        stream: PollStream,
        since: DateTime<Utc>,
        _etag: Option<&str>,
    ) -> Result<FetchPage, GitChannelError> {
        let token = self.token()?;
        match stream {
            PollStream::Issues => self.fetch_issues(token, repo, since).await,
            PollStream::Comments => self.fetch_comments(token, repo, since).await,
            PollStream::Releases => self.fetch_releases(token, repo, since).await,
            PollStream::ReviewComments | PollStream::WorkflowRuns | PollStream::Feed => {
                Ok(FetchPage {
                    events: Vec::new(),
                    advance_to: None,
                    etag: None,
                    not_modified: false,
                })
            }
        }
    }

    async fn post_comment(&self, target: &IssueRef, body: &str) -> Result<String, GitChannelError> {
        let id = self.api.create_comment(self.token()?, target, body).await?;
        Ok(id.to_string())
    }

    async fn edit_comment(
        &self,
        repo: &RepoRef,
        comment_id: &str,
        body: &str,
    ) -> Result<(), GitChannelError> {
        let id: u64 = comment_id
            .parse()
            .map_err(|_| GitChannelError::BadRecipient(comment_id.to_string()))?;
        self.api.update_comment(self.token()?, repo, id, body).await
    }

    async fn delete_comment(
        &self,
        repo: &RepoRef,
        comment_id: &str,
    ) -> Result<(), GitChannelError> {
        let id: u64 = comment_id
            .parse()
            .map_err(|_| GitChannelError::BadRecipient(comment_id.to_string()))?;
        self.api.delete_comment(self.token()?, repo, id).await
    }

    async fn add_reaction(
        &self,
        target: &ReactionTarget,
        emoji: &str,
    ) -> Result<(), GitChannelError> {
        let Some(content) = mapping::map_reaction(emoji) else {
            return Ok(());
        };
        match target {
            ReactionTarget::Comment { repo, comment_id } => {
                let Ok(id) = comment_id.parse::<u64>() else {
                    return Ok(());
                };
                self.api
                    .add_comment_reaction(self.token()?, repo, id, content)
                    .await
            }
            ReactionTarget::Issue(issue) => {
                self.api
                    .add_issue_reaction(self.token()?, issue, content)
                    .await
            }
        }
    }
}
