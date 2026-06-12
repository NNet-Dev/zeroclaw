//! `GithubChannel` — composes `auth`, `api`, `events`, and `poll` into the
//! `Channel` trait. This is the module's composition root: the only file
//! that imports its siblings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_config::schema::GithubConfig;

use super::api::GithubApi;
use super::auth::AppAuth;
use super::events::{self, EventFilter};
use super::poll::PollState;
use super::types::{COMMENT_MAX_CHARS, GithubChannelError, InstallationId, IssueRef, RepoRef};

/// Floor for `poll_interval_secs` — protects the rate budget against
/// configs like `poll_interval_secs = 1`.
const MIN_POLL_INTERVAL_SECS: u64 = 15;

/// Minimum spacing between draft edits on one comment; GitHub's secondary
/// abuse limits punish rapid content mutation.
const DRAFT_EDIT_MIN_INTERVAL: Duration = Duration::from_secs(2);

pub struct GithubChannel {
    cfg: GithubConfig,
    /// The alias key under `[channels.github.<alias>]` this handle is
    /// bound to. Used to scope peer-group lookups and session keys.
    alias: String,
    /// Resolves inbound external peers from canonical state at message-time.
    /// No cache (see AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH").
    peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    auth: AppAuth,
    api: GithubApi,
    /// App slug resolved from `GET /app`, cached for `self_handle`.
    slug: parking_lot::Mutex<Option<String>>,
    /// Installation resolved from config or discovery.
    installation: parking_lot::Mutex<Option<InstallationId>>,
    /// Last draft-edit instant per comment id (throttle).
    draft_edits: parking_lot::Mutex<HashMap<String, Instant>>,
}

impl GithubChannel {
    pub fn new(
        cfg: GithubConfig,
        alias: impl Into<String>,
        peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    ) -> Self {
        let auth = AppAuth::new(cfg.app_id, cfg.private_key_path.clone());
        let api = GithubApi::new(cfg.proxy_url.clone());
        Self {
            cfg,
            alias: alias.into(),
            peer_resolver,
            auth,
            api,
            slug: parking_lot::Mutex::new(None),
            installation: parking_lot::Mutex::new(None),
            draft_edits: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Return the alias under `[channels.github.<alias>]` that this
    /// channel handle is bound to.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Swap in an API client pointed at a mock server.
    #[cfg(test)]
    fn with_api(mut self, api: GithubApi) -> Self {
        self.api = api;
        self
    }

    fn is_user_allowed(&self, login: &str) -> bool {
        let peers = (self.peer_resolver)();
        // GitHub logins are case-insensitive (and ASCII-only).
        crate::allowlist::is_user_allowed(&peers, login, crate::allowlist::Match::CaseInsensitive)
    }

    /// Resolve the installation to act as: config wins, then the cached
    /// discovery result, then `GET /app/installations` (sole entry).
    async fn installation_id(&self) -> Result<InstallationId, GithubChannelError> {
        if let Some(id) = self.cfg.installation_id {
            return Ok(InstallationId(id));
        }
        if let Some(id) = *self.installation.lock() {
            return Ok(id);
        }
        let jwt = self.auth.mint_jwt()?;
        let installations = self.api.list_installations(&jwt).await?;
        let id = match installations.as_slice() {
            [] => return Err(GithubChannelError::NoInstallation),
            [only] => InstallationId(only.id),
            many => return Err(GithubChannelError::MultipleInstallations(many.len())),
        };
        *self.installation.lock() = Some(id);
        Ok(id)
    }

    /// A fresh-enough installation token, exchanging a new app JWT when
    /// the cached one is inside the refresh buffer.
    async fn token(&self) -> Result<String, GithubChannelError> {
        if let Some(token) = self.auth.cached_token() {
            return Ok(token);
        }
        let installation = self.installation_id().await?;
        let jwt = self.auth.mint_jwt()?;
        let token = self
            .api
            .create_installation_token(&jwt, installation)
            .await?;
        self.auth.store_token(token.clone());
        Ok(token.token)
    }

    /// The app slug (users mention `@<slug>`; the bot's login is
    /// `<slug>[bot]`), resolved once via `GET /app`.
    async fn ensure_slug(&self) -> Result<String, GithubChannelError> {
        if let Some(slug) = self.slug.lock().clone() {
            return Ok(slug);
        }
        let jwt = self.auth.mint_jwt()?;
        let slug = self.api.app_slug(&jwt).await?;
        *self.slug.lock() = Some(slug.clone());
        Ok(slug)
    }

    /// Repos to poll: explicit config, else everything visible to the
    /// installation.
    async fn resolve_repos(&self, token: &str) -> Result<Vec<RepoRef>, GithubChannelError> {
        if !self.cfg.repos.is_empty() {
            let mut repos = Vec::with_capacity(self.cfg.repos.len());
            for entry in &self.cfg.repos {
                match RepoRef::parse(entry) {
                    Some(repo) => repos.push(repo),
                    None => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"entry": entry})),
                            "ignoring malformed `repos` entry (expected owner/repo)"
                        );
                    }
                }
            }
            return Ok(repos);
        }
        self.api.list_installation_repos(token).await
    }

    fn parse_recipient(recipient: &str) -> Result<IssueRef, GithubChannelError> {
        IssueRef::parse(recipient)
            .ok_or_else(|| GithubChannelError::BadRecipient(recipient.to_string()))
    }

    /// Poll one repo and forward fresh events. Returns the number of
    /// messages forwarded; `Err` aborts the whole listen loop only for
    /// rate limiting (handled by the caller) — API errors are returned
    /// for the caller to log and continue.
    async fn poll_repo(
        &self,
        token: &str,
        repo: &RepoRef,
        filter: &EventFilter<'_>,
        state: &mut PollState,
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
    ) -> Result<bool, GithubChannelError> {
        let repo_key = repo.to_string();
        let since = state.since(&repo_key);

        let mut messages: Vec<(chrono::DateTime<chrono::Utc>, ChannelMessage)> = Vec::new();

        for issue in self.api.list_issues_since(token, repo, since).await? {
            if !state.admit(&format!("ghi_{}", issue.id), issue.created_at) {
                continue;
            }
            state.advance(&repo_key, issue.created_at);
            if let Some(msg) = events::issue_to_message(&issue, repo, filter, &self.alias) {
                messages.push((issue.created_at, msg));
            }
        }

        for comment in self
            .api
            .list_issue_comments_since(token, repo, since)
            .await?
        {
            if !state.admit(&format!("ghc_{}", comment.id), comment.created_at) {
                continue;
            }
            state.advance(&repo_key, comment.created_at);
            if let Some(msg) = events::comment_to_message(&comment, repo, filter, &self.alias) {
                messages.push((comment.created_at, msg));
            }
        }

        messages.sort_by_key(|(created_at, _)| *created_at);

        for (_, msg) in messages {
            if !self.is_user_allowed(&msg.sender) {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"sender": msg.sender})),
                    "ignoring GitHub event from unauthorized user"
                );
                continue;
            }
            if tx.send(msg).await.is_err() {
                // Orchestrator hung up; tell the caller to stop listening.
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn edit_comment(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
        throttle: bool,
    ) -> anyhow::Result<()> {
        let issue = Self::parse_recipient(recipient)?;
        let comment_id: u64 = message_id
            .parse()
            .map_err(|_| GithubChannelError::BadRecipient(message_id.to_string()))?;
        if throttle {
            let mut edits = self.draft_edits.lock();
            if let Some(last) = edits.get(message_id)
                && last.elapsed() < DRAFT_EDIT_MIN_INTERVAL
            {
                // Drop this intermediate update; the next one (or
                // finalize) carries the accumulated content anyway.
                return Ok(());
            }
            edits.insert(message_id.to_string(), Instant::now());
        }
        let token = self.token().await?;
        self.api
            .update_comment(&token, &issue.repo, comment_id, text)
            .await?;
        Ok(())
    }
}

impl ::zeroclaw_api::attribution::Attributable for GithubChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(::zeroclaw_api::attribution::ChannelKind::Github)
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl Channel for GithubChannel {
    fn name(&self) -> &str {
        "github"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let issue = Self::parse_recipient(&message.recipient)?;
        let token = self.token().await?;
        for chunk in split_comment_text(&message.content, COMMENT_MAX_CHARS) {
            self.api.create_comment(&token, &issue, &chunk).await?;
        }
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let slug = self.ensure_slug().await?;
        let bot_login = format!("{slug}[bot]");
        let token = self.token().await?;
        let repos = self.resolve_repos(&token).await?;
        let interval = Duration::from_secs(self.cfg.poll_interval_secs.max(MIN_POLL_INTERVAL_SECS));

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "app": slug,
                    "repos": repos.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "poll_interval_secs": interval.as_secs(),
                    "mention_only": self.cfg.mention_only,
                })
            ),
            "GitHub App channel polling"
        );
        if repos.is_empty() {
            anyhow::bail!(
                "GitHub channel has no repositories to poll; set `repos` or install \
                 the app on at least one repository"
            );
        }

        let filter = EventFilter {
            bot_login: &bot_login,
            mention_handle: &slug,
            mention_only: self.cfg.mention_only,
            listen_to_bots: self.cfg.listen_to_bots,
        };
        let mut state = PollState::new(chrono::Utc::now());

        loop {
            let token = match self.token().await {
                Ok(t) => t,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": e.to_string()})),
                        "GitHub token refresh failed; retrying next tick"
                    );
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };

            for repo in &repos {
                match self.poll_repo(&token, repo, &filter, &mut state, &tx).await {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(GithubChannelError::RateLimited { reset_at }) => {
                        let wait = (reset_at - chrono::Utc::now())
                            .to_std()
                            .unwrap_or(Duration::from_secs(60))
                            // Jitter so multiple repos/instances don't
                            // stampede the moment the window resets.
                            + Duration::from_millis(u64::from(rand::random::<u16>()) % 5_000);
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"wait_secs": wait.as_secs()})),
                            "GitHub rate limited; backing off"
                        );
                        tokio::time::sleep(wait).await;
                        break;
                    }
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "repo": repo.to_string(),
                                "error": e.to_string(),
                            })),
                            "GitHub poll failed for repo; continuing"
                        );
                    }
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    async fn health_check(&self) -> bool {
        self.ensure_slug().await.is_ok()
    }

    fn self_handle(&self) -> Option<String> {
        self.slug.lock().as_ref().map(|slug| format!("{slug}[bot]"))
    }

    fn self_addressed_mention(&self) -> Option<String> {
        self.slug.lock().as_ref().map(|slug| format!("@{slug}"))
    }

    fn supports_draft_updates(&self) -> bool {
        true
    }

    async fn send_draft(&self, message: &SendMessage) -> anyhow::Result<Option<String>> {
        let issue = Self::parse_recipient(&message.recipient)?;
        let token = self.token().await?;
        let id = self
            .api
            .create_comment(&token, &issue, &message.content)
            .await?;
        self.draft_edits
            .lock()
            .insert(id.to_string(), Instant::now());
        Ok(Some(id.to_string()))
    }

    async fn update_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        self.edit_comment(recipient, message_id, text, true).await
    }

    async fn update_draft_progress(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        self.edit_comment(recipient, message_id, text, true).await
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let result = self.edit_comment(recipient, message_id, text, false).await;
        self.draft_edits.lock().remove(message_id);
        result
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
        let issue = Self::parse_recipient(recipient)?;
        let comment_id: u64 = message_id
            .parse()
            .map_err(|_| GithubChannelError::BadRecipient(message_id.to_string()))?;
        let token = self.token().await?;
        self.api
            .delete_comment(&token, &issue.repo, comment_id)
            .await?;
        self.draft_edits.lock().remove(message_id);
        Ok(())
    }

    /// Reactions are best-effort: unmappable emoji and unparsable targets
    /// are dropped silently, matching the trait's no-op default.
    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> anyhow::Result<()> {
        let Some(content) = events::map_reaction(emoji) else {
            return Ok(());
        };
        let Some(issue) = IssueRef::parse(channel_id) else {
            return Ok(());
        };
        let token = self.token().await?;
        if let Some(comment_id) = message_id
            .strip_prefix("ghc_")
            .and_then(|id| id.parse::<u64>().ok())
        {
            self.api
                .add_comment_reaction(&token, &issue.repo, comment_id, content)
                .await?;
        } else {
            self.api.add_issue_reaction(&token, &issue, content).await?;
        }
        Ok(())
    }
}

/// Split text into comment-sized chunks at paragraph (preferred) or word
/// boundaries.
fn split_comment_text(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        let limit = crate::util::floor_char_boundary(remaining, max_len);
        let split_at = remaining[..limit]
            .rfind("\n\n")
            .or_else(|| remaining[..limit].rfind('\n'))
            .or_else(|| remaining[..limit].rfind(' '))
            .unwrap_or(limit);
        let split_at = split_at.max(1);
        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start();
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(peers: Vec<String>) -> GithubChannel {
        GithubChannel::new(
            GithubConfig::default(),
            "github_test_alias",
            Arc::new(move || peers.clone()),
        )
    }

    #[test]
    fn name_and_alias() {
        let ch = channel(vec![]);
        assert_eq!(ch.name(), "github");
        assert_eq!(ch.alias(), "github_test_alias");
    }

    #[test]
    fn self_handle_unknown_until_slug_resolved() {
        let ch = channel(vec![]);
        assert!(ch.self_handle().is_none());
        *ch.slug.lock() = Some("myapp".into());
        assert_eq!(ch.self_handle().as_deref(), Some("myapp[bot]"));
        assert_eq!(ch.self_addressed_mention().as_deref(), Some("@myapp"));
    }

    #[test]
    fn user_allowlist_is_case_insensitive_like_github_logins() {
        let ch = channel(vec!["*".into()]);
        assert!(ch.is_user_allowed("anyone"));
        let ch = channel(vec!["Marc".into()]);
        assert!(ch.is_user_allowed("marc"));
        assert!(ch.is_user_allowed("MARC"));
        assert!(!ch.is_user_allowed("mallory"));
        let ch = channel(vec![]);
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn recipient_parser_rejects_garbage() {
        assert!(GithubChannel::parse_recipient("octo/repo#3").is_ok());
        assert!(GithubChannel::parse_recipient("octo/repo").is_err());
        assert!(GithubChannel::parse_recipient("nonsense").is_err());
    }

    #[test]
    fn split_comment_text_prefers_paragraph_boundaries() {
        let text = format!("{}\n\n{}", "a".repeat(60), "b".repeat(60));
        let chunks = split_comment_text(&text, 100);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "a".repeat(60));
        assert_eq!(chunks[1], "b".repeat(60));
    }

    #[test]
    fn split_comment_text_short_passthrough_and_multibyte_safety() {
        assert_eq!(split_comment_text("hi", 100), vec!["hi"]);
        let text = format!("{}{}tail", "a".repeat(99), "😀");
        let chunks = split_comment_text(&text, 100);
        assert_eq!(chunks.concat().replace(' ', ""), text.replace(' ', ""));
        for chunk in &chunks {
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    // ── Mock-server integration tests (wiremock) ───────────────────

    fn write_test_key() -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(crate::github::auth::TEST_KEY_PEM.as_bytes())
            .unwrap();
        f
    }

    fn mock_channel(server_uri: String, key_file: &tempfile::NamedTempFile) -> GithubChannel {
        let cfg = GithubConfig {
            enabled: true,
            app_id: 1,
            private_key_path: key_file.path().to_string_lossy().into_owned(),
            installation_id: Some(77),
            repos: vec!["octo/repo".into()],
            ..GithubConfig::default()
        };
        GithubChannel::new(cfg, "main", Arc::new(|| vec!["*".into()]))
            .with_api(GithubApi::with_base(server_uri))
    }

    fn test_filter() -> EventFilter<'static> {
        EventFilter {
            bot_login: "myapp[bot]",
            mention_handle: "myapp",
            mention_only: true,
            listen_to_bots: false,
        }
    }

    async fn mount_token_mock(server: &wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        wiremock::Mock::given(method("POST"))
            .and(path("/app/installations/77/access_tokens"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "token": "ghs_test",
                    "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn full_tick_polls_maps_and_forwards() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let now = chrono::Utc::now();
        mount_token_mock(&server).await;
        Mock::given(method("GET"))
            .and(path("/repos/octo/repo/issues"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": 555,
                    "number": 12,
                    "title": "Flaky test",
                    "body": "@myapp please investigate",
                    "user": {"login": "marc", "type": "User"},
                    "created_at": (now - chrono::Duration::seconds(60)).to_rfc3339(),
                }])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo/repo/issues/comments"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": 9001,
                    "body": "@myapp ping",
                    "user": {"login": "marc", "type": "User"},
                    "created_at": (now - chrono::Duration::seconds(30)).to_rfc3339(),
                    "issue_url": "https://api.github.com/repos/octo/repo/issues/12",
                }])),
            )
            .mount(&server)
            .await;

        let key = write_test_key();
        let ch = mock_channel(server.uri(), &key);
        let token = ch.token().await.unwrap();
        assert_eq!(token, "ghs_test");

        let filter = test_filter();
        let mut state = PollState::new(now - chrono::Duration::hours(1));
        let repo = RepoRef::parse("octo/repo").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let keep = ch
            .poll_repo(&token, &repo, &filter, &mut state, &tx)
            .await
            .unwrap();
        assert!(keep);
        drop(tx);

        let first = rx.recv().await.unwrap();
        assert_eq!(first.id, "ghi_555");
        assert_eq!(first.subject.as_deref(), Some("Flaky test"));
        assert_eq!(first.content, "please investigate");
        let second = rx.recv().await.unwrap();
        assert_eq!(second.id, "ghc_9001");
        assert_eq!(second.content, "ping");
        assert_eq!(second.reply_target, "octo/repo#12");
        assert_eq!(second.thread_ts.as_deref(), Some("octo/repo#12"));
        assert!(rx.recv().await.is_none());

        // Second tick: same fixtures come back from the mock, but the
        // dedup set drops them — nothing is re-forwarded.
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(8);
        ch.poll_repo(&token, &repo, &filter, &mut state, &tx2)
            .await
            .unwrap();
        drop(tx2);
        assert!(rx2.recv().await.is_none());
    }

    #[tokio::test]
    async fn draft_flow_creates_throttles_and_finalizes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_token_mock(&server).await;
        Mock::given(method("POST"))
            .and(path("/repos/octo/repo/issues/5/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 42})))
            .expect(1)
            .mount(&server)
            .await;
        // Exactly one PATCH: the intermediate update inside the 2 s
        // throttle window is dropped; only finalize lands.
        Mock::given(method("PATCH"))
            .and(path("/repos/octo/repo/issues/comments/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 42})))
            .expect(1)
            .mount(&server)
            .await;

        let key = write_test_key();
        let ch = mock_channel(server.uri(), &key);
        let draft_id = ch
            .send_draft(&SendMessage::new("thinking…", "octo/repo#5"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft_id, "42");
        ch.update_draft("octo/repo#5", "42", "partial")
            .await
            .unwrap();
        ch.finalize_draft("octo/repo#5", "42", "done")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rate_limit_surfaces_reset_time() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_token_mock(&server).await;
        let reset = chrono::Utc::now() + chrono::Duration::seconds(120);
        Mock::given(method("GET"))
            .and(path("/repos/octo/repo/issues"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset.timestamp().to_string().as_str()),
            )
            .mount(&server)
            .await;

        let key = write_test_key();
        let ch = mock_channel(server.uri(), &key);
        let token = ch.token().await.unwrap();
        let filter = test_filter();
        let mut state = PollState::new(chrono::Utc::now());
        let repo = RepoRef::parse("octo/repo").unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);

        let err = ch
            .poll_repo(&token, &repo, &filter, &mut state, &tx)
            .await
            .unwrap_err();
        match err {
            GithubChannelError::RateLimited { reset_at } => {
                assert_eq!(reset_at.timestamp(), reset.timestamp());
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }
}
