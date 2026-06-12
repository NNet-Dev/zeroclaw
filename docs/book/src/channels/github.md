# GitHub

Converse with the agent through GitHub issue and pull-request comments. ZeroClaw authenticates as a **GitHub App** and replies as the app's own bot identity (`your-app[bot]`), so it works on any repository the app is installed on — no personal access token, no shared user account.

> **Build note:** the GitHub channel is **not included** in the lean default build. Build with `--features channel-github` (or `channels-full`).

## How it works

- **Polling, not webhooks.** The channel polls the GitHub REST API for new issues, pull requests, and comments on a `since` cursor. The daemon needs no public URL, tunnel, or inbound exposure — it works behind NAT.
- **Issue-scoped conversations.** Every message on the same issue or PR shares one conversation thread; the agent replies as a comment on that issue.
- **Streaming replies.** The agent posts a draft comment and edits it in place as the response grows (edits are spaced ≥ 2 s to respect GitHub's abuse limits).
- **Reactions.** Acknowledgement reactions map onto GitHub's fixed reaction set (👀 → `eyes`, ✅ → `+1`, ⚠️ → `confused`, …); unmappable emoji are skipped.
- **Cold start.** Events created before the daemon started are never processed, so restarting can't replay history. The flip side: comments posted while the daemon was down are missed — mention the app again.
- **Comment edits are ignored.** Only newly created comments and issue/PR opening posts trigger the agent.

## Create the GitHub App

1. GitHub → **Settings → Developer settings → GitHub Apps → New GitHub App**.
2. **Webhook:** uncheck *Active* — this channel doesn't use one.
3. **Repository permissions:** Issues *Read & write*, Pull requests *Read & write*, Metadata *Read-only*. Nothing else.
4. After creating, note the **App ID** and **generate a private key** — GitHub downloads a `.pem` file. Move it somewhere stable and `chmod 600` it (looser permissions log a startup warning).
5. **Install the app** on your account or organization, selecting the repositories the agent should see.

## Configure

```toml
[channels.github.default]
enabled = true
app_id = 12345
private_key_path = "~/.zeroclaw/github-app.pem"
repos = ["your-org/your-repo"]   # empty = every repo the installation can see
poll_interval_secs = 30          # minimum 15
mention_only = true              # respond only to comments that @mention the app
# installation_id = 987654       # only needed when the app has several installations
# listen_to_bots = false         # process comments from other bot accounts
```

Or via the CLI:

```bash
zeroclaw config set channels.github.default.app-id 12345
zeroclaw config set channels.github.default.private-key-path ~/.zeroclaw/github-app.pem
zeroclaw config set channels.github.default.enabled true
```

{{#peer-group github}}

## Operating notes

- **Rate budget:** each installation gets 5,000 requests/hour; the channel spends 2 per repository per poll tick (5 repos at a 30 s interval ≈ 1,200/hour). On a rate-limit response the channel backs off until the limit window resets.
- **Many repositories:** when `repos` is empty and the installation can see more than 100 repositories, only the first page is polled (a warning is logged) — list `repos` explicitly in that case.
- **Multiple installations:** one channel alias serves one installation. If the app is installed on several accounts, set `installation_id` (and add more aliases for the others).

## Safety

Issues and PR comments on public repositories are adversarial input. Keep `mention_only = true`, gate senders with a peer group (an empty peer set denies everyone, `["*"]` accepts anyone), and keep autonomy at `Supervised` or lower for public-facing repositories — the same guidance as [social channels](./social.md#operating-social-channels-safely).
