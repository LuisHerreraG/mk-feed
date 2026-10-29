# Privacy policy — mk-feed

`mk-feed` is a local command-line tool. It runs on the user's own machine and
has no server, no account system and no telemetry.

## What it stores

- **OAuth tokens for LinkedIn** — stored in the macOS Keychain
  (`security add-generic-password`, service `mk-feed`). They never leave the
  machine except as the `Authorization` header of the requests the user asks for.
- **The queue** — Markdown files in a directory the user chooses.
- **A publish log** — `publish.jsonl` under the user's state directory: post id,
  timestamp, outcome, URL.

## What it sends, and to whom

Only to `api.linkedin.com` and `www.linkedin.com`, only when the user runs
`mk-feed auth` (OAuth) or publishes a post. Nothing is sent to the author of this
tool or to any third party.

## What it does not do

No analytics, no crash reporting, no scraping, no background upload, no selling
of data, no third-party SDKs.

## Deleting your data

`mk-feed auth --forget` removes the tokens from the Keychain. Deleting the queue
directory and the state directory removes everything else.

## Contact

Open an issue at https://github.com/LuisHerreraG/mk-feed/issues
