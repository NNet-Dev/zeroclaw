//! Real channel delivery for the approval route adapter (EPIC G follow-up).
//!
//! [`super::broker::NoopRouteAdapter`] only logs; this adapter actually delivers an
//! approval notice to a configured channel (Discord, Slack, ...), so a SOP that
//! parks at a policied gate can reach an approver OUT OF BAND - e.g. a channel-git
//! trigger starts a triage SOP, the SOP parks for approval, and the request lands in
//! a Discord ops channel where a maintainer approves it through the normal HTTP/WS/
//! tool surfaces (which already route back through the broker + chokepoint).
//!
//! Layering: this lives in `zeroclaw-runtime` because it needs only
//! [`zeroclaw_api::channel::Channel`] (a runtime dependency already), never the
//! `zeroclaw-channels` implementations. The DAEMON builds the concrete channel map
//! (via `zeroclaw_channels::orchestrator::build_channel_map`) and injects it here, so
//! there is no runtime -> channels layering inversion.
//!
//! Delivery is best-effort and fire-and-forget: [`ApprovalRouteAdapter::deliver`] is
//! a SYNC call made under the engine `Mutex` (on park, and on the maintenance-tick
//! escalation path), so it cannot `.await`. It spawns the async `Channel::send` onto
//! a tokio [`Handle`] captured at construction and returns immediately. A send
//! failure is logged inside the spawned task; it never blocks or clears the gate -
//! the gate state is the source of truth, the notice is only a courtesy.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::runtime::Handle;
use zeroclaw_api::channel::{Channel, SendMessage};

use super::broker::{ApprovalRouteAdapter, GateNotice};

/// A route adapter that delivers approval notices to configured channels.
///
/// `channels` is keyed by the channel-map key (`<channel>.<alias>` or bare
/// `<channel>`), the same keys `build_channel_map` produces. A route string is
/// `channel_key:recipient` (e.g. `discord.ops:123456789012345678`); the
/// `channel_key` selects the channel, the `recipient` is that channel's addressee.
pub struct ChannelRouteAdapter {
    channels: HashMap<String, Arc<dyn Channel>>,
    handle: Handle,
}

impl ChannelRouteAdapter {
    /// Build from a channel map and the tokio runtime handle to spawn sends onto.
    /// The daemon passes `tokio::runtime::Handle::current()` from its async context;
    /// capturing it here (rather than calling `Handle::current()` inside `deliver`)
    /// keeps `deliver` callable from the sync engine without panicking off-runtime.
    pub fn new(channels: HashMap<String, Arc<dyn Channel>>, handle: Handle) -> Self {
        Self { channels, handle }
    }
}

/// Parse a `channel_key:recipient` route into its two non-empty halves. Splits on
/// the FIRST `:` only, so a recipient that itself contains `:` (e.g. a Matrix
/// `@user:server` id) survives intact on the right. Channel-map keys are
/// dot-separated (`discord.ops`), never colon-separated, so the first colon is
/// unambiguously the channel/recipient boundary.
fn parse_route(route: &str) -> Option<(&str, &str)> {
    let (channel_key, recipient) = route.split_once(':')?;
    if channel_key.is_empty() || recipient.is_empty() {
        return None;
    }
    Some((channel_key, recipient))
}

/// Resolve `{{path.to.field}}` placeholders against the notice context: pure
/// dotted lookups into the JSON, nothing else (no logic, no filters). A string
/// value substitutes raw; other values substitute as compact JSON; a missing
/// path substitutes empty. Untrusted values are DATA here — they land in a
/// notification body, never in an instruction stream.
fn render_template(template: &str, context: &serde_json::Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let path = after[..end].trim();
        let mut value = context;
        let mut found = true;
        for key in path.split('.') {
            match value.get(key) {
                Some(v) => value = v,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            match value {
                serde_json::Value::String(v) => out.push_str(v),
                other => out.push_str(&other.to_string()),
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Automatic context summary when the step has no authored `- prompt:`: the
/// commonly-present fields of a gate context, compactly.
fn summarize_context(context: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    if let (Some(repo), Some(number)) = (
        context.get("repo").and_then(|v| v.as_str()),
        context.get("number").and_then(|v| v.as_u64()),
    ) {
        lines.push(format!("{repo}#{number}"));
    }
    if let Some(title) = context.get("title").and_then(|v| v.as_str()) {
        lines.push(format!("\u{201c}{title}\u{201d}"));
    }
    if let Some(author) = context
        .get("author")
        .and_then(|a| a.get("login"))
        .and_then(|v| v.as_str())
    {
        lines.push(format!("by {author}"));
    }
    if let Some(body) = context.get("body").and_then(|v| v.as_str()) {
        let trimmed: String = body.chars().take(400).collect();
        let suffix = if body.chars().count() > 400 {
            "\u{2026}"
        } else {
            ""
        };
        lines.push(format!("\n{trimmed}{suffix}"));
    }
    lines.join(" ")
}

/// How to answer, appended to every notice.
fn reply_instructions(run_id: &str) -> String {
    format!(
        "Reply `approve {run_id}` or `deny {run_id}` here, or use \
         `zeroclaw sop approve|deny {run_id}`."
    )
}

/// Render the approval-request notice body: WHAT is being approved (the step's
/// authored `- prompt:` rendered over the gate context, or an automatic context
/// summary) plus how to answer. The `approve <run_id>` text reply resolves the
/// gate via the orchestrator's gate intercept; CLI / gateway keep working.
fn render_notice(notice: &GateNotice<'_>) -> String {
    let what = match notice.gate_prompt {
        // The `- prompt:` bullet is a single line; a literal `\n` in it is the
        // author's line break.
        Some(template) => render_template(template, notice.context).replace("\\n", "\n"),
        None => summarize_context(notice.context),
    };
    let (run_id, sop_name, step) = (notice.run_id, notice.sop_name, notice.step);
    let header = format!("SOP approval needed: '{sop_name}' run `{run_id}` (step {step}).");
    let instructions = reply_instructions(run_id);
    if what.trim().is_empty() {
        format!("{header}\n\n{instructions}")
    } else {
        format!("{header}\n\n{what}\n\n{instructions}")
    }
}

/// Build the native gate prompt for channels that render one (buttons /
/// keyboards). The description carries the text-reply form too, so a screenshot
/// or forward of the prompt is still actionable.
fn build_gate_prompt(notice: &GateNotice<'_>) -> zeroclaw_api::channel::ChannelGatePrompt {
    use zeroclaw_api::channel::{ChannelGatePrompt, GateChoice, GateChoiceEmphasis};
    // Discord embeds cap descriptions at 4096 chars; stay comfortably under.
    let mut description = render_notice(notice);
    if description.chars().count() > 3500 {
        description = description.chars().take(3500).collect::<String>() + "\u{2026}";
    }
    ChannelGatePrompt {
        title: format!("SOP approval needed: {}", notice.sop_name),
        description,
        reference: notice.run_id.to_string(),
        choices: vec![
            GateChoice {
                id: "approve".to_string(),
                label: "Approve".to_string(),
                emphasis: GateChoiceEmphasis::Positive,
            },
            GateChoice {
                id: "deny".to_string(),
                label: "Deny".to_string(),
                emphasis: GateChoiceEmphasis::Negative,
            },
        ],
    }
}

/// Build the (channel_key, message) delivery pair from a route + run identity, or an
/// error describing why it can't be built. PURE (no I/O, no spawn) so the parse +
/// message-shaping is unit-testable without a runtime.
fn build_delivery(route: &str, notice: &GateNotice<'_>) -> anyhow::Result<(String, SendMessage)> {
    let Some((channel_key, recipient)) = parse_route(route) else {
        anyhow::bail!(
            "approval route '{route}' is not 'channel:recipient' (e.g. \
             'discord.ops:123456789') - both halves must be non-empty"
        );
    };
    let msg = SendMessage::new(render_notice(notice), recipient).suppress_voice();
    Ok((channel_key.to_string(), msg))
}

impl ApprovalRouteAdapter for ChannelRouteAdapter {
    fn deliver(&self, route: &str, notice: &GateNotice<'_>) -> anyhow::Result<()> {
        let (channel_key, msg) = build_delivery(route, notice)?;
        let Some(channel) = self.channels.get(&channel_key).cloned() else {
            // A misconfigured route (names a channel that isn't configured) is a real
            // operator error worth surfacing: return Err so the broker logs it. It
            // still never affects the gate (the broker's deliver_* wrappers only log).
            anyhow::bail!(
                "approval route channel '{channel_key}' is not a configured channel \
                 (route '{route}')"
            );
        };
        // Fire-and-forget: hand the async send to the runtime and return. The gate is
        // never blocked on channel I/O; a send failure is logged in the task.
        // Native gate prompt first (buttons / keyboards, answered through the
        // channel's inbound path); channels without one fall back to the text
        // notice, whose `approve <run_id>` reply the orchestrator also resolves.
        let prompt = build_gate_prompt(notice);
        let recipient = msg.recipient.clone();
        let run_id = notice.run_id.to_string();
        let route = route.to_string();
        self.handle.spawn(async move {
            let prompted = match channel.send_gate_prompt(&recipient, &prompt).await {
                Ok(prompted) => prompted,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "route": route, "run_id": run_id, "error": e.to_string()
                            })),
                        "approval route gate prompt failed; falling back to text notice"
                    );
                    false
                }
            };
            if !prompted && let Err(e) = channel.send(&msg).await {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "route": route, "run_id": run_id, "error": e.to_string()
                        })),
                    "approval route channel send failed (gate unaffected)"
                );
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_resolves_dotted_paths_and_drops_missing_ones() {
        let ctx = serde_json::json!({
            "repo": "o/r", "number": 7,
            "author": {"login": "nillth"},
            "title": "It broke"
        });
        assert_eq!(
            render_template(
                "Issue {{repo}}#{{number}} by {{author.login}}: {{title}} {{absent.field}}",
                &ctx
            ),
            "Issue o/r#7 by nillth: It broke "
        );
        // Unclosed braces pass through untouched rather than panicking.
        assert_eq!(render_template("broken {{oops", &ctx), "broken {{oops");
    }

    #[test]
    fn notice_prefers_the_authored_prompt_and_always_appends_instructions() {
        let ctx = serde_json::json!({"repo": "o/r", "number": 9, "body": "hello"});
        let authored = GateNotice {
            run_id: "run-9",
            sop_name: "triage",
            step: 1,
            context: &ctx,
            gate_prompt: Some("Review {{repo}}#{{number}} please"),
        };
        let text = render_notice(&authored);
        assert!(text.contains("Review o/r#9 please"));
        assert!(text.contains("approve run-9"));

        let auto = GateNotice {
            gate_prompt: None,
            ..authored
        };
        let text = render_notice(&auto);
        assert!(
            text.contains("o/r#9"),
            "auto summary carries repo#number: {text}"
        );
        assert!(
            text.contains("hello"),
            "auto summary carries the body: {text}"
        );
    }
    use async_trait::async_trait;
    use std::sync::Mutex;
    use zeroclaw_api::attribution::{Attributable, ChannelKind, Role};
    use zeroclaw_api::channel::ChannelMessage;

    // ── pure build_delivery / parse_route ────────────────────────

    #[test]
    fn parse_route_splits_on_first_colon_and_keeps_colons_in_recipient() {
        assert_eq!(
            parse_route("discord.ops:12345"),
            Some(("discord.ops", "12345"))
        );
        // A Matrix-style recipient with its own ':' survives on the right.
        assert_eq!(
            parse_route("matrix.main:@alice:server.example"),
            Some(("matrix.main", "@alice:server.example"))
        );
    }

    #[test]
    fn parse_route_rejects_missing_or_empty_halves() {
        assert_eq!(parse_route("discord.ops"), None, "no recipient");
        assert_eq!(parse_route("discord.ops:"), None, "empty recipient");
        assert_eq!(parse_route(":12345"), None, "empty channel key");
    }

    #[test]
    fn build_delivery_shapes_the_message_and_targets_the_recipient() {
        let (key, msg) = build_delivery(
            "discord.ops:98765",
            &GateNotice {
                run_id: "run-7",
                sop_name: "triage",
                step: 3,
                context: &serde_json::Value::Null,
                gate_prompt: None,
            },
        )
        .unwrap();
        assert_eq!(key, "discord.ops");
        assert_eq!(msg.recipient, "98765");
        assert!(msg.content.contains("run-7"), "identifies the run");
        assert!(msg.content.contains("triage"), "names the SOP");
        assert!(msg.content.contains("step 3"), "names the step");
        assert!(msg.suppress_voice, "an approval notice must not be voiced");
    }

    #[test]
    fn build_delivery_errors_on_a_route_without_a_recipient() {
        assert!(
            build_delivery(
                "discord.ops",
                &GateNotice {
                    run_id: "r",
                    sop_name: "s",
                    step: 1,
                    context: &serde_json::Value::Null,
                    gate_prompt: None,
                }
            )
            .is_err()
        );
    }

    // ── a stub Channel that records what it was asked to send ─────

    #[derive(Default)]
    struct RecordingChannel {
        sent: Arc<Mutex<Vec<SendMessage>>>,
    }

    impl Attributable for RecordingChannel {
        fn role(&self) -> Role {
            Role::Channel(ChannelKind::Discord)
        }
        fn alias(&self) -> &str {
            "ops"
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "recording"
        }
        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }
        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn deliver_sends_the_notice_to_the_named_channel() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let channel = Arc::new(RecordingChannel { sent: sent.clone() });
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("discord.ops".to_string(), channel);
        let adapter = ChannelRouteAdapter::new(channels, Handle::current());

        adapter
            .deliver(
                "discord.ops:98765",
                &GateNotice {
                    run_id: "run-7",
                    sop_name: "triage",
                    step: 3,
                    context: &serde_json::Value::Null,
                    gate_prompt: None,
                },
            )
            .expect("a registered channel delivers");

        // deliver() spawns the send; poll until the recording channel observes it.
        let deadline = std::time::Duration::from_secs(2);
        let got = tokio::time::timeout(deadline, async {
            loop {
                if let Some(m) = sent.lock().unwrap().first().cloned() {
                    break m;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("send task ran within the deadline");
        assert_eq!(got.recipient, "98765");
        assert!(got.content.contains("run-7"));
    }

    #[tokio::test]
    async fn deliver_errors_when_the_route_channel_is_not_configured() {
        let adapter = ChannelRouteAdapter::new(HashMap::new(), Handle::current());
        let err = adapter
            .deliver(
                "discord.ops:98765",
                &GateNotice {
                    run_id: "run-7",
                    sop_name: "triage",
                    step: 3,
                    context: &serde_json::Value::Null,
                    gate_prompt: None,
                },
            )
            .expect_err("an unregistered channel is a real misconfiguration");
        assert!(err.to_string().contains("not a configured channel"));
    }

    #[tokio::test]
    async fn deliver_errors_on_a_malformed_route() {
        let adapter = ChannelRouteAdapter::new(HashMap::new(), Handle::current());
        assert!(
            adapter
                .deliver(
                    "discord.ops",
                    &GateNotice {
                        run_id: "run-7",
                        sop_name: "triage",
                        step: 3,
                        context: &serde_json::Value::Null,
                        gate_prompt: None,
                    },
                )
                .is_err(),
            "a route with no ':recipient' cannot be delivered"
        );
    }
}
