//! Pure mapping from GitHub REST payloads to `ChannelMessage`s, plus the
//! channel's inbound filtering rules (self/bot/mention). No IO — every
//! function here is fixture-testable.

use zeroclaw_api::channel::ChannelMessage;

use super::types::{GhComment, GhIssue, IssueRef, RepoRef};

/// Inbound filtering parameters, derived from channel config and the
/// app identity resolved at listen start.
pub struct EventFilter<'a> {
    /// The app's bot login (`<slug>[bot]`) — its own comments are always
    /// dropped.
    pub bot_login: &'a str,
    /// The handle users type to address the app (`@<slug>`, without `@`).
    pub mention_handle: &'a str,
    pub mention_only: bool,
    pub listen_to_bots: bool,
}

impl EventFilter<'_> {
    /// Shared author/body gate; returns the message content (with the
    /// mention stripped) when the event passes.
    fn admit(&self, author_login: &str, author_is_bot: bool, body: &str) -> Option<String> {
        if author_login.eq_ignore_ascii_case(self.bot_login) {
            return None;
        }
        if author_is_bot && !self.listen_to_bots {
            return None;
        }
        if self.mention_only && !contains_mention(body, self.mention_handle) {
            return None;
        }
        let content = strip_mention(body, self.mention_handle);
        if content.is_empty() {
            return None;
        }
        Some(content)
    }
}

/// Map an issue/PR comment to a `ChannelMessage`. Returns `None` when the
/// comment is filtered out (self/bot/mention/empty) or malformed.
pub fn comment_to_message(
    comment: &GhComment,
    repo: &RepoRef,
    filter: &EventFilter<'_>,
    alias: &str,
) -> Option<ChannelMessage> {
    let body = comment.body.as_deref().unwrap_or("");
    let content = filter.admit(&comment.user.login, comment.user.is_bot(), body)?;
    let issue = IssueRef {
        repo: repo.clone(),
        number: comment.issue_number()?,
    };
    Some(message(
        format!("ghc_{}", comment.id),
        comment.user.login.clone(),
        &issue,
        content,
        comment.created_at.timestamp(),
        None,
        alias,
    ))
}

/// Map a newly opened issue/PR (its opening post) to a `ChannelMessage`.
pub fn issue_to_message(
    issue: &GhIssue,
    repo: &RepoRef,
    filter: &EventFilter<'_>,
    alias: &str,
) -> Option<ChannelMessage> {
    let body = issue.body.as_deref().unwrap_or("");
    let content = filter.admit(&issue.user.login, issue.user.is_bot(), body)?;
    let issue_ref = IssueRef {
        repo: repo.clone(),
        number: issue.number,
    };
    Some(message(
        format!("ghi_{}", issue.id),
        issue.user.login.clone(),
        &issue_ref,
        content,
        issue.created_at.timestamp(),
        Some(issue.title.clone()),
        alias,
    ))
}

fn message(
    id: String,
    sender: String,
    issue: &IssueRef,
    content: String,
    timestamp: i64,
    subject: Option<String>,
    alias: &str,
) -> ChannelMessage {
    let target = issue.to_string();
    ChannelMessage {
        id,
        sender,
        reply_target: target.clone(),
        content,
        channel: "github".to_string(),
        channel_alias: Some(alias.to_string()),
        timestamp: timestamp.max(0) as u64,
        // Conversation context is issue-scoped: every message on the same
        // issue/PR shares a thread.
        thread_ts: Some(target),
        subject,
        ..ChannelMessage::default()
    }
}

/// Case-insensitive `@handle` match on a word boundary, so `@myapp` does
/// not match `@myapp-helper`.
///
/// ASCII-only folding throughout this module: GitHub app slugs and logins
/// are ASCII, and `to_ascii_lowercase` preserves byte offsets, which
/// `strip_mention` relies on to index the original body safely.
pub fn contains_mention(body: &str, handle: &str) -> bool {
    if handle.is_empty() {
        return false;
    }
    let body_lower = body.to_ascii_lowercase();
    let needle = format!("@{}", handle.to_ascii_lowercase());
    let mut start = 0;
    while let Some(pos) = body_lower[start..].find(&needle) {
        let end = start + pos + needle.len();
        let boundary = body_lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'));
        if boundary {
            return true;
        }
        start = end;
    }
    false
}

/// Remove every word-boundary `@handle` mention and tidy whitespace.
pub fn strip_mention(body: &str, handle: &str) -> String {
    if handle.is_empty() {
        return body.trim().to_string();
    }
    let needle = format!("@{}", handle.to_ascii_lowercase());
    let mut out = String::with_capacity(body.len());
    let body_lower = body.to_ascii_lowercase();
    let mut idx = 0;
    while let Some(pos) = body_lower[idx..].find(&needle) {
        let abs = idx + pos;
        let end = abs + needle.len();
        let boundary = body_lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'));
        out.push_str(&body[idx..abs]);
        if !boundary {
            out.push_str(&body[abs..end]);
        }
        idx = end;
    }
    out.push_str(&body[idx..]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Map a channel emoji onto GitHub's fixed reaction set. Unmappable
/// emoji are dropped by the caller (reaction support is best-effort).
pub fn map_reaction(emoji: &str) -> Option<&'static str> {
    match emoji {
        "👍" | "+1" | "✅" => Some("+1"),
        "👎" | "-1" => Some("-1"),
        "😀" | "😄" | "😆" => Some("laugh"),
        "😕" | "⚠️" => Some("confused"),
        "❤️" | "💜" => Some("heart"),
        "🎉" => Some("hooray"),
        "🚀" => Some("rocket"),
        "👀" => Some("eyes"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::GhUser;
    use chrono::Utc;

    fn filter(mention_only: bool, listen_to_bots: bool) -> EventFilter<'static> {
        EventFilter {
            bot_login: "myapp[bot]",
            mention_handle: "myapp",
            mention_only,
            listen_to_bots,
        }
    }

    fn user(login: &str, kind: &str) -> GhUser {
        GhUser {
            login: login.into(),
            kind: kind.into(),
        }
    }

    fn comment(login: &str, kind: &str, body: &str) -> GhComment {
        GhComment {
            id: 9001,
            body: Some(body.into()),
            user: user(login, kind),
            created_at: Utc::now(),
            issue_url: "https://api.github.com/repos/octo/repo/issues/7".into(),
        }
    }

    fn repo() -> RepoRef {
        RepoRef::parse("octo/repo").unwrap()
    }

    #[test]
    fn mentioned_comment_maps_with_issue_threading() {
        let c = comment("marc", "User", "@myapp run the tests");
        let msg = comment_to_message(&c, &repo(), &filter(true, false), "main").unwrap();
        assert_eq!(msg.id, "ghc_9001");
        assert_eq!(msg.sender, "marc");
        assert_eq!(msg.reply_target, "octo/repo#7");
        assert_eq!(msg.thread_ts.as_deref(), Some("octo/repo#7"));
        assert_eq!(msg.content, "run the tests");
        assert_eq!(msg.channel, "github");
        assert_eq!(msg.channel_alias.as_deref(), Some("main"));
    }

    #[test]
    fn unmentioned_comment_dropped_when_mention_only() {
        let c = comment("marc", "User", "just chatting");
        assert!(comment_to_message(&c, &repo(), &filter(true, false), "main").is_none());
        // ...but accepted when mention_only is off.
        assert!(comment_to_message(&c, &repo(), &filter(false, false), "main").is_some());
    }

    #[test]
    fn own_bot_comment_always_dropped() {
        let c = comment("myapp[bot]", "Bot", "@myapp echo");
        assert!(comment_to_message(&c, &repo(), &filter(false, true), "main").is_none());
    }

    #[test]
    fn foreign_bot_respects_listen_to_bots() {
        let c = comment("dependabot[bot]", "Bot", "@myapp review this");
        assert!(comment_to_message(&c, &repo(), &filter(true, false), "main").is_none());
        assert!(comment_to_message(&c, &repo(), &filter(true, true), "main").is_some());
    }

    #[test]
    fn issue_opening_post_maps_with_title_subject() {
        let i = GhIssue {
            id: 555,
            number: 12,
            title: "Flaky test".into(),
            body: Some("@myapp please investigate".into()),
            user: user("marc", "User"),
            created_at: Utc::now(),
        };
        let msg = issue_to_message(&i, &repo(), &filter(true, false), "main").unwrap();
        assert_eq!(msg.id, "ghi_555");
        assert_eq!(msg.reply_target, "octo/repo#12");
        assert_eq!(msg.subject.as_deref(), Some("Flaky test"));
        assert_eq!(msg.content, "please investigate");
    }

    #[test]
    fn empty_or_missing_body_dropped() {
        let mut c = comment("marc", "User", "@myapp");
        assert!(comment_to_message(&c, &repo(), &filter(true, false), "main").is_none());
        c.body = None;
        assert!(comment_to_message(&c, &repo(), &filter(false, false), "main").is_none());
    }

    #[test]
    fn mention_requires_word_boundary() {
        assert!(contains_mention("hey @myapp do it", "myapp"));
        assert!(contains_mention("@MyApp case insensitive", "myapp"));
        assert!(!contains_mention("ping @myapp-helper instead", "myapp"));
        assert!(!contains_mention("email me@myapp nothing", "myap"));
    }

    #[test]
    fn strip_mention_keeps_non_boundary_matches() {
        assert_eq!(strip_mention("@myapp do it", "myapp"), "do it");
        assert_eq!(
            strip_mention("cc @myapp-helper stays", "myapp"),
            "cc @myapp-helper stays"
        );
    }

    #[test]
    fn mention_handling_is_safe_on_non_ascii_bodies() {
        // 'İ' (U+0130) lowercases to a longer byte sequence under full
        // Unicode folding; ASCII folding keeps offsets aligned so the
        // mention is found and stripped without slicing mid-character.
        let body = "İstanbul rollout: @myapp ölçüm çalıştır 😀";
        assert!(contains_mention(body, "myapp"));
        assert_eq!(
            strip_mention(body, "myapp"),
            "İstanbul rollout: ölçüm çalıştır 😀"
        );
    }

    #[test]
    fn reaction_map_covers_ack_flow() {
        assert_eq!(map_reaction("👀"), Some("eyes"));
        assert_eq!(map_reaction("✅"), Some("+1"));
        assert_eq!(map_reaction("⚠️"), Some("confused"));
        assert_eq!(map_reaction("🦖"), None);
    }
}
