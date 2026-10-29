# mk-feed

Plan, schedule and publish posts from the terminal. LinkedIn today.
**Files are the queue**, the Keychain holds the token, and nothing is ever
published twice.

Built because Postiz is a GUI over seven services and this needs to be a TUI
over none. The LinkedIn transport is a reimplementation of what
[Postiz](https://github.com/gitroomhq/postiz-app) taught about its API
(`postiz-app@64f00fd6`, AGPL — read, not vendored).

## The three verbs

| | who | what |
|---|---|---|
| `add` | an agent | writes a draft: hook, body, **artifact** |
| `schedule` / `send` | an agent or you | puts it on the calendar, or publishes now |
| `tui` | you | reads the queue, `e` to edit, `s` to publish |

A post is one Markdown file with frontmatter. Editing it with `$EDITOR` is a
supported workflow, not a fallback.

```yaml
---
id: "rag-chunking-bug"
hook: "Mi baseline de chunking partía una función por la mitad"
artifact: "https://github.com/.../commit/abc123 + tests/test_chunking.py"
state: "scheduled"          # draft ready scheduled attempting posted failed unknown cancelled
mode: "confirm"             # auto | confirm
at: "2026-11-02T09:10:00-05:00"
visibility: "PUBLIC"
media: []                   # up to 3 images, in order — not uploads/tests
urn: ""                     # set on publish; the public URL is derived from it
posted_at: ""
note: ""
---
Cuerpo del post.
```

## Why it cannot double-post

LinkedIn has **no idempotency key**, and without Marketing Developer Platform
approval this tool has **no read scope**, so a timeout is genuinely unknowable.
The design follows from that:

1. `check` runs first; a failing draft is refused (exit 3) unless `--force`.
2. The queue lock serialises the daemon against the TUI and other shells.
3. `attempting` is written **to disk before the POST** — the state file is the
   mutex across process lifetimes, which is what makes a restart under a supervisor safe.
4. The outcome is terminal: `posted`, `failed` (nothing published, safe to
   retry) or `unknown` (**never retried automatically** — a human looks).

`daemon` reconciles on boot: anything caught in `attempting` becomes `unknown`.
That is the whole restart story, and `tests/cli.rs` proves it.

## Modes

`confirm` is the default and `auto` must be asked for — per post
(`schedule --mode auto`) or per config (`default_mode`). A missed post costs a
slot; an unattended bad post costs credibility with the exact audience this
feed exists for.

**Flip criterion** (not a vibe): after **10 published posts with zero `unknown`,
zero `failed`, and zero edits after `schedule`**, set `default_mode = "auto"`.

## Install

```bash
cargo build --release && install -m 0755 target/release/mk-feed ~/.local/bin/mk-feed
mk-feed cfg --init          # writes ~/.config/mk-feed/config.toml
```

Then the one-time LinkedIn setup (all free, no app review):

1. A LinkedIn **Page** you admin (app creation asks for one).
2. Create the app → **Products**: add *Sign In with LinkedIn using OpenID
   Connect* and *Share on LinkedIn* (both instant, grants `openid profile
   w_member_social`).
3. **Auth** tab → redirect URL `http://127.0.0.1:8787/callback` (must match
   `redirect_port`). If LinkedIn refuses a loopback URL, register any https URL
   and use `mk-feed auth --url '<pasted redirect URL>'`.
4. `client_id` into the config; the secret into the Keychain — copied from the
   portal, never typed on a command line:
   ```bash
   pbpaste | mk-feed auth --store-secret
   ```
5. `mk-feed auth` → `mk-feed doctor --online`.

**Tokens, measured 2026-10-28:** the self-serve tier issues an access token valid
for **60 days** and **no refresh token** — programmatic refresh is a Marketing
Developer Platform feature. So the access token is the only credential: `doctor`
counts down the days and fails a week out, and re-authorizing is one
`mk-feed auth` every two months. The code still handles a refresh token if the
app ever gets one.

## Verbs

```
mk-feed doctor [--online] [--json]      config, token age, scopes, author, API-version deadline
mk-feed auth [--url <pasted>] [--forget]
mk-feed cfg [--init]                    resolved config
mk-feed add --hook .. --artifact ..     body from --body, --body-file or stdin
mk-feed list [--state X] [--json]
mk-feed show <id> [--json]              includes the lint verdict
mk-feed style                           prints STYLE.md from the queue dir
mk-feed check <id>                      0 clean · 3 fail · 4 warnings
mk-feed schedule <id> --at "2026-11-02 09:10" [--mode auto|confirm]
mk-feed send <id> [--dry-run] [--force]
mk-feed cancel <id>
mk-feed mark <id> [--url <url>]        the paste path: record a manual publish
mk-feed log [--tail N]
mk-feed tui
mk-feed daemon [--once] [--interval 60]
```

Exit codes are deliberate: `0` ok · `1` error · `2` usage · `3` refused ·
`4` partial (warnings, or an `unknown` publish). `--json` prints exactly one
document; never parse the human output.

## The daemon

Any supervisor works — launchd, systemd, cron, or a process manager of your
choice — pointed at `ops/daemon.sh`, with the repo root as the working directory.

The wrapper is three lines because many supervisors run scripts through a shell:
`exec` replaces that shell with the Rust process, so the supervisor's pid, signals
and log capture belong to the daemon itself.

```bash
./ops/daemon.sh     # foreground first: watch it tick
# then register it with whatever keeps processes alive on your machine
```

## Layout

```
src/main.rs     verbs, exit codes
src/auth.rs     OAuth (loopback + paste), refresh, Keychain
src/api.rs      POST /rest/posts  (the only network write)
src/publish.rs  lint → lock → attempting → POST → outcome
src/check.rs    the house format, as code
src/state.rs    frontmatter, the queue, the lock
src/daemon.rs   poll, notify, boot reconcile
src/tui.rs      ratatui
```

## Images

Up to three, PNG or JPEG, listed in `media:` and resolved relative to the queue
directory. One image uses `content.media`, several use `content.multiImage`, and
the array order is the post order — which is the point of a format that says
"arquitectura, captura, métricas".

Uploads happen **before** the post is armed (`initializeUpload` then a single
`PUT`), so an upload that fails publishes nothing and leaves the draft where it
was. Personal tokens cannot poll image processing, so the first attempt may be
rejected as "not ready": that is a 4xx, nothing was published, and one retry after
a pause is safe. Verified end to end against the live API, 2026-10-28.

`STYLE.md` beside the drafts is normative — `mk-feed style` prints it,
`check` enforces it. The skill that drives this tool carries no paths, so it
works from any cwd.

## The paste path (no LinkedIn app)

There is no personal-profile-only developer app: LinkedIn requires every app to
be registered against a **Page**, and the Page is the app's permanent publisher.
If that is a price you do not want to pay, the tool still works:

```bash
mk-feed tui          # y copies the exact text, you paste it into LinkedIn
mk-feed mark <id> --url <post-url>   # closes the loop: state, log, PUBLISHED.md
```

`mark` writes the same log entry and the same index row a real publish does, so
the record does not care how the bytes arrived. What you lose is the click, not
the bookkeeping.

## Measured quirks (all verified against the live API, 2026-10-28)

- **`commentary` is not plain text.** It is a "little format" where `( ) [ ] { }
  * _ ~ | < > \` are syntax. An unescaped `(` makes LinkedIn **truncate the post
  at that character and silently discard the rest** — three probes: 229 of 1348
  characters arrived, then 17 of 327, while square brackets came through fine.
  `mk-feed` escapes the whole set on the wire (the list mirrors Postiz's
  `fixText`, read then verified, not copied), so drafts keep their parentheses
  and arrive intact. `send --dry-run --json` prints the escaped payload.
- **No markdown.** `**bold**` and `` `code` `` render as raw characters. Escaped,
  they are harmless literal characters rather than post-breaking syntax.
- **The feed shows ~229 characters** and hides the rest behind "ver más". Not a
  defect: put the claim in the first line.
- **Blank lines survive** through the API, unlike pasting into the web composer.
- **Long URLs get shortened** by LinkedIn (`github.com/...` becomes `lnkd.in/...`).

## Scope

A personal tool, published because the work is worth reading. It does one
network, text only, on one machine. Issues may go unanswered and there is no
roadmap: the pinned `LinkedIn-Version` is the thing most likely to rot first,
and `doctor` is what will say so.

## Not here yet

Media (3 images, PDF carousel — the personal-token polling quirk is documented
in Postiz's provider), other networks, analytics (that scope is gated behind
Marketing Developer Platform approval; it is free, it is just not automatic).
