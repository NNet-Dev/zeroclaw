//! Contract layer for the GitHub App channel.
//!
//! Shared constants, identifier newtypes, REST payload structs, and the
//! channel error enum. Zero business logic — sibling modules (`auth`,
//! `api`, `events`, `poll`) depend on this file and never on each other;
//! `channel` composes them.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// GitHub REST API base URL.
pub const GITHUB_API_BASE: &str = "https://api.github.com";
/// `Accept` header value for all GitHub REST requests.
pub const GITHUB_ACCEPT: &str = "application/vnd.github+json";
/// Pinned `X-GitHub-Api-Version` header value.
pub const GITHUB_API_VERSION: &str = "2022-11-28";
/// GitHub rejects requests without a `User-Agent`.
pub const GITHUB_USER_AGENT: &str = "zeroclaw";

/// Refresh installation tokens this many seconds before they expire.
pub const TOKEN_REFRESH_BUFFER_SECS: i64 = 60;

/// Maximum characters per issue comment (GitHub caps bodies at 65536;
/// leave headroom for split markers).
pub const COMMENT_MAX_CHARS: usize = 65_000;

/// A GitHub App installation identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstallationId(pub u64);

impl std::fmt::Display for InstallationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A repository reference (`owner/repo`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

impl RepoRef {
    /// Parse `owner/repo`. Returns `None` when either half is empty.
    pub fn parse(s: &str) -> Option<Self> {
        let (owner, repo) = s.split_once('/')?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

impl std::fmt::Display for RepoRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// An issue or pull-request reference (`owner/repo#number`) — the
/// channel's `reply_target` / `recipient` wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    pub repo: RepoRef,
    pub number: u64,
}

impl IssueRef {
    /// Parse `owner/repo#number`.
    pub fn parse(s: &str) -> Option<Self> {
        let (repo, number) = s.split_once('#')?;
        Some(Self {
            repo: RepoRef::parse(repo)?,
            number: number.parse().ok()?,
        })
    }
}

impl std::fmt::Display for IssueRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

// ── REST payloads (deserialized verbatim from api.github.com) ──────

/// Comment or issue author.
#[derive(Debug, Clone, Deserialize)]
pub struct GhUser {
    pub login: String,
    /// `"User"`, `"Bot"`, or `"Organization"`.
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl GhUser {
    pub fn is_bot(&self) -> bool {
        self.kind.eq_ignore_ascii_case("bot")
    }
}

/// An issue or pull request (the issues namespace covers both).
#[derive(Debug, Clone, Deserialize)]
pub struct GhIssue {
    pub id: u64,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub user: GhUser,
    pub created_at: DateTime<Utc>,
}

/// A comment on an issue or pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct GhComment {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    pub user: GhUser,
    pub created_at: DateTime<Utc>,
    /// API URL of the parent issue; the trailing segment is the issue number.
    pub issue_url: String,
}

impl GhComment {
    /// Issue number extracted from `issue_url`'s trailing path segment.
    pub fn issue_number(&self) -> Option<u64> {
        self.issue_url.rsplit('/').next()?.parse().ok()
    }
}

/// A repository visible to the installation.
#[derive(Debug, Clone, Deserialize)]
pub struct GhRepo {
    pub full_name: String,
}

/// The app itself (`GET /app`).
#[derive(Debug, Clone, Deserialize)]
pub struct GhApp {
    pub slug: String,
}

/// One installation of the app (`GET /app/installations`).
#[derive(Debug, Clone, Deserialize)]
pub struct GhInstallation {
    pub id: u64,
}

/// Response of `POST /app/installations/{id}/access_tokens`.
#[derive(Debug, Clone, Deserialize)]
pub struct GhTokenResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// A cached installation access token.
#[derive(Debug, Clone)]
pub struct CachedToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl CachedToken {
    /// Whether the token is still safely usable at `now` (refresh-buffer
    /// seconds before the hard expiry).
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.expires_at - chrono::Duration::seconds(TOKEN_REFRESH_BUFFER_SECS) > now
    }
}

/// Errors raised by the GitHub App channel.
#[derive(Debug, thiserror::Error)]
pub enum GithubChannelError {
    #[error("failed to read GitHub App private key at {path}: {source}")]
    KeyRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("GitHub App JWT error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("GitHub API {endpoint} failed ({status}): {body}")]
    Api {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("GitHub API rate limited until {reset_at}")]
    RateLimited { reset_at: DateTime<Utc> },
    #[error(
        "GitHub App has no installations; install the app on a repository \
         or set `installation_id`"
    )]
    NoInstallation,
    #[error("GitHub App has {0} installations; set `installation_id` to choose one")]
    MultipleInstallations(usize),
    #[error("invalid GitHub recipient `{0}` (expected `owner/repo#number`)")]
    BadRecipient(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_parses_owner_and_repo() {
        let r = RepoRef::parse("octo/repo").unwrap();
        assert_eq!(r.owner, "octo");
        assert_eq!(r.repo, "repo");
        assert_eq!(r.to_string(), "octo/repo");
    }

    #[test]
    fn repo_ref_rejects_malformed_input() {
        assert!(RepoRef::parse("no-slash").is_none());
        assert!(RepoRef::parse("/repo").is_none());
        assert!(RepoRef::parse("owner/").is_none());
        assert!(RepoRef::parse("a/b/c").is_none());
    }

    #[test]
    fn issue_ref_round_trips() {
        let i = IssueRef::parse("octo/repo#42").unwrap();
        assert_eq!(i.number, 42);
        assert_eq!(i.to_string(), "octo/repo#42");
    }

    #[test]
    fn issue_ref_rejects_bad_number_and_missing_hash() {
        assert!(IssueRef::parse("octo/repo").is_none());
        assert!(IssueRef::parse("octo/repo#abc").is_none());
    }

    #[test]
    fn comment_issue_number_comes_from_issue_url() {
        let c = GhComment {
            id: 1,
            body: Some("hi".into()),
            user: GhUser {
                login: "u".into(),
                kind: "User".into(),
            },
            created_at: Utc::now(),
            issue_url: "https://api.github.com/repos/o/r/issues/77".into(),
        };
        assert_eq!(c.issue_number(), Some(77));
    }

    #[test]
    fn cached_token_freshness_respects_buffer() {
        let now = Utc::now();
        let fresh = CachedToken {
            token: "t".into(),
            expires_at: now + chrono::Duration::seconds(TOKEN_REFRESH_BUFFER_SECS + 5),
        };
        let stale = CachedToken {
            token: "t".into(),
            expires_at: now + chrono::Duration::seconds(TOKEN_REFRESH_BUFFER_SECS - 5),
        };
        assert!(fresh.is_fresh(now));
        assert!(!stale.is_fresh(now));
    }
}
