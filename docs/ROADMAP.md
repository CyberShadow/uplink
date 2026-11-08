# uplink - roadmap

This document captures functionality deliberately deferred from the MVP, organized by area.
See `VISION.md` for project goals and non-goals.

The MVP targets a working end-to-end personal communication tool:
one Gmail account, one M365 account (mail + read-only calendar), one Matrix homeserver
(plain text only), a local TODO list, a unified transaction-log-backed undo system,
and a Preact SPA frontend over a single-process Rust backend.

Everything below is in-scope long-term but not part of the MVP cut.

## Matrix

MVP supports only:
- Sending and receiving plain text messages (`m.room.message` with `msgtype: m.text`).
- Joining/leaving rooms; room list with unread counts.
- Unrecognized event types (edits, reactions, replies, threads, media, formatted bodies, mentions, redactions, state changes, etc.) are appended to the timeline as raw JSON.

Deferred:
- Rich formatted body rendering (HTML allowlist sanitization, Markdown compose).
- Image and video display with BlurHash placeholders.
- Reactions (standard and MSC4027 custom-image).
- Replies (`m.in_reply_to` fallback rendering) and threads (`m.thread`).
- Message edits and edit-history viewer.
- Mentions, mention highlights, in-room mention notifications.
- Moderation operations (kick, ban, redact, power-level changes).
- Room property editing (name, topic, avatar, encryption mode, history visibility, join rules).
- Element-style Developer Tools (room state and account-data viewer/editor).
- Emoji picker; MSC2545 custom-emoji picker; image-pack management.
- Code-block syntax highlighting; math (KaTeX) rendering.
- In-room search.
- Read receipts and typing indicators.
- Sticker picker.

Out of scope (intentional, may revisit):
- Native voice/video calling (`m.call.*` / MatrixRTC). Call events render as inline stubs.
- Jitsi and other widget embedding. Widget URLs may render as side-panel links; never iframed.
- Map embeds (`m.location`). Render as link to OpenStreetMap.
- Spaces hierarchy navigation. Spaces appear as rooms in v1.
- Public room directory browsing.

## Matrix cross-signing

MVP:
- Backend uses option 3 (proper cross-signing bootstrap via Secret Storage pairing against an existing trusted device).
- Pairing flow is a CLI subcommand on the backend (e.g. `uplink matrix pair --account=foo`), not an SPA flow.

Deferred:
- SPA-side device-list and verification UI.
- Verifying *other* users' devices from within uplink.

## Email — sending

MVP:
- Gmail send via Gmail API (`users.messages.send`).
- M365 send via Graph (`/me/sendMail`).
- Compose: plain text + WYSIWYG (TipTap) producing HTML.

Deferred:
- SMTP+XOAUTH2 send path for generic IMAP/SMTP accounts.
- Drafts on the wire (RFC 5322 .eml drafts persisted to provider Drafts folder). MVP keeps drafts local-only.
- Scheduled send.
- Mail templates / canned responses.

## Email — receiving

MVP:
- Gmail via the Gmail API, polling `history.list` (see SPEC §8.1).
- M365 via Graph (mail) with delta-link polling every 30 s (no push;
  see SPEC §8.3 and §15.11 — IMAP is not available via Outlook
  impersonation, EWS Streaming deferred).
- Per-account configuration via single YAML file.

Deferred:
- M365 push via EWS Streaming Notifications (or whatever first-party
  Outlook successor uses if one ships first — research bundled with
  the implementation work). See SPEC §15.11. The polling baseline
  stays in place as the always-available fallback.
- Generic IMAP/SMTP accounts (any non-Gmail, non-M365 server).
- Gmail push: API notifications via Cloud Pub/Sub, or IMAP IDLE as a change trigger.
- JMAP support.
- Server-side filters/rules.
- S/MIME and PGP signature verification (read-only).
- Composing signed/encrypted mail.

## Calendar

MVP:
- M365 calendar via Graph, read-only.

Deferred:
- M365 calendar read-write (event create/edit/delete/RSVP).
- Google Calendar (Calendar API).
- Generic CalDAV (Fastmail, Nextcloud, Radicale, etc.).
- iCalendar (.ics) feed subscription.
- Cross-source dedup by `UID`.
- Event reminders / desktop notifications.

## TODO / tasks

MVP:
- Local-table backend (SQLite). Pluggable backend trait, single implementation.
- Mail-flag-for-follow-up creates a linked TODO entry.

Deferred:
- Google Tasks backend.
- Outlook Tasks backend.
- CalDAV VTODO backend.
- Task due dates as calendar events.

## Search

MVP:
- SQLite FTS5 over message subject, from, to, and extracted body text.
- Cross-account search.

Deferred:
- Vectorized / semantic search (e.g. via `sqlite-vec` with locally-hosted embeddings).
- Per-account search scoping UI.
- Saved search subscriptions with notification.

## Transaction log / undo

MVP:
- Every state-mutating operation flows through a single dispatcher that records a tx_log entry before applying.
- Mass operations recorded as one entry with batch inverse.
- Dedicated log UI lists past operations, each with an Undo button.
- Undo itself is appended as a new operation; log is append-only.
- Cross-account scope.

Deferred (designed-for but not implemented):
- Retention horizon (e.g. auto-purge log entries older than N days).
- Hard-delete soft-deletion model with retention window (Gmail-style).

## Real-time / sync

MVP:
- WebSocket transport with app-level heartbeat and exponential reconnect.
- Server-arbitrated compare-and-swap edits for sync'd client state (drafts) using `(hash(old_value), edit_operation)`.
- IndexedDB client-side write-through for draft text resilience.

Deferred:
- OS push notifications.
- Cross-device draft sync to non-browser clients.

## Mobile / Android

Deferred (not blocking but a forward design constraint):
- Native Android app wrapping the SPA in a WebView.
- Push notifications via FCM.
- Mobile-tuned UI density / layout.

## Deployment / operations

MVP:
- Single Rust binary listening on one TCP port (HTTP or HTTPS with BYO cert+key).
- HTTP basic auth.
- All configuration in a single YAML file with inotify-driven reload and frontend push of applicable changes.
- btrfs snapshots for backup (user-managed).

Deferred:
- ACME / Let's Encrypt integration.
- Multiple binary distribution targets (Docker, NixOS module, deb/rpm).
- Off-host WAL streaming (Litestream).
- Audit log for admin operations.

## Other

Deferred:
- Contact management (CardDAV).

Out of scope (per VISION non-goals):
- Multi-tenancy.
- Offline mode.
- End-to-end encryption on the client (server is the trust boundary).
- Chat protocols other than Matrix (use Matrix bridges).
