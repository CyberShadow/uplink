# Spike — M365 Outlook desktop client_id impersonation

> **Status: RESOLVED (2026-05-22).** The flow works. M365 mail will go
> through Graph; IMAP fallback is not available via this client_id and
> is removed from the spec. Findings below; the rest of this document
> is preserved as iteration history and as the source for the wire-
> shape details cited in SPEC §13.3.

## 0. Outcome

Ran against a real corporate tenant. Three iterations of the probe;
final results:

| Q  | Result | Notes |
|----|--------|-------|
| Q1 — auth flow | **PASS** | v1 endpoint + `resource=` + URN redirect + no PKCE works. Browser shows a `urn:` redirect Location that it cannot follow; the code is read from DevTools. |
| Q2 — Graph delta | **PASS** | At `/me/mailFolders/inbox/messages/delta`. Root `/me/messages/delta` returns BadRequest — delta is per-folder. |
| Q3 — IMAP XOAUTH2 | **STRUCTURAL FAIL** | Token mint succeeds with correct audience (`outlook.office365.com`), but Outlook desktop's pre-authorised scopes do NOT include `IMAP.AccessAsUser.All` — they cover `EAS / EWS / MapiHttp / Oab / OWA / OutlookService / Mail.* / Calendars.*` etc., because real Outlook doesn't use IMAP. No amount of refresh-token twiddling can produce an IMAP-capable token from this client_id. |
| Q4 — refresh | **PASS** for Graph; **N/A** for IMAP (same structural reason as Q3). |

Decisions taken into the spec on the back of this:

- §8.3: M365 mail through Graph only. No IMAP fallback.
- §8.3 + §15.11: push deferred. MVP polls the Graph delta link every
  30 s per folder. Real push (EWS Streaming Notifications, or whatever
  Microsoft ships as its Graph-native successor) is bundled as future
  work; polling stays as the always-available baseline.
- §13.3: OAuth flow documented exactly — v1 endpoint, `resource=`
  parameter, URN redirect URI, no PKCE, code exchange takes only
  `grant_type/code/redirect_uri/client_id`.
- §15.3: removed the line that mentioned "swap to IMAP+XOAUTH2 as a
  mitigation" — IMAP is not available via this client_id, that
  mitigation never existed.

The Outlook desktop client_id's Graph token grants the full Outlook
permission set: `Mail.ReadWrite Mail.Send Calendars.ReadWrite
Contacts.ReadWrite Notes.Create Files.* People.* User.Read.*` and
more — i.e. everything uplink needs for mail/calendar/contacts.

---

## 1. Why this spike exists

uplink's "drop-in Outlook replacement" goal (`docs/VISION.md`) hinges on
talking to Microsoft 365 mail + calendar from a process that is *not*
Microsoft Outlook, without any tenant-side configuration change (no admin
consent, no Entra app registration, no Intune enrolment).

The strategy is to authenticate as the documented Outlook for Windows
public client (`client_id = d3590ed6-52b3-4102-aeff-aad2292ab01c`). This
is empirically known to work for DavMail, historically Thunderbird, and
K-9 Mail; Microsoft cannot disable it wholesale without breaking Outlook
itself.

What we don't yet know is whether the **specific corporate tenant** the
user authenticates against permits the auth flow we use. Conditional
Access policies can:

- Block device-code flow entirely (a common phishing mitigation; an
  earlier draft of this spike used device code and ran into this — the
  AAD response was "You don't have access to this").
- Reject explicit-scope consent between first-party client and first-
  party resource without tenant-side pre-authorization (raises
  `AADSTS65001`; the v2-OIDC draft of this spike hit this with both
  enumerated scopes and `.default`).
- Require Intune-managed device or WAM-broker tokens (Windows-only
  signal a Linux process cannot produce).
- Restrict third-party apps even when claiming the Outlook client_id.

### 1.1 Flow choice — mirror DavMail's working "EWS default" path bit-for-bit

After two false starts (device code, then v2 OIDC `/oauth2/v2.0/authorize`
with `scope=…/.default`), we read the DavMail source for the path that
actually works against real corporate tenants today:

- `~/work/extern/davmail/src/java/davmail/exchange/auth/O365Authenticator.java`
  — `buildAuthorizeUrl` (lines 65–106) and `authenticate` (lines 140–285).
- `~/work/extern/davmail/src/java/davmail/exchange/auth/O365Token.java`
  — `build` (token exchange, lines 60–95) and `refreshToken` (lines
  220–260).
- `~/work/extern/davmail/src/java/davmail/exchange/auth/O365InteractiveAuthenticator.java`
  — the browser-WebView wrapper that reuses the same `buildAuthorizeUrl`
  and `O365Token.build` (lines 116, 219) and parses the code out of the
  `nativeclient` redirect (lines 236–254).

The default DavMail path uses the **v1 (classic) AAD endpoint** with a
`resource=` parameter, **not** v2 OIDC with `scope=`. There is a load-
bearing comment at `O365Authenticator.java:86–87`:

> *"Outlook desktop can work with classic resource based endpoint for
> authorization"*

The same code/token endpoints work for the interactive (browser)
WebView path (`O365InteractiveAuthenticator.authenticate` line 116
delegates to the same `buildAuthorizeUrl`, line 219 to the same
`O365Token.build`). The only difference between DavMail's "default" and
"interactive" modes is *how the user proves identity to Microsoft*
(scripted form POST vs. real browser) — the OAuth wire shape on the
authorize and token endpoints is identical, and is what this spike
mirrors.

Concretely DavMail's default flow uses, on the wire:

```
# Authorize (browser-driven; we ask the user to open this in their browser)
GET https://login.microsoftonline.com/{tenant}/oauth2/authorize
    ?client_id={Outlook desktop client_id}
    &response_type=code
    &redirect_uri=urn:ietf:wg:oauth:2.0:oob   ← see §1.2 below
    &response_mode=query
    &login_hint={user email}
    &resource=https://graph.microsoft.com   ← or outlook.office365.com for EWS/IMAP

# Token exchange (POST)
POST https://login.microsoftonline.com/{tenant}/oauth2/token
Content-Type: application/x-www-form-urlencoded

grant_type=authorization_code
&code={auth code}
&redirect_uri=urn:ietf:wg:oauth:2.0:oob
&client_id={Outlook desktop client_id}

# (no scope, no resource, no PKCE — DavMail does not use PKCE in
#  either its default or its WebView path)

# Cross-resource refresh
POST https://login.microsoftonline.com/{tenant}/oauth2/token

grant_type=refresh_token
&refresh_token={rt}
&client_id={Outlook desktop client_id}
&redirect_uri=urn:ietf:wg:oauth:2.0:oob
&resource=https://outlook.office365.com   ← target audience for this token mint
```

### 1.2 Redirect URI must match what the *Outlook* client_id has registered, not DavMail's

DavMail's source defaults to
`https://login.microsoftonline.com/common/oauth2/nativeclient`
(`O365Authenticator.java:144`, `O365InteractiveAuthenticator.java:105`).
That works for DavMail because their **own** registered application
`facd6cff-a294-4415-b59f-c5b01937d7bd` is configured in their tenant
with `nativeclient` as one of its allowed redirect URIs. When impersonating
the Outlook desktop client_id, DavMail's documentation directs users to
override **both** `davmail.oauth.clientId` and
`davmail.oauth.redirectUri` together — the Outlook desktop client
`d3590ed6-52b3-4102-aeff-aad2292ab01c` has `urn:ietf:wg:oauth:2.0:oob`
registered as its native-client redirect URI, **not** the `nativeclient`
URL. AAD enforces an exact match between the `redirect_uri` parameter
on the authorize request and the URIs registered on the app registration;
sending the wrong one yields `AADSTS50011 "Reply URL specified in the
request does not match…"` and the sign-in cannot proceed.

The OOB redirect URI is a legacy OAuth 2.0 convention meaning "the
user agent is not actually a web client; the calling app will recover
the code out of band". On modern AAD this presents as one of three
things — observed in the wild:

1. **AAD returns `302 Location: urn:ietf:wg:oauth:2.0:oob?code=…&session_state=…`.**
   The browser cannot follow a `urn:` scheme, so it stops with a
   protocol error and displays nothing useful. You recover the code
   from the browser's network panel (DevTools → Network → the request
   to `/oauth2/authorize` or `/appverify` → look at the response
   `Location:` header). Paste the entire header value into the probe.
   This was the actual behaviour observed against a corporate tenant
   in May 2026.
2. **AAD redirects to `https://login.microsoftonline.com/common/oauth2/nativeclient?code=…&session_state=…`**
   anyway and displays a near-blank page. Paste the full URL from the
   address bar.
3. **AAD lands on a results page** whose title or body contains the
   literal text `Success code=<code>`. Paste either the title or the
   code itself.

The probe accepts any of the above: a full URL, a `urn:` URI, a
`Success code=…` string, or the raw code. Whichever form the browser
or DevTools hands you, paste it and the probe extracts the code via
a `code=` substring scan after attempting URL/URN query parsing.

Why this works where v2/scope-based did not on this tenant: the v1
endpoint pre-dates the explicit-consent model and uses the legacy
"this client is pre-authorized for this resource" pairing that
first-party Microsoft clients (Outlook, OWA, mobile Outlook) ride on.
The `AADSTS65001` "consent must be triggered via pre-authorization"
gate on v2 is precisely the consent surface that the v1 endpoint
sidesteps for first-party-to-first-party pairs.

The spike is the *minimum* deviation from DavMail's wire shape that lets
a user complete the flow without DavMail's embedded WebView: print the
authorize URL, the user opens it in a real browser, completes sign-in
(plus any MFA / Conditional Access prompts), and arrives at the
`nativeclient` near-blank landing page whose URL contains
`?code=…&session_state=…`. The user pastes that URL back. The probe
extracts the `code` (DavMail does the same at `O365Authenticator.java:266`
and `O365InteractiveAuthenticator.java:236–254`), exchanges with the
v1 token endpoint, and runs Q1–Q4.

The spike must be run interactively because the user must complete
sign-in in their own browser, including any MFA / CA prompts the tenant
imposes.

---

## 2. Goal — four discrete pass/fail questions

| Q  | Question | Pass criterion |
|----|----------|----------------|
| Q1 | Does authorization-code (no PKCE) against `client_id = d3590ed6-…`, `authority = login.microsoftonline.com/common`, `redirect_uri = .../oauth2/nativeclient`, v1 endpoint, `resource = https://graph.microsoft.com`, complete against the user's tenant **without** an "approval required" / "admin consent" / "device must be compliant" screen? | A code is captured and exchanged for a Graph access token + refresh token pair; that refresh token also mints an `outlook.office.com` access token via cross-resource refresh. |
| Q2 | Does the Graph access token authorise `GET https://graph.microsoft.com/v1.0/me/messages/delta`? | HTTP 200 with a `value` array and a `@odata.deltaLink` or `@odata.nextLink`. |
| Q3 | Does the IMAP access token authorise XOAUTH2 against `outlook.office365.com:993` for `EXAMINE INBOX`? | `OK` response and an `EXISTS` count. |
| Q4 | Does the refresh token return fresh, working access tokens for both resources on a second exchange? | Q2 and Q3 still pass with the refreshed tokens. |

---

## 3. Constants

```
client_id      = d3590ed6-52b3-4102-aeff-aad2292ab01c   (Outlook desktop, first-party public client)
authority      = https://login.microsoftonline.com/common
authorize_endp = https://login.microsoftonline.com/common/oauth2/authorize       (v1 — classic)
token_endp     = https://login.microsoftonline.com/common/oauth2/token           (v1 — classic)
redirect_uri   = urn:ietf:wg:oauth:2.0:oob                                       (see §1.2)
resource_graph = https://graph.microsoft.com
resource_imap  = https://outlook.office365.com                                   (DavMail Settings.OUTLOOK_URL)
graph_delta    = https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta
imap_host      = outlook.office365.com
imap_port      = 993
```

Three surprises in the constants worth calling out so they aren't
"corrected" later:

- The endpoints are **v1**, not v2: `/oauth2/authorize` and `/oauth2/token`,
  *not* `/oauth2/v2.0/authorize` and `/oauth2/v2.0/token`. This is the
  DavMail default for EWS/Outlook.
- The `redirect_uri` is the **URN form** `urn:ietf:wg:oauth:2.0:oob`,
  *not* `https://login.microsoftonline.com/common/oauth2/nativeclient`.
  DavMail's source defaults to the nativeclient URL, but that is matched
  against DavMail's own registered client_id; the Outlook desktop
  client_id has the URN registered. See §1.2 above. Using the wrong
  redirect URI yields `AADSTS50011 "Reply URL specified in the request
  does not match…"`.
- Graph's `messages: delta` endpoint is **per-folder**, not at the
  `/me/messages` root. Calling `/me/messages/delta` returns HTTP 400
  `"Change tracking is not supported against 'microsoft.graph.message'"`.
  The probe targets `/me/mailFolders/inbox/messages/delta`; production
  uplink will iterate over folders.

Why no PKCE: DavMail does not use PKCE in either its default
(scripted-form) flow or its WebView (`O365InteractiveAuthenticator`)
flow. The v1 endpoint accepts public clients without PKCE provided
they post no `client_secret`. Since the goal of this spike is to do
*exactly* what DavMail does — the configuration we know works against
real corporate tenants today — we follow suit. (Adding PKCE later would
be defence-in-depth, not a correctness change; M5 implementation can
revisit.)

Why no `scope=` parameter: the v1 endpoint uses `resource=` instead,
and "scopes" are inferred from the application registration of
`client_id` for that resource. The Outlook desktop client_id is
pre-authorized for the Mail.*, Calendars.*, Contacts.*, and
IMAP.AccessAsUser.All / SMTP.Send permissions that real Outlook needs.
We get all of those by virtue of being that client_id; we cannot pick
a subset.

Why no `code_verifier`, `client_secret`, `scope` in token exchange:
DavMail sends only `grant_type`, `code`, `redirect_uri`, `client_id`
(see `O365Token.java:60–95`). The v1 endpoint accepts that minimal
form for public clients.

Why `resource=outlook.office365.com` on refresh: at the v1 endpoint a
refresh token issued against one resource can be redeemed against
another resource by re-stating `resource=<target>` on the refresh
POST. DavMail's `O365Token.refreshToken` (lines 236–254) does
exactly this when it needs to switch audiences (e.g. mint an EWS
token after originally signing in for Graph).

---

## 4. How to run

The probe is one Cargo project, two files. On NixOS:

```bash
mkdir -p /tmp/outlook-spike/src && cd /tmp/outlook-spike
# Paste the contents of "Cargo.toml" (§7) into ./Cargo.toml
# Paste the contents of "src/main.rs" (§8) into ./src/main.rs
nix-shell -p cargo rustc pkg-config openssl \
  --run "cargo run --release -- alice@example.com"
```

On a non-Nix system with rustup + OpenSSL dev headers installed, just:

```bash
cargo run --release -- alice@example.com
```

The first run will compile dependencies (~1 minute). Then the probe will
print:

```
=== Q1: authorization code (v1 endpoint, resource=graph.microsoft.com) ===
Open this URL in a browser:

  https://login.microsoftonline.com/common/oauth2/authorize?... (long)

Sign in. AAD's response to the OOB redirect varies — you will see one
of (in order of observed prevalence on corporate tenants):

  (a) Browser stops on a protocol error because the redirect Location
      header is "urn:ietf:wg:oauth:2.0:oob?code=...&session_state=...".
      Open DevTools → Network, find the request that ended in a 302 to
      a urn: scheme, copy the response Location header value.

  (b) Browser lands on a near-blank page at
      https://login.microsoftonline.com/common/oauth2/nativeclient
      whose URL contains "?code=...&session_state=...". Copy the URL.

  (c) A results page with title or body "Success code=...". Copy the
      title or the code itself.

Paste any of:
  - the urn: Location header value (case a)
  - the full landing URL          (case b)
  - the "Success code=..." string (case c)
  - or just the bare code

The probe extracts the code from any of these forms.
>
```

**Record any of these screens — they answer Q1 as a fail:**

- "Need admin approval" / "Admin consent required"
- "This device must be marked as compliant"
- "Your sign-in was successful but does not meet the criteria"
- "You don't have access to this" (this is what the device-code attempt
  hit; if it appears under v1 auth_code too, the tenant block is
  app-id-specific rather than flow-specific)
- Any Conditional Access policy denial

If the landing page appears, paste its URL; the probe parses out the
code, exchanges, and continues with Q2–Q4 automatically.

---

## 5. Expected output (success path)

```
=== Q1: authorization code (v1 endpoint, resource=graph.microsoft.com) ===
Open this URL in a browser:
  https://login.microsoftonline.com/common/oauth2/authorize?...
Paste the landing URL here:
> https://login.microsoftonline.com/common/oauth2/nativeclient?code=0.ATC...&session_state=...
Q1: PASS — Graph access token eyJ0eXAi...XyzQ; refresh token 0.AXc...wQXr

=== Mint IMAP-resource access token (cross-resource refresh) ===
IMAP token: eyJ0eXAi...4kjK

=== Q2: Graph /me/messages/delta ===
Q2: PASS — HTTP 200; deltaLink prefix = https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=...

=== Q3: IMAP XOAUTH2 EXAMINE INBOX ===
Q3: PASS — INBOX EXISTS 12473

=== Q4: refresh and re-test ===
Q4: PASS — refresh works for both resources (graph=200, imap=12473)

All four questions PASS. Fill in the result template (§9) and bring
that single file back to whoever requested the spike.
```

---

## 6. Failure modes and what they mean

| Failure | Likely cause | Implication |
|---|---|---|
| Authorize returns `AADSTS50011 "Reply URL specified in the request does not match…"` | `redirect_uri` parameter does not match what the Outlook client_id has registered | Confirm the probe is sending `urn:ietf:wg:oauth:2.0:oob` (the URN form), not `…/oauth2/nativeclient`. The latter is registered against DavMail's own client_id, not Outlook's. |
| Q1 blocked by "approval required" / "admin consent" screen | Tenant CA enforces app-allowlist by app id, even against first-party Outlook client_id | M365 leg of MVP unavailable on this tenant. Try a personal MSA next to separate protocol-level from policy-level. |
| Q1 blocked by "device not compliant" / WAM-broker-required screen | Tenant requires Intune-managed device or Windows WAM broker | Linux/web cannot satisfy. Structurally blocked. |
| Q1 blocked by "You don't have access to this" with device-code flow only | Tenant CA blocks device-code flow as a phishing mitigation (this v1 auth_code flow is unaffected) | This is why this spike uses v1 auth_code instead of device-code; if the same message appears under v1 auth_code, the block is broader. |
| Authorize returns `AADSTS65001` "consent must be triggered via pre-authorization" | Hitting v2 endpoint or sending `scope=` instead of `resource=` | This is why this spike uses the v1 endpoint with `resource=`. If the v1 path also returns `AADSTS65001`, the tenant has explicitly removed Outlook's first-party pre-authorization for that resource — admin-side block. |
| Code exchange returns `AADSTS50158` or `AADSTS500011` | Resource / audience mismatch | Re-check that the authorize URL used `resource=https://graph.microsoft.com` (no trailing slash, exact spelling) and that the token endpoint is v1 (`/oauth2/token` not `/oauth2/v2.0/token`). |
| Code exchange returns `AADSTS9002313` "invalid request" | Stray `scope=` or `code_verifier=` on the v1 token POST | v1 token endpoint expects only `grant_type`, `code`, `redirect_uri`, `client_id` for the public-client auth_code grant. Drop any extra parameters. |
| Cross-resource refresh returns `AADSTS70011` "scope value is not valid" | Sent `scope=` instead of `resource=` on refresh | v1 refresh switches audiences via `resource=`, not `scope=`. |
| Q2 returns 400 `"Change tracking is not supported against 'microsoft.graph.message'"` | Calling delta on `/me/messages` directly | Graph requires delta on a specific folder. Probe targets `/me/mailFolders/inbox/messages/delta` already. |
| Q2 returns 401 `InvalidAuthenticationToken` | Graph audience not on the token | Decode the JWT (jwt.ms) and confirm `aud` is `https://graph.microsoft.com` or `00000003-0000-0000-c000-000000000000`. If it says `outlook.office365.com`, the wrong refresh leg supplied the Graph call. |
| Q2 returns 403 `ErrorAccessDenied` | App-level deny by tenant policy | Tenant restricts Graph delegated access by client_id even after a successful sign-in. |
| Q3 returns `NO LOGIN failed` or `AUTHENTICATE failed` | IMAP audience not on the token, IMAP disabled at tenant level, or scope missing | Decode JWT at jwt.ms; `aud` must be `https://outlook.office365.com` or `00000002-0000-0ff1-ce00-000000000000`. Confirm refresh used `resource=https://outlook.office365.com`. If audience is correct and IMAP still fails, the tenant has likely disabled IMAP at the org level — record the data point and fall back to Graph-only for M365 in the spec. |
| Q3 returns connect refused / TLS handshake fail | Tenant disabled IMAP/POP at the org level | Admin toggle. M365 mail must go through Graph only — adjust §8.3 of the spec to drop the IMAP path. |
| Q4 refresh returns `invalid_grant` | Refresh token rotation invalidated the previous token, or tenant token-lifetime policy is short | v1 refresh tokens at AAD typically do not rotate per-use the way v2 does, but tenant policy can override. Re-run with rotation-aware persistence (probe handles this by holding the most recent refresh token, but if Q4 fails specifically because of rotation, document it — uplink will need rotation-aware persistence by M5). |

---

## 7. Embedded artefact — `Cargo.toml`

Save to `./Cargo.toml`:

```toml
[package]
name    = "outlook-spike"
version = "0.0.0"
edition = "2021"

[dependencies]
anyhow      = "1"
imap        = "2.4"
native-tls  = "0.2"
reqwest     = { version = "0.12", features = ["blocking", "json", "rustls-tls"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
url         = "2"

[profile.release]
debug = true   # we want backtraces in case of crash; this is a probe

[workspace]
```

---

## 8. Embedded artefact — `src/main.rs`

Save to `./src/main.rs`:

```rust
//! Outlook desktop client_id impersonation probe — v1 endpoint,
//! authorization code, resource= parameter, no PKCE.
//!
//! Usage: `cargo run --release -- <email>`
//!
//! This is a faithful port of the DavMail wire shape (see
//! `O365Authenticator.buildAuthorizeUrl` / `O365Token.build` /
//! `O365Token.refreshToken`) — minimum deviations only:
//!
//!  - User opens the URL in their own browser rather than an embedded
//!    WebView. DavMail's `O365InteractiveAuthenticator` does the
//!    WebView; we cannot (we are headless), so the user pastes the
//!    landing artefact back. DavMail extracts the `code` from the
//!    landing URL at `O365Authenticator.java:266`; we accept either a
//!    full URL or a bare code (the OOB redirect URI sometimes lands
//!    on a results page rather than redirecting).
//!  - `redirect_uri` is the OOB URN that the Outlook desktop client_id
//!    has registered, not the `nativeclient` URL that DavMail defaults
//!    to (which is registered against DavMail's own client_id). See
//!    §1.2 of the spike doc.
//!
//! Otherwise: identical parameters, identical endpoints (v1, not v2),
//! identical refresh shape with `resource=` for cross-resource minting.
//!
//! This is a throwaway probe — do not link it into the main project.

use std::{
    io::{self, BufRead, Write},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client as HttpClient;
use serde::Deserialize;

const CLIENT_ID: &str = "d3590ed6-52b3-4102-aeff-aad2292ab01c";
const AUTHORITY: &str = "https://login.microsoftonline.com/common";

// The Outlook desktop client_id has `urn:ietf:wg:oauth:2.0:oob`
// registered as its native-client redirect URI. The `…/oauth2/nativeclient`
// URL is *not* registered against this client_id — using it produces
// AADSTS50011 "Reply URL does not match". See §1.2 of the spike doc.
const REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

// v1 endpoints — NOT /oauth2/v2.0/...
const AUTHORIZE_ENDPOINT: &str =
    "https://login.microsoftonline.com/common/oauth2/authorize";
const TOKEN_ENDPOINT: &str =
    "https://login.microsoftonline.com/common/oauth2/token";

// AAD "resource" identifiers — for Outlook/EWS/IMAP, DavMail uses
// Settings.OUTLOOK_URL = "https://outlook.office365.com" (with the 365).
const RESOURCE_GRAPH: &str = "https://graph.microsoft.com";
const RESOURCE_IMAP:  &str = "https://outlook.office365.com";

// `messages: delta` is per-folder on Graph. Calling /me/messages/delta
// directly returns BadRequest "Change tracking is not supported against
// 'microsoft.graph.message'". Use a well-known folder name (`inbox`).
const GRAPH_DELTA: &str =
    "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$top=10";

const IMAP_HOST: &str = "outlook.office365.com";
const IMAP_PORT: u16 = 993;

#[derive(Deserialize, Debug)]
struct TokenResp {
    access_token: String,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<serde_json::Value>,  // v1 returns expires_in as string, v2 as number
    #[allow(dead_code)]
    resource: Option<String>,
    #[allow(dead_code)]
    scope: Option<String>,
}

fn http() -> Result<HttpClient> {
    Ok(HttpClient::builder()
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn authorize_url(email: &str) -> Result<String> {
    // Exact parameter set matches O365Authenticator.buildAuthorizeUrl
    // (lines 65–106) for the EWS / default path, with `resource=graph` for
    // our Graph-first scenario. No `state=`: the OOB redirect URI sometimes
    // lands on a results page rather than a query-string redirect, and AAD
    // does not always echo `state` in that mode. CSRF is moot here because
    // the user is doing a one-shot manual paste, not driving a session.
    let url = url::Url::parse_with_params(
        AUTHORIZE_ENDPOINT,
        &[
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("response_mode", "query"),
            ("login_hint", email),
            ("resource", RESOURCE_GRAPH),
        ],
    )?;
    Ok(url.into())
}

fn prompt_for_url() -> Result<String> {
    print!("> ");
    io::stdout().flush()?;
    let mut line = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut line)?;
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("empty input");
    }
    Ok(line)
}

/// Accept any of:
///
///   - a `urn:` Location header value (e.g.
///     `urn:ietf:wg:oauth:2.0:oob?code=…&session_state=…` — what the
///     browser's DevTools shows when AAD redirects to the OOB URN and
///     the browser refuses to follow a non-HTTP scheme),
///   - a full landing URL (`https://…/nativeclient?code=…&session_state=…`),
///   - a `Success code=<code>` string (the page title AAD shows for
///     OOB in some configurations),
///   - or a bare code (the long opaque token, usually starting `0.A…`).
///
/// Strategy: try url::Url::parse first to pick up structured queries
/// (works for both https: and urn: URIs); if that yields no `code`,
/// fall back to a substring scan for `code=`; finally accept a bare
/// token. We don't enforce CSRF state — see comment on `authorize_url`.
fn parse_code(input: &str) -> Result<String> {
    let trimmed = input.trim();

    // Case 1: full URL with ?code=...
    if let Ok(parsed) = url::Url::parse(trimmed) {
        let mut code = None;
        let mut err_kind = None;
        let mut err_desc = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "error" => err_kind = Some(v.into_owned()),
                "error_description" => err_desc = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some(e) = err_kind {
            bail!(
                "authorize endpoint returned error: {e} — {}",
                err_desc.unwrap_or_default()
            );
        }
        if let Some(c) = code {
            return Ok(c);
        }
        // Parsed as URL but no code — fall through to text heuristics
        // (e.g. user pasted the OOB results-page URL with no query).
    }

    // Case 2: contains "code=" (e.g. "Success code=ABC..." or a query
    // fragment the user copied without the host).
    if let Some(idx) = trimmed.find("code=") {
        let after = &trimmed[idx + "code=".len()..];
        // Code runs until whitespace, '&', or end of string.
        let code: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '&')
            .collect();
        if !code.is_empty() {
            return Ok(code);
        }
    }

    // Case 3: bare code. AAD codes are opaque but long (>= 32 chars) and
    // contain no whitespace. If it looks plausible, accept it.
    if trimmed.len() >= 16 && !trimmed.chars().any(|c| c.is_whitespace()) {
        return Ok(trimmed.to_string());
    }

    bail!(
        "could not extract an authorization code from pasted input: {input}"
    )
}

fn exchange_code(c: &HttpClient, code: &str) -> Result<TokenResp> {
    // Exact parameter set matches O365Token.java:60–95 — no scope, no
    // resource, no PKCE verifier. v1 token endpoint accepts this minimal
    // form for public clients.
    let resp = c
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
        ])
        .send()?;
    if !resp.status().is_success() {
        let body = resp.text()?;
        bail!("code exchange: {body}");
    }
    Ok(resp.json()?)
}

fn refresh(c: &HttpClient, refresh_token: &str, resource: &str)
    -> Result<TokenResp>
{
    // Exact parameter set matches O365Token.refreshToken (lines 236–260):
    // grant_type, refresh_token, redirect_uri, client_id, resource.
    let resp = c
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("resource", resource),
        ])
        .send()?;
    if !resp.status().is_success() {
        let body = resp.text()?;
        bail!("refresh HTTP error: {body}");
    }
    Ok(resp.json()?)
}

fn graph_delta(c: &HttpClient, token: &str) -> Result<(u16, String)> {
    let resp = c.get(GRAPH_DELTA).bearer_auth(token).send()?;
    let status = resp.status().as_u16();
    let body = resp.text()?;
    Ok((status, body))
}

struct XOAuth2 {
    user: String,
    token: String,
}

impl imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> String {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

fn imap_examine(email: &str, token: &str) -> Result<u32> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .context("build TLS connector")?;
    let client = imap::connect((IMAP_HOST, IMAP_PORT), IMAP_HOST, &tls)
        .context("IMAP TLS connect")?;
    let auth = XOAuth2 {
        user: email.to_owned(),
        token: token.to_owned(),
    };
    let mut session = client
        .authenticate("XOAUTH2", &auth)
        .map_err(|(e, _)| anyhow!("XOAUTH2 failed: {e}"))?;
    let mailbox = session.examine("INBOX")?;
    let _ = session.logout();
    Ok(mailbox.exists)
}

fn redact(t: &str) -> String {
    if t.len() < 16 {
        return "***".into();
    }
    format!("{}...{}", &t[..8], &t[t.len() - 4..])
}

fn extract_delta_prefix(body: &str) -> String {
    if let Some(idx) = body.find("\"@odata.deltaLink\":\"") {
        let start = idx + "\"@odata.deltaLink\":\"".len();
        let rest = &body[start..];
        if let Some(end) = rest.find('"') {
            let s = &rest[..end];
            return s.chars().take(80).collect::<String>() + "...";
        }
    }
    if let Some(idx) = body.find("\"@odata.nextLink\":\"") {
        let start = idx + "\"@odata.nextLink\":\"".len();
        let rest = &body[start..];
        if let Some(end) = rest.find('"') {
            let s = &rest[..end];
            return format!(
                "nextLink (no deltaLink yet): {}...",
                s.chars().take(80).collect::<String>()
            );
        }
    }
    "(no @odata link found in body)".into()
}

fn main() -> Result<()> {
    let email = std::env::args()
        .nth(1)
        .context("usage: outlook-spike <email>")?;

    let c = http()?;

    println!("=== Q1: authorization code (v1 endpoint, resource=graph.microsoft.com) ===");
    let url = authorize_url(&email)?;
    println!("Open this URL in a browser:\n");
    println!("  {url}\n");
    println!(
        "Sign in. AAD's OOB response varies — you will see one of:\n\n  \
         (a) Browser protocol error; the response Location header is\n      \
             \"urn:ietf:wg:oauth:2.0:oob?code=...&session_state=...\".\n      \
             Open DevTools → Network and copy the Location header value.\n  \
         (b) https://login.microsoftonline.com/common/oauth2/nativeclient?code=...\n  \
         (c) A page titled or showing \"Success code=...\"\n\n\
         Paste any of:\n  \
         - the urn: Location header value (case a)\n  \
         - the full landing URL          (case b)\n  \
         - the \"Success code=...\" string (case c)\n  \
         - just the bare code"
    );
    let pasted = prompt_for_url()?;
    let code = parse_code(&pasted)?;

    // Initial code exchange returns a Graph access token (since the
    // authorize step used resource=graph.microsoft.com) plus a refresh
    // token. The refresh token can be redeemed against another resource
    // by re-stating resource=<target> on the refresh POST (v1 behaviour).
    let initial = exchange_code(&c, &code)?;
    let refresh_tok = initial.refresh_token.clone().ok_or_else(|| {
        anyhow!(
            "Q1 PARTIAL — got access token but no refresh token; \
             DavMail expects a refresh token from this flow — \
             tenant may have restricted offline access"
        )
    })?;
    println!(
        "Q1: PASS — Graph access token {}; refresh token {}",
        redact(&initial.access_token),
        redact(&refresh_tok),
    );

    println!("\n=== Mint IMAP-resource access token (cross-resource refresh) ===");
    let imap_t = refresh(&c, &refresh_tok, RESOURCE_IMAP)?;
    println!("IMAP token: {}", redact(&imap_t.access_token));

    println!("\n=== Q2: Graph /me/messages/delta ===");
    let (status, body) = graph_delta(&c, &initial.access_token)?;
    let q2_pass = status == 200;
    if q2_pass {
        println!("Q2: PASS — HTTP 200; {}", extract_delta_prefix(&body));
    } else {
        println!(
            "Q2: FAIL — HTTP {}: {}",
            status,
            body.chars().take(400).collect::<String>(),
        );
    }

    println!("\n=== Q3: IMAP XOAUTH2 EXAMINE INBOX ===");
    let q3 = imap_examine(&email, &imap_t.access_token);
    let q3_pass = q3.is_ok();
    match &q3 {
        Ok(n) => println!("Q3: PASS — INBOX EXISTS {n}"),
        Err(e) => println!("Q3: FAIL — {e}"),
    }

    println!("\n=== Q4: refresh and re-test ===");
    let graph2 = refresh(&c, &refresh_tok, RESOURCE_GRAPH)?;
    let imap2 = refresh(&c, &refresh_tok, RESOURCE_IMAP)?;
    let (s2, _) = graph_delta(&c, &graph2.access_token)?;
    let q4_graph_pass = s2 == 200;
    let q4_imap = imap_examine(&email, &imap2.access_token);
    let q4_imap_pass = q4_imap.is_ok();
    if q4_graph_pass && q4_imap_pass {
        println!(
            "Q4: PASS — refresh works for both resources (graph={}, imap={})",
            s2,
            q4_imap.as_ref().unwrap(),
        );
    } else {
        println!(
            "Q4: FAIL — graph={} ({}), imap={} ({:?})",
            q4_graph_pass, s2, q4_imap_pass, q4_imap.err(),
        );
    }

    let all_pass = q2_pass && q3_pass && q4_graph_pass && q4_imap_pass;
    println!(
        "\nAll four questions {}. Fill in the result template in §9 \
         of the spike document and return the resulting standalone \
         file to whoever requested the spike.",
        if all_pass { "PASS" } else { "had failures — see above" },
    );
    Ok(())
}
```

---

## 9. Embedded artefact — result template

After running, fill in the template below and save it as a **single
standalone Markdown file** (e.g. `outlook-impersonation-result.md`
anywhere convenient on the spike machine). The probe project, the
spike spec, and the result file are all unrelated to any repo on the
spike machine — bring just the standalone result file back to
whoever requested the spike. The probe code is throwaway and should
not be checked in anywhere.

```markdown
# Spike result — M365 Outlook desktop client_id impersonation

**Run date:** <YYYY-MM-DD HH:MM TZ>
**Tenant domain:** <example.com — mask if sensitive; or "personal MSA">
**Probe machine OS:** <Linux / macOS / Windows + version>
**Outcome:** <ALL PASS | PARTIAL — Qx FAIL | BLOCKED at Q1>

## Q1 — authorization code (v1 endpoint, `resource=graph.microsoft.com`)

- Outcome: <PASS | FAIL>
- Browser screens encountered between "open URL" and the nativeclient
  landing page:
  - <e.g. "Sign in with work account">
  - <"Stay signed in?" — no consent screen>
  - <any MFA prompts>
- Landing URL prefix: <https://login.microsoftonline.com/common/oauth2/nativeclient?code=0.ATC...>
- Initial tokens received: Graph access_token (redacted prefix),
  refresh_token (redacted prefix).
- Cross-resource IMAP refresh (`resource=https://outlook.office365.com`):
  <PASS | FAIL — error code if fail>.
- Notable: <unusual delays? prompt to install Authenticator? approval
  required screens? IT-managed-device requirement? AADSTSxxxxx error
  codes seen?>

## Q2 — Graph /me/messages/delta

- Outcome: <PASS | FAIL>
- HTTP status: <200 / 401 / 403 / ...>
- If pass: deltaLink prefix `<https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=...>`.
- If fail: response body (PII removed): <...>

## Q3 — IMAP XOAUTH2 EXAMINE INBOX

- Outcome: <PASS | FAIL>
- If pass: EXISTS count = <N>.
- If fail: server response or error: <...>

## Q4 — refresh and re-test

- Outcome: <PASS | FAIL>
- New tokens minted: yes/no.
- Q2 still passes with new graph token: yes/no.
- Q3 still passes with new IMAP token: yes/no.
- Refresh-token rotation observed: <yes — token changed on refresh / no
  — same token returned>. (DavMail's v1 path typically does NOT rotate;
  worth recording either way for M5 persistence design.)

## Unexpected interactions

<anything the auth flow did that wasn't on the script — extra prompts,
browser warnings, "your IT department needs to approve",
device-binding requests, app installation prompts, etc.>

## Recommended next action

<one of:>

- All pass → proceed with §8.3, §10.1, §13.3 of the spec as written;
  M5 unblocked.
- Q1 blocked at consent / approval screen → record the screen, attempt
  against a personal MSA to separate policy-level from protocol-level;
  if personal MSA passes, document the tenant restriction in the
  uplink roadmap and accept that M5 may not work on this specific
  tenant.
- Q3 fails specifically (IMAP disabled at org level) → update §8.3 of
  the spec to drop the IMAP path for M365 and route everything through
  Graph.
- Some other partial failure → patch to §13.3 wording: <...>

## Token capture (redacted) for §13.3 reference

- Graph token `aud` claim: <https://graph.microsoft.com or 00000003-0000-0000-c000-000000000000>
- IMAP token `aud` claim: <https://outlook.office365.com or 00000002-0000-0ff1-ce00-000000000000>
- Refresh token rotation observed: <yes / no — see above>
- Access token TTL: <e.g. 3599 s>
```

---

## 10. Out of scope for this spike

- Performance (latency, throttle behaviour).
- Long-lived refresh-token rotation over days/weeks.
- Calendar `/me/calendarView/delta` — covered separately at M8 of the
  main plan.
- Multi-tenant or guest accounts.
- SMTP. Sending in MVP is via Graph `sendMail`, not SMTP.
- Embedded WebView (DavMail-style code interception); the manual
  paste-URL UX is the headless equivalent and adequate for a one-shot
  probe.
- PKCE. DavMail does not use it in either of its working flows;
  adding it would be defence-in-depth, not a correctness change.
  Revisit at M5 once the v1 flow is confirmed working.

---

*End of spike document.*
