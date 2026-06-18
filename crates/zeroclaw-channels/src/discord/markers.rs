//! Outbound media markers and the egress trust boundary.
//!
//! The agent emits `[IMAGE:…]` / `[DOCUMENT:…]` / `[VIDEO:…]` / `[AUDIO:…]` /
//! `[VOICE:…]` markers in its reply text. This module parses them out, validates
//! each target against the workspace sandbox (only `http(s)` URLs and absolute
//! paths inside `workspace_dir` may be exposed to chatters), and renders the
//! count-only delivery-failure note and the 🚫/⚠️ reactions when a target is
//! dropped.

use anyhow::Context as _;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use zeroclaw_runtime::i18n;

use super::embed::{DiscordEmbed, EmbedAuthor, EmbedField, EmbedFooter, EmbedMedia};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscordAttachmentKind {
    Image,
    Document,
    Video,
    Audio,
    Voice,
}

impl DiscordAttachmentKind {
    fn from_marker(kind: &str) -> Option<Self> {
        match kind.trim().to_ascii_uppercase().as_str() {
            "IMAGE" | "PHOTO" => Some(Self::Image),
            "DOCUMENT" | "FILE" => Some(Self::Document),
            "VIDEO" => Some(Self::Video),
            "AUDIO" => Some(Self::Audio),
            "VOICE" => Some(Self::Voice),
            _ => None,
        }
    }

    pub(crate) fn marker_name(&self) -> &'static str {
        match self {
            Self::Image => "IMAGE",
            Self::Document => "DOCUMENT",
            Self::Video => "VIDEO",
            Self::Audio => "AUDIO",
            Self::Voice => "VOICE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscordAttachment {
    pub(crate) kind: DiscordAttachmentKind,
    pub(crate) target: String,
}

pub(crate) fn parse_attachment_markers(message: &str) -> (String, Vec<DiscordAttachment>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut attachments = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel_start) = message[cursor..].find('[') {
        let start = cursor + rel_start;
        cleaned.push_str(&message[cursor..start]);

        let Some(rel_end) = message[start..].find(']') else {
            cleaned.push_str(&message[start..]);
            cursor = message.len();
            break;
        };
        let end = start + rel_end;
        let marker_text = &message[start + 1..end];

        let parsed = marker_text.split_once(':').and_then(|(kind, target)| {
            let kind = DiscordAttachmentKind::from_marker(kind)?;
            let target = target.trim();
            if target.is_empty() {
                return None;
            }
            Some(DiscordAttachment {
                kind,
                target: target.to_string(),
            })
        });

        if let Some(attachment) = parsed {
            attachments.push(attachment);
        } else {
            cleaned.push_str(&message[start..=end]);
        }

        cursor = end + 1;
    }

    if cursor < message.len() {
        cleaned.push_str(&message[cursor..]);
    }

    (cleaned.trim().to_string(), attachments)
}

// ─────────────────────────────────────────────────────────────────────────────
// Embed author surface: the `[EMBED:{json}]` marker
//
// An agent emits `[EMBED:{ …discord embed json… }]` to attach a rich embed.
// Unlike the media markers (whose payload is a single path/URL), the embed
// payload is a JSON object that may itself contain `]`, so it is extracted with
// a brace-aware scan rather than the first-`]` rule. Every URL the author puts
// in an embed (image/thumbnail/url/author.url/author.icon_url/footer.icon_url)
// is fetched or linked by Discord, so each routes through the same
// `validate_marker_target` egress trust boundary as a media marker — only
// `http(s)` URLs survive; local paths and other schemes are dropped.
// ─────────────────────────────────────────────────────────────────────────────

const EMBED_TAG: &str = "[EMBED:";

/// Author-supplied embed shape, deserialized from the `[EMBED:{json}]` payload.
/// Mirrors [`DiscordEmbed`] but takes bare URL strings for media and is lenient
/// about unknown keys (an agent typo drops the key, not the whole embed).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub(crate) struct EmbedSpec {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) color: Option<u32>,
    #[serde(default)]
    pub(crate) timestamp: Option<String>,
    #[serde(default)]
    pub(crate) footer: Option<EmbedFooterSpec>,
    #[serde(default)]
    pub(crate) image: Option<EmbedMediaSpec>,
    #[serde(default)]
    pub(crate) thumbnail: Option<EmbedMediaSpec>,
    #[serde(default)]
    pub(crate) author: Option<EmbedAuthorSpec>,
    #[serde(default)]
    pub(crate) fields: Vec<EmbedFieldSpec>,
}

/// An embed `image`/`thumbnail` value. Discord models these as objects
/// (`{ "url": … }`), which is what an agent following the "Discord embed JSON
/// object" affordance emits; a bare URL string is also accepted for leniency.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum EmbedMediaSpec {
    Url(String),
    Object { url: String },
}

impl EmbedMediaSpec {
    fn into_url(self) -> String {
        match self {
            Self::Url(url) => url,
            Self::Object { url } => url,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub(crate) struct EmbedFooterSpec {
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub(crate) struct EmbedAuthorSpec {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub(crate) struct EmbedFieldSpec {
    pub(crate) name: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) inline: bool,
}

/// Parse `[EMBED:{json}]` markers out of `message`, returning the marker-free
/// text and the parsed specs in author order. A malformed marker (bad JSON,
/// missing closing `]`) is left verbatim so the author sees it failed rather
/// than having it silently vanish.
pub(crate) fn parse_embed_markers(message: &str) -> (String, Vec<EmbedSpec>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut specs = Vec::new();
    let mut cursor = 0usize;

    while cursor < message.len() {
        let Some(rel) = find_ci(&message[cursor..], EMBED_TAG) else {
            break;
        };
        let tag_start = cursor + rel;
        match scan_one_embed(message, tag_start) {
            Some((end, Some(spec))) => {
                cleaned.push_str(&message[cursor..tag_start]);
                specs.push(spec);
                cursor = end;
            }
            Some((end, None)) => {
                // A structurally-complete `[EMBED:{…}]` whose JSON failed to
                // deserialize. Keep the whole span verbatim so the author sees
                // it failed, and skip PAST it — never re-scan inside the
                // rejected JSON (a nested `[EMBED:` there must not be parsed).
                cleaned.push_str(&message[cursor..end]);
                cursor = end;
            }
            None => {
                // Not a structural marker: keep the `[` literal and re-scan
                // from just past it.
                cleaned.push_str(&message[cursor..=tag_start]);
                cursor = tag_start + 1;
            }
        }
    }
    if cursor < message.len() {
        cleaned.push_str(&message[cursor..]);
    }
    (cleaned.trim().to_string(), specs)
}

/// Scan a single `[EMBED:{json}]` whose `[` is at `tag_start`. Locates the
/// structural span first (so a serde rejection can still be skipped over as a
/// unit), then attempts to deserialize. Returns:
/// * `None` — not a structural marker (no `{`, unbalanced braces, no `]`),
/// * `Some((end, Some(spec)))` — a valid marker ending just past `]` at `end`,
/// * `Some((end, None))` — a structural span whose JSON was invalid.
fn scan_one_embed(message: &str, tag_start: usize) -> Option<(usize, Option<EmbedSpec>)> {
    let after_tag = tag_start + EMBED_TAG.len();
    let brace = next_non_ws(message, after_tag)?;
    if message.as_bytes().get(brace) != Some(&b'{') {
        return None;
    }
    let obj_end = json_object_end(message, brace)?;
    let close = next_non_ws(message, obj_end)?;
    if message.as_bytes().get(close) != Some(&b']') {
        return None;
    }
    let spec = serde_json::from_str::<EmbedSpec>(&message[brace..obj_end]).ok();
    Some((close + 1, spec))
}

/// Byte index of the next non-whitespace char at or after `from`.
fn next_non_ws(message: &str, from: usize) -> Option<usize> {
    message[from..]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| from + i)
}

/// Given `start` indexing a `{`, return the byte index just past the matching
/// `}`, honoring nested objects and JSON strings/escapes. `None` if unbalanced.
fn json_object_end(message: &str, start: usize) -> Option<usize> {
    let bytes = message.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &c) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Case-insensitive (ASCII) substring search.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Convert an author [`EmbedSpec`] into a wire [`DiscordEmbed`], routing every
/// URL through [`validate_marker_target`]: only `http(s)` URLs survive (Discord
/// fetches/links them server-side), so a local path or disallowed scheme drops
/// that field and records a [`DiscordMarkerFailure`]. Returns `None` when the
/// embed has no content left to render.
pub(crate) fn spec_to_embed(
    spec: EmbedSpec,
    workspace_dir: Option<&Path>,
) -> (Option<DiscordEmbed>, Vec<DiscordMarkerFailure>) {
    let mut failures = Vec::new();
    let mut vet = |url: Option<String>| -> Option<String> {
        let url = url?;
        match vet_embed_url(&url, workspace_dir) {
            Ok(url) => Some(url),
            Err(failure) => {
                failures.push(failure);
                None
            }
        }
    };

    let footer = spec.footer.map(|f| EmbedFooter {
        text: f.text,
        icon_url: vet(f.icon_url),
    });
    let author = spec.author.map(|a| EmbedAuthor {
        name: a.name,
        url: vet(a.url),
        icon_url: vet(a.icon_url),
    });
    let image = vet(spec.image.map(EmbedMediaSpec::into_url)).map(|url| EmbedMedia { url });
    let thumbnail = vet(spec.thumbnail.map(EmbedMediaSpec::into_url)).map(|url| EmbedMedia { url });
    let url = vet(spec.url);
    let fields = spec
        .fields
        .into_iter()
        .map(|f| EmbedField {
            name: f.name,
            value: f.value,
            inline: f.inline,
        })
        .collect();

    let embed = DiscordEmbed {
        title: spec.title,
        description: spec.description,
        url,
        color: spec.color,
        timestamp: spec.timestamp,
        footer,
        image,
        thumbnail,
        author,
        fields,
    };
    if embed.is_empty() {
        (None, failures)
    } else {
        (Some(embed), failures)
    }
}

/// Vet a single embed URL: accept only `http(s)` (Discord fetches/links it),
/// mapping a local-path or scheme rejection to a [`DiscordMarkerFailure`].
fn vet_embed_url(url: &str, workspace_dir: Option<&Path>) -> Result<String, DiscordMarkerFailure> {
    match validate_marker_target(url, workspace_dir) {
        Ok(DiscordMarkerTarget::Http(url)) => Ok(url),
        Ok(DiscordMarkerTarget::Local(_)) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(
                        ::serde_json::json!({ "url": url, "reason": "local_not_embeddable" })
                    ),
                "discord: embed URL is a local path; Discord cannot fetch local files for embeds"
            );
            Err(DiscordMarkerFailure::Refused)
        }
        Err(e) => Err(e.kind()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Interactive components marker — `[COMPONENTS:{json}]`
//
// The agent emits a single `[COMPONENTS:{…}]` marker carrying one JSON object
// `{"rows": [ [ <component>, … ], … ]}`. Each action button / select option may
// carry a server-side `prompt` that is enqueued as a new agent turn when the
// component is clicked. The marker is parsed out on the outgoing path (`send`),
// its prompts are registered in the channel's single-use `PendingComponents`
// registry, and the rendered action rows ride along on the first message chunk.
//
// Trust note: the `prompt` is the *agent's own* text (same trust as any other
// model output). It is registered server-side at emit time and bound to a
// freshly-minted `custom_id`; a click resolves only that registered prompt and
// never anything reconstructed from the click payload (see `pending.rs`).
// ─────────────────────────────────────────────────────────────────────────────

/// One component declared inside a `[COMPONENTS:{…}]` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ComponentSpec {
    /// An action button: a click enqueues `prompt` as a new agent turn.
    Button {
        label: String,
        style: super::components::ButtonStyle,
        prompt: String,
    },
    /// A link button: opens `url`, never dispatched back to the bot.
    Link { label: String, url: String },
    /// A string-select menu: choosing an option enqueues that option's `prompt`.
    Select {
        placeholder: String,
        options: Vec<ComponentOptionSpec>,
    },
}

/// One choice in a `[COMPONENTS:…]` select. `value` is the agent-supplied option
/// value (shown to no one); `prompt` is enqueued when the option is chosen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComponentOptionSpec {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) prompt: String,
}

/// Map a textual style name to a [`ButtonStyle`]. Unknown / missing → Secondary
/// (a neutral default rather than a parse failure that would drop the button).
fn button_style_from_str(s: Option<&str>) -> super::components::ButtonStyle {
    use super::components::ButtonStyle;
    match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("primary") => ButtonStyle::Primary,
        Some("success") => ButtonStyle::Success,
        Some("danger") => ButtonStyle::Danger,
        // "secondary" and anything unrecognized.
        _ => ButtonStyle::Secondary,
    }
}

/// Parse one component JSON object into a [`ComponentSpec`]. `None` for a shape
/// that can't be rendered (e.g. a button with no label, a select with no
/// options) so the caller can skip it without failing the whole send.
fn component_from_json(v: &serde_json::Value) -> Option<ComponentSpec> {
    let obj = v.as_object()?;

    // Select: distinguished by the `select` key (its placeholder text).
    if let Some(placeholder) = obj.get("select") {
        let placeholder = placeholder.as_str().unwrap_or("").to_string();
        let options: Vec<ComponentOptionSpec> = obj
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        let oo = o.as_object()?;
                        let label = oo.get("label")?.as_str()?.to_string();
                        if label.is_empty() {
                            return None;
                        }
                        let value = oo
                            .get("value")
                            .and_then(|x| x.as_str())
                            .unwrap_or(label.as_str())
                            .to_string();
                        let prompt = oo
                            .get("prompt")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(ComponentOptionSpec {
                            label,
                            value,
                            prompt,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return None;
        }
        return Some(ComponentSpec::Select {
            placeholder,
            options,
        });
    }

    // Buttons require a label.
    let label = obj.get("label").and_then(|x| x.as_str())?.to_string();
    if label.is_empty() {
        return None;
    }
    // Link button: distinguished by the `url` key.
    if let Some(url) = obj.get("url").and_then(|x| x.as_str()) {
        if url.is_empty() {
            return None;
        }
        return Some(ComponentSpec::Link {
            label,
            url: url.to_string(),
        });
    }
    // Action button.
    let prompt = obj
        .get("prompt")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(ComponentSpec::Button {
        label,
        style: button_style_from_str(obj.get("style").and_then(|x| x.as_str())),
        prompt,
    })
}

/// Parse the `{"rows": [[…], …]}` body of a `[COMPONENTS:…]` marker into row
/// specs. Returns `None` when the JSON is malformed or carries no renderable
/// rows, so the caller drops the marker rather than 400-ing the send.
fn parse_components_body(json: &str) -> Option<Vec<Vec<ComponentSpec>>> {
    let parsed: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let rows = parsed.get("rows")?.as_array()?;
    let rows: Vec<Vec<ComponentSpec>> = rows
        .iter()
        .filter_map(|row| {
            let comps: Vec<ComponentSpec> = row
                .as_array()?
                .iter()
                .filter_map(component_from_json)
                .collect();
            (!comps.is_empty()).then_some(comps)
        })
        .collect();
    (!rows.is_empty()).then_some(rows)
}

/// The marker tag, including the trailing colon — `[COMPONENTS:`.
const COMPONENTS_TAG: &str = "[COMPONENTS:";

/// Strip a single `[COMPONENTS:{json}]` marker from `message`, returning the
/// stripped text and the parsed row specs (empty when no valid marker was
/// present). Unlike the attachment scanner this is JSON-aware: the marker body
/// is a JSON object that itself contains `[`/`]`, so the close bracket is found
/// by tracking brace depth and string state rather than the first `]`.
///
/// Tolerance mirrors `parse_attachment_markers`: a malformed or empty-result
/// marker is dropped from the text (its raw JSON should never leak to chatters)
/// but never fails the send. Only the first marker is honored; any further
/// `[COMPONENTS:…]` markers are left untouched in the text.
pub(crate) fn parse_component_markers(message: &str) -> (String, Vec<Vec<ComponentSpec>>) {
    let Some(tag_start) = message.find(COMPONENTS_TAG) else {
        return (message.to_string(), Vec::new());
    };
    let body_start = tag_start + COMPONENTS_TAG.len();

    // Walk from the start of the JSON body to the marker's closing `]`,
    // skipping any `]` that sits inside the JSON (in a string or nested array).
    let Some(close_rel) = find_marker_close(&message[body_start..]) else {
        // No closing bracket — leave the text untouched (it isn't a marker).
        return (message.to_string(), Vec::new());
    };
    let close = body_start + close_rel;
    let json = &message[body_start..close];

    let mut cleaned = String::with_capacity(message.len());
    cleaned.push_str(&message[..tag_start]);
    cleaned.push_str(&message[close + 1..]);
    let cleaned = cleaned.trim().to_string();

    let rows = parse_components_body(json).unwrap_or_default();
    (cleaned, rows)
}

/// Given the text immediately after `[COMPONENTS:`, return the byte offset of
/// the marker's closing `]` (relative to that slice), tracking JSON string and
/// brace/bracket nesting so a `]` inside the JSON body doesn't end the marker.
/// `None` when no balanced close is found.
fn find_marker_close(after_tag: &str) -> Option<usize> {
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in after_tag.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' => depth -= 1,
            ']' => {
                if depth == 0 {
                    // Top-level `]` closes the marker.
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}


/// Resolved outbound attachment target after sandbox validation.
#[derive(Debug)]
pub(crate) enum DiscordMarkerTarget {
    Local(PathBuf),
    Http(String),
}

/// Why a marker target was rejected. Drives the user-facing emoji reaction
/// on the bot's outgoing message: `Refused` (trust-boundary rejection) maps
/// to 🚫, `NotFound` (path didn't resolve on disk) maps to ⚠️. The
/// distinction matters because a chatter should see at a glance that the
/// bot deliberately declined a target rather than tried and failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscordMarkerFailure {
    /// Trust-boundary refusal: disallowed scheme, relative path, missing
    /// workspace_dir, or canonicalised path outside the workspace.
    Refused,
    /// Path passed scheme/absolute/workspace checks but did not resolve
    /// to anything on disk.
    NotFound,
}

#[derive(Debug)]
pub(crate) enum DiscordMarkerError {
    Refused(anyhow::Error),
    NotFound(anyhow::Error),
}

impl DiscordMarkerError {
    pub(crate) fn kind(&self) -> DiscordMarkerFailure {
        match self {
            Self::Refused(_) => DiscordMarkerFailure::Refused,
            Self::NotFound(_) => DiscordMarkerFailure::NotFound,
        }
    }
}

impl std::fmt::Display for DiscordMarkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) | Self::NotFound(e) => write!(f, "{e}"),
        }
    }
}

/// Validate an outbound marker target against Discord's trust-boundary policy.
///
/// The orchestrator system prompt mandates absolute paths for media markers,
/// and the workspace is the only directory the agent is authorised to
/// expose to chatters:
///
/// * `http`/`https` URLs are accepted and inlined as links.
/// * Any other URL scheme (`file:`, `data:`, custom `://`) is refused.
/// * Local paths must be absolute. Relative paths are agent
///   misconfiguration and dropped, not silently resolved against cwd.
/// * Absolute paths are canonicalised and must resolve inside
///   `workspace_dir`. Anything outside or any traversal escape is
///   refused; a path that simply doesn't exist on disk returns
///   `NotFound`, which the caller renders differently from a refusal.
/// * When `workspace_dir` is not configured, no local path can be safely
///   bounded, so all local targets are refused.
pub(crate) fn validate_marker_target(
    target: &str,
    workspace_dir: Option<&Path>,
) -> Result<DiscordMarkerTarget, DiscordMarkerError> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(DiscordMarkerTarget::Http(target.to_string()));
    }
    if target.contains("://") {
        let scheme = target.split("://").next().unwrap_or("?");
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "scheme": scheme,
                    "target": target,
                })),
            "discord: marker target uses disallowed scheme"
        );
        return Err(DiscordMarkerError::Refused(anyhow::Error::msg(format!(
            "marker target uses disallowed scheme {scheme:?}; only http/https and absolute workspace paths are accepted"
        ))));
    }
    if target.starts_with("data:") || target.starts_with("file:") {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"target": target})),
            "discord: marker target uses disallowed data: or file: scheme"
        );
        return Err(DiscordMarkerError::Refused(anyhow::Error::msg(
            "marker target uses disallowed scheme; only http/https and absolute workspace paths are accepted",
        )));
    }

    let target_path = Path::new(target);
    if !target_path.is_absolute() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "target": target,
                    "reason": "not_absolute",
                })),
            "discord: marker target is not absolute"
        );
        return Err(DiscordMarkerError::Refused(anyhow::Error::msg(format!(
            "marker target {target} is not an absolute path; the agent must emit absolute paths inside workspace_dir"
        ))));
    }

    let workspace = workspace_dir.ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "target": target,
                    "reason": "no_workspace_dir",
                })),
            "discord: marker target is local path but channel has no workspace_dir"
        );
        DiscordMarkerError::Refused(anyhow::Error::msg(format!(
            "marker target {target} is a local path but the channel was started without a workspace_dir, refusing for safety"
        )))
    })?;
    let workspace_canon = std::fs::canonicalize(workspace)
        .with_context(|| format!("canonicalize workspace {}", workspace.display()))
        .map_err(DiscordMarkerError::Refused)?;
    let target_canon = match std::fs::canonicalize(target_path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "target": target,
                        "reason": "not_found",
                    })),
                "discord: marker target not found on disk"
            );
            return Err(DiscordMarkerError::NotFound(anyhow::Error::msg(format!(
                "marker target {target} not found on disk"
            ))));
        }
        Err(e) => {
            return Err(DiscordMarkerError::Refused(
                anyhow::Error::from(e).context(format!("canonicalize marker target {target}")),
            ));
        }
    };

    if !target_canon.starts_with(&workspace_canon) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "target": target,
                    "target_canon": target_canon.display().to_string(),
                    "workspace_canon": workspace_canon.display().to_string(),
                    "reason": "outside_workspace",
                })),
            "discord: marker target escapes workspace_dir"
        );
        return Err(DiscordMarkerError::Refused(anyhow::Error::msg(format!(
            "marker target {target} resolves to {} which is outside workspace_dir {}; refusing",
            target_canon.display(),
            workspace_canon.display(),
        ))));
    }
    Ok(DiscordMarkerTarget::Local(target_canon))
}

pub(crate) fn classify_outgoing_attachments(
    attachments: &[DiscordAttachment],
    workspace_dir: Option<&Path>,
) -> (Vec<PathBuf>, Vec<String>, Vec<DiscordMarkerFailure>) {
    let mut local_files = Vec::new();
    let mut remote_urls = Vec::new();
    let mut failures = Vec::new();

    for attachment in attachments {
        match validate_marker_target(&attachment.target, workspace_dir) {
            Ok(DiscordMarkerTarget::Local(path)) => local_files.push(path),
            Ok(DiscordMarkerTarget::Http(url)) => remote_urls.push(url),
            Err(e) => {
                let kind_label = match e.kind() {
                    DiscordMarkerFailure::Refused => "trust boundary",
                    DiscordMarkerFailure::NotFound => "not found",
                };
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"kind": attachment.kind.marker_name(), "target": attachment.target, "reason": kind_label, "error": format!("{}", e)})), "dropping unresolved outbound attachment marker");
                failures.push(e.kind());
            }
        }
    }

    (local_files, remote_urls, failures)
}

/// Build the count-only delivery failure tail appended to the bot's reply
/// when at least one marker was dropped. Returns `None` when the failure
/// list is empty so callers can keep the body untouched.
pub(crate) fn delivery_failure_note(failures: &[DiscordMarkerFailure]) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    let count = failures.len().to_string();
    let key = if failures.len() == 1 {
        "channel-discord-delivery-failure-note-one"
    } else {
        "channel-discord-delivery-failure-note-many"
    };
    Some(i18n::get_required_cli_string_with_args(
        key,
        &[("count", count.as_str())],
    ))
}

/// Compose the final reply body with the delivery-failure note appended.
/// When the marker-stripped content is empty the note replaces the body;
/// otherwise the note follows the content separated by a blank line.
pub(crate) fn compose_body_with_failure_note(content: &str, note: Option<&str>) -> String {
    match note {
        Some(note) if content.trim().is_empty() => note.to_string(),
        Some(note) => format!("{content}\n\n{note}"),
        None => content.to_string(),
    }
}

/// Emoji reactions applied to the bot's own outgoing message based on which
/// kinds of marker failures occurred. 🚫 signals a trust-boundary refusal,
/// ⚠️ signals a post-validation delivery failure. Both can fire on the
/// same message when a batch mixes refusals and not-found targets.
pub(crate) fn decide_failure_reactions(failures: &[DiscordMarkerFailure]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if failures
        .iter()
        .any(|k| matches!(k, DiscordMarkerFailure::Refused))
    {
        out.push("🚫");
    }
    if failures
        .iter()
        .any(|k| matches!(k, DiscordMarkerFailure::NotFound))
    {
        out.push("⚠️");
    }
    out
}

pub(crate) fn with_inline_attachment_urls(content: &str, remote_urls: &[String]) -> String {
    let mut lines = Vec::new();
    if !content.trim().is_empty() {
        lines.push(content.trim().to_string());
    }
    if !remote_urls.is_empty() {
        lines.extend(remote_urls.iter().cloned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod embed_tests {
    use super::*;

    #[test]
    fn parses_a_single_embed_and_strips_it() {
        let (cleaned, specs) = parse_embed_markers(
            "before [EMBED:{\"title\":\"Hi\",\"description\":\"there\"}] after",
        );
        assert_eq!(cleaned, "before  after");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].title.as_deref(), Some("Hi"));
        assert_eq!(specs[0].description.as_deref(), Some("there"));
    }

    #[test]
    fn brace_aware_scan_tolerates_brackets_inside_json_strings() {
        // A naive first-`]` scan would truncate the JSON here.
        let (cleaned, specs) = parse_embed_markers("x [EMBED:{\"description\":\"a [b] c]\"}] y");
        assert_eq!(cleaned, "x  y");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].description.as_deref(), Some("a [b] c]"));
    }

    #[test]
    fn parses_nested_objects_and_fields_array() {
        let (_, specs) = parse_embed_markers(
            "[EMBED:{\"footer\":{\"text\":\"ft\"},\"fields\":[{\"name\":\"n\",\"value\":\"v\",\"inline\":true}]}]",
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].footer.as_ref().unwrap().text, "ft");
        assert_eq!(specs[0].fields.len(), 1);
        assert!(specs[0].fields[0].inline);
    }

    #[test]
    fn malformed_marker_is_left_verbatim() {
        // Missing closing brace → not a valid marker, kept in the text.
        let (cleaned, specs) = parse_embed_markers("keep [EMBED:{not json] here");
        assert!(specs.is_empty());
        assert_eq!(cleaned, "keep [EMBED:{not json] here");
    }

    #[test]
    fn serde_rejected_span_is_kept_verbatim_and_not_rescanned_inside() {
        // The outer footer is missing its required `text`, so serde rejects the
        // whole (structurally complete) marker. The scanner must skip PAST the
        // span — not re-enter it and extract the nested `[EMBED:` sitting inside
        // the description string as a spurious embed.
        let msg = r#"x [EMBED:{"footer":{"icon_url":"u"},"description":"see [EMBED:{\"title\":\"INNER\"}] now"}] y"#;
        let (cleaned, specs) = parse_embed_markers(msg);
        assert!(
            specs.is_empty(),
            "no embed parsed: outer invalid, inner not re-scanned"
        );
        assert_eq!(
            cleaned,
            msg.trim(),
            "the whole rejected span is preserved verbatim"
        );
    }

    #[test]
    fn tag_is_case_insensitive() {
        let (cleaned, specs) = parse_embed_markers("[embed:{\"title\":\"T\"}]");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].title.as_deref(), Some("T"));
        assert_eq!(cleaned, "");
    }

    #[test]
    fn multiple_embeds_parse_in_order() {
        let (cleaned, specs) =
            parse_embed_markers("[EMBED:{\"title\":\"one\"}] mid [EMBED:{\"title\":\"two\"}]");
        assert_eq!(cleaned, "mid");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].title.as_deref(), Some("one"));
        assert_eq!(specs[1].title.as_deref(), Some("two"));
    }

    #[test]
    fn spec_to_embed_keeps_http_image_and_links() {
        let spec = EmbedSpec {
            title: Some("T".to_string()),
            image: Some(EmbedMediaSpec::Url("https://example.com/i.png".to_string())),
            url: Some("http://example.com".to_string()),
            ..Default::default()
        };
        let (embed, failures) = spec_to_embed(spec, None);
        let embed = embed.expect("non-empty embed");
        assert_eq!(
            embed.image.as_ref().unwrap().url,
            "https://example.com/i.png"
        );
        assert_eq!(embed.url.as_deref(), Some("http://example.com"));
        assert!(failures.is_empty());
    }

    #[test]
    fn image_and_thumbnail_accept_discord_nested_object_and_bare_string() {
        // Discord models image/thumbnail as `{ "url": … }`, which is what an
        // agent following the "Discord embed JSON object" affordance emits.
        let (_, mut specs) = parse_embed_markers(
            r#"[EMBED:{"title":"T","image":{"url":"https://e.com/i.png"},"thumbnail":{"url":"https://e.com/t.png"}}]"#,
        );
        assert_eq!(
            specs.len(),
            1,
            "the nested-media embed parses (does not reject)"
        );
        let (embed, failures) = spec_to_embed(specs.remove(0), None);
        let embed = embed.expect("renders");
        assert_eq!(embed.image.as_ref().unwrap().url, "https://e.com/i.png");
        assert_eq!(embed.thumbnail.as_ref().unwrap().url, "https://e.com/t.png");
        assert!(failures.is_empty());

        // The bare-string form is still accepted.
        let (_, mut bare) = parse_embed_markers(r#"[EMBED:{"image":"https://e.com/b.png"}]"#);
        let (embed, _) = spec_to_embed(bare.remove(0), None);
        assert_eq!(embed.unwrap().image.unwrap().url, "https://e.com/b.png");
    }

    #[test]
    fn spec_to_embed_drops_disallowed_scheme_url_but_keeps_text() {
        let spec = EmbedSpec {
            title: Some("Kept".to_string()),
            image: Some(EmbedMediaSpec::Url("file:///etc/passwd".to_string())),
            ..Default::default()
        };
        let (embed, failures) = spec_to_embed(spec, None);
        let embed = embed.expect("text survives");
        assert_eq!(embed.title.as_deref(), Some("Kept"));
        assert!(embed.image.is_none());
        assert_eq!(failures, vec![DiscordMarkerFailure::Refused]);
    }

    #[test]
    fn spec_to_embed_drops_local_path_image_as_not_embeddable() {
        // A real, in-workspace file still cannot be referenced by URL in an
        // embed — Discord only fetches http(s). It must be refused, not Local.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pic.png");
        std::fs::write(&file, b"x").unwrap();
        let abs = std::fs::canonicalize(&file).unwrap();
        let spec = EmbedSpec {
            description: Some("body".to_string()),
            thumbnail: Some(EmbedMediaSpec::Url(abs.to_string_lossy().to_string())),
            ..Default::default()
        };
        let (embed, failures) = spec_to_embed(spec, Some(dir.path()));
        let embed = embed.expect("description survives");
        assert!(embed.thumbnail.is_none());
        assert_eq!(failures, vec![DiscordMarkerFailure::Refused]);
    }

    #[test]
    fn spec_to_embed_returns_none_when_nothing_renders() {
        let spec = EmbedSpec {
            image: Some(EmbedMediaSpec::Url("file:///etc/passwd".to_string())),
            ..Default::default()
        };
        let (embed, failures) = spec_to_embed(spec, None);
        assert!(embed.is_none());
        assert_eq!(failures, vec![DiscordMarkerFailure::Refused]);
    }
}

#[cfg(test)]
mod component_marker_tests {
    use super::*;
    use crate::discord::components::ButtonStyle;

    #[test]
    fn parses_button_row_and_strips_marker() {
        let msg = "Choose: [COMPONENTS:{\"rows\":[[{\"label\":\"Approve\",\"style\":\"success\",\"prompt\":\"approve it\"},{\"label\":\"Deny\",\"style\":\"danger\",\"prompt\":\"deny it\"}]]}] thanks";
        let (cleaned, rows) = parse_component_markers(msg);
        assert_eq!(cleaned, "Choose:  thanks");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(
            rows[0][0],
            ComponentSpec::Button {
                label: "Approve".into(),
                style: ButtonStyle::Success,
                prompt: "approve it".into(),
            }
        );
        assert_eq!(
            rows[0][1],
            ComponentSpec::Button {
                label: "Deny".into(),
                style: ButtonStyle::Danger,
                prompt: "deny it".into(),
            }
        );
    }

    #[test]
    fn parses_link_button_without_prompt() {
        let (_, rows) = parse_component_markers(
            "[COMPONENTS:{\"rows\":[[{\"label\":\"Docs\",\"url\":\"https://example.com\"}]]}]",
        );
        assert_eq!(
            rows[0][0],
            ComponentSpec::Link {
                label: "Docs".into(),
                url: "https://example.com".into(),
            }
        );
    }

    #[test]
    fn parses_select_with_options() {
        let (_, rows) = parse_component_markers(
            "[COMPONENTS:{\"rows\":[[{\"select\":\"Pick one\",\"options\":[{\"label\":\"A\",\"value\":\"a\",\"prompt\":\"chose a\"},{\"label\":\"B\",\"value\":\"b\",\"prompt\":\"chose b\"}]}]]}]",
        );
        assert_eq!(rows.len(), 1);
        match &rows[0][0] {
            ComponentSpec::Select {
                placeholder,
                options,
            } => {
                assert_eq!(placeholder, "Pick one");
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].label, "A");
                assert_eq!(options[0].value, "a");
                assert_eq!(options[0].prompt, "chose a");
                assert_eq!(options[1].prompt, "chose b");
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn unknown_style_defaults_to_secondary() {
        let (_, rows) = parse_component_markers(
            "[COMPONENTS:{\"rows\":[[{\"label\":\"X\",\"prompt\":\"p\"}]]}]",
        );
        assert!(matches!(
            rows[0][0],
            ComponentSpec::Button {
                style: ButtonStyle::Secondary,
                ..
            }
        ));
    }

    #[test]
    fn nested_brackets_in_prompt_dont_truncate_marker() {
        // A prompt containing `]` (and a JSON array) must not end the marker early.
        let msg =
            "[COMPONENTS:{\"rows\":[[{\"label\":\"Go\",\"prompt\":\"run [tool] now ]]\"}]]}] tail";
        let (cleaned, rows) = parse_component_markers(msg);
        assert_eq!(cleaned, "tail");
        assert_eq!(rows.len(), 1);
        match &rows[0][0] {
            ComponentSpec::Button { prompt, .. } => assert_eq!(prompt, "run [tool] now ]]"),
            other => panic!("expected button, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_tolerated_and_marker_dropped() {
        // Bad JSON: the marker is stripped (so its raw body never leaks) but no
        // rows are produced and the send is NOT failed.
        let (cleaned, rows) = parse_component_markers("before [COMPONENTS:{not valid json}] after");
        assert_eq!(cleaned, "before  after");
        assert!(rows.is_empty());
    }

    #[test]
    fn missing_close_bracket_leaves_text_untouched() {
        // No balanced close — it isn't a marker, so the text is returned as-is.
        let msg = "[COMPONENTS:{\"rows\":[[{\"label\":\"x\"}]]} no close";
        let (cleaned, rows) = parse_component_markers(msg);
        assert_eq!(cleaned, msg);
        assert!(rows.is_empty());
    }

    #[test]
    fn no_marker_returns_input_unchanged() {
        let (cleaned, rows) = parse_component_markers("just a normal message");
        assert_eq!(cleaned, "just a normal message");
        assert!(rows.is_empty());
    }

    #[test]
    fn empty_options_select_is_dropped() {
        // A select with no renderable options yields no rows (dropped, not 400).
        let (cleaned, rows) = parse_component_markers(
            "hi [COMPONENTS:{\"rows\":[[{\"select\":\"p\",\"options\":[]}]]}]",
        );
        assert_eq!(cleaned, "hi");
        assert!(rows.is_empty());
    }
}
