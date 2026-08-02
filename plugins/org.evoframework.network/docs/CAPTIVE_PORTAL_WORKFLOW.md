# Captive portal workflow

The reference architecture for captive-portal admission is a
**device-proxied session**. Portals authorise the device MAC on
the captive-carrying interface (wlan0); a remote operator on
the management LAN is on a different network segment and can
never reach the venue directly. The device is the HTTP client
to the portal; the framework serves a same-origin session URL
on the management HTTPS plane; the UI iframes that URL. The
operator's browser never touches the venue portal.

This document is the contract between plugin, framework, and
UI for that flow.

## Reference implementation

- Plugin: `org.evoframework.network` — allocates session ids,
  keeps a per-session cookie jar, dispatches upstream fetches
  through the wlan0-bound wrapper.
- Wrapper: `dist/bin/evo-captive-probe` — `proxy-fetch` mode
  runs curl with `SO_BINDTODEVICE` set for wlan0 (via a narrow
  root-elevated sudoers grant); response headers + body stream
  to stdout in raw HTTP wire format.
- Framework: `evo-runtime-http/src/captive_session_endpoint.rs`
  — mounts `/api/v1/network/captive/session/{sid}[/*path]`,
  dispatches every browser request through the plugin's
  `network.nm.captive.upstream.fetch` verb, rewrites `Location`
  + absolute URLs, strips `Set-Cookie` + hop-by-hop headers.
- Contract: `PromptType::ExternalRedirect` +
  `OpenMechanism::DeviceProxiedSession` — plugin signals a
  device-proxied flow; UI iframes the returned `session_url`.
- Widget: `evo.network.captive_portal_handoff` — see
  `docs/engineering/WIFI-OPERATOR-WIDGETS.md`.

## Verbs

Read-only detection:

- `network.nm.captive.status` — is a portal currently detected?
  Returns `{ phase, is_captive, portal_url, last_probe_url,
  last_http_code, capport_api_uri? }`. Anonymous-OK read.

Device-proxied session (three verbs, all
`write:network_admin`):

- `network.nm.captive.session.start` — opens a session against
  the currently detected portal. Returns `{ session_id,
  session_url, upstream_host, initial_path, initial_query,
  expires_at_epoch }`. `session_url` is the same-origin URL the
  UI iframes. Refuses (permanent error) when no portal is
  detected — call `captive.status` first.
- `network.nm.captive.upstream.fetch` — called by the framework
  proxy on every browser request. Payload names `session_id`,
  HTTP method, path, query, request headers, request body_b64.
  Plugin resolves `upstream_host + path?query`, adds the jar's
  current `Cookie` header, invokes the wrapper's `proxy-fetch`
  mode (wlan0-bound), parses the response, updates the jar from
  any `Set-Cookie`. Returns `{ http_status, http_status_text,
  headers, body_b64, upstream_host, session_id }`. Not called
  directly by the UI — the framework's endpoint is the sole
  caller.
- `network.nm.captive.session.close` — explicit close.
  Drops the jar, re-runs `captive_detect`, returns the fresh
  reachability verdict + captive state so the UI flips its
  state without a follow-up status poll.

## Session lifecycle

1. Plugin's `captive_detect` reports `is_captive: true` +
   `portal_url` (either from the RFC 8910 lease-carried capport
   URI or the legacy redirect probe). UI widget shows the
   "sign-in required" banner.
2. Operator taps "Open sign-in". UI dispatches
   `network.nm.captive.session.start` over the framework
   WebSocket. Plugin allocates a `session_id` (128-bit, hex),
   records `upstream_host` + `initial_path` + `initial_query`,
   initialises an empty jar, returns `{ session_id,
   session_url, ... }`. Session is persisted in
   `captive-sessions.json` (LKG-shadow-mirrored, same
   discipline as `captive-session.json`).
3. UI renders `<iframe src="{session_url}">`.
4. Iframe navigates to `/api/v1/network/captive/session/{sid}
   {initial_path}?{initial_query}` — same-origin. Framework's
   `attach_captive_session_endpoint` handles the GET.
5. Framework filters caller headers (drops hop-by-hop + Host),
   composes an `inner_payload` naming session_id + method + path
   + query + headers + body_b64, base64-encodes it, envelopes
   under `{shelf: "networking.link", request_type:
   "network.nm.captive.upstream.fetch", payload_b64}`, and
   dispatches through the framework's `Dispatcher`.
6. Plugin resolves upstream URL, adds Cookie from jar,
   dispatches wrapper `proxy-fetch` mode, parses response,
   updates jar, returns `{ http_status, headers, body_b64,
   upstream_host, session_id }`.
7. Framework post-processes:
   - Drops hop-by-hop response headers.
   - Drops `Set-Cookie` — plugin owns the jar; the browser
     has no cookies to send back (it is not the party the
     portal is tracking).
   - Rewrites `Location` two ways: absolute URL whose
     authority matches `upstream_host` gets stripped +
     prepended with the session prefix; a path-absolute
     value (`/portal-path/…`, NOT `//…`) also gets prepended
     with the session prefix. Protocol-relative and foreign
     hosts pass through untouched.
   - For `text/html` / `text/css` / `text/plain` /
     `application/xhtml` responses, runs two passes: (a)
     byte-substitute every occurrence of `upstream_host`
     with the session prefix; (b) prepend the session prefix
     to every path-absolute URL in a URL-carrying context
     (HTML `href`/`src`/`action`/etc., CSS `url()`).
     Relative URLs (`./static/js/main.js`) resolve against
     the browser's current location — the session URL — so
     the common case still needs no rewrite.
   - Deliberately does NOT rewrite `application/javascript`,
     `text/javascript`, or `application/json` bodies — a
     byte-scan cannot AST-distinguish `/foo` (URL) from
     `/foo/` (regex) from `+"/pending"` (Redux action
     suffix) from `x /= 2` (division). Modern SPAs derive
     request URLs from `window.location.pathname` and ride
     the session prefix naturally.
   - Decodes upstream `Content-Encoding` (`gzip`,
     `x-gzip`, `deflate` with zlib wrapper or raw, magic
     sniff `1f 8b` when the header is absent) to identity
     BEFORE rewrite or handoff. `br` (brotli) and unknown
     encodings fail closed with a 502 rather than pass
     compressed bytes through. Never forwards the browser's
     `Accept-Encoding` upstream, and always emits identity
     to the browser (Content-Encoding stripped,
     Content-Length recomputed).
8. Browser receives same-origin bytes; portal HTML/JS/CSS
   renders. JS in the browser resolves fetches relative to
   `session_url` — every XHR / `fetch` / navigation the SPA
   makes routes back through the proxy naturally; the cookie
   jar persists on the plugin side across those hops.
9. Portal admits the device MAC (wlan0 associated). Portal
   responds with a completion redirect (302 to venue root or
   an operator-visible "you're online" page) OR the UI
   operator taps "Done" once the portal shows admission.
10. UI dispatches `network.nm.captive.session.close`. Plugin
    drops the session, re-runs `captive_detect`. When the
    portal has admitted the device, the re-probe returns
    `is_captive: false / connectivity: full`. UI transitions to
    the "completed" state.

## What the plugin owns

- Session id generation (128-bit `getrandom` → 32-char
  lowercase hex).
- Per-session cookie jar (`name=value` map; cookie attributes
  are intentionally dropped — jars are per-session, per-portal,
  live only for the session's TTL).
- All upstream HTTP — every fetch is wlan0-bound through
  `evo-captive-probe proxy-fetch`.
- Persisted session state (`captive-sessions.json` +
  `captive-sessions.lkg.json`), so a plugin or steward restart
  mid-authentication does not orphan an open operator iframe.
- TTL enforcement (default 30 minutes; expired sessions pruned
  on load).

## What the framework owns

- The same-origin operator-facing HTTPS route.
- Capability gating (`write:network_admin`) — the paired-
  operator connect scope; matches
  `network.nm.captive.submit` and the plugin's session verbs
  so the bearer chain carries end-to-end.
- Header filtering (hop-by-hop drop, Set-Cookie strip,
  Location absolute-upstream + path-absolute rewrite).
- Content-Encoding decode (gzip / deflate / magic-sniff;
  brotli fail-closed) + Accept-Encoding strip upstream.
- Body byte-substitution + path-absolute prefix in URL
  contexts for HTML / CSS / plain-text / xhtml content
  types. JS / JSON are deliberately excluded.

## What the UI owns

- Detecting `phase == probe_detected` on
  `network.nm.captive.status` and rendering the banner + CTA.
- Dispatching `session.start` and iframing the returned
  `session_url`. **Never** `window.open(portal_url)` or
  `<iframe src={venue_url}>`.
- Detecting completion (iframe navigation away, or an
  operator "Done" tap) and dispatching `session.close`.

## Reliability controls

Session TTL default 30 minutes. `retry_budget` /
`credential_policy` / `replay_window_sec` on the older
form-scrape submit path (`network.nm.captive.submit`) still
apply — that path remains for portals with simple static forms
where a full byte proxy is overkill, but the device-proxied
session is the primary path.

Recovery scenarios:

- **Network drop (short outage):** session state survives on
  disk; on plugin restart the framework proxy can continue
  routing through the same `session_id`.
- **Power failure / reboot:** session survives via LKG shadow;
  if the operator does not resume before TTL expires, session
  is pruned on next load and the UI transitions back to
  `detected` for a re-open.
- **Portal remembers device (MAC/session):** operator opens
  session, portal admits without a UI submit round-trip; the
  first upstream fetch's response includes the "admitted"
  page.
- **Single-use ticket:** the older
  `network.nm.captive.submit` reliability controls still
  govern the form-scrape path; device-proxied sessions do not
  replay operator input on their own.

## Scenario checklist

Every scenario is a rig property, not a CI green.

- Open network + JS-SPA captive portal (common controllers,
  Meraki, similar) — iframe renders portal, operator completes
  voucher, connectivity flips to `full`.
- Open network + static-HTML captive portal (cafe / hotel) —
  either the device-proxied session OR the legacy form-scrape
  path admits.
- WPA2 network + captive web auth — plugin `intent.apply`
  associates, portal detection fires, session flow works.
- Multi-step portal with cookies — jar persists across
  requests; framework does not need to re-open.
- Redirect chain ending in success — Location rewriting keeps
  each hop inside the same-origin session prefix.
- Reboot mid-login — session survives via LKG; iframe reload
  resumes at last portal page (or re-detects when TTL
  expired).
