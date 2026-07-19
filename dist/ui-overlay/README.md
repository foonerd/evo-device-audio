# UI overlay files served by the reference distribution

Files in this directory are installed to `/opt/evo/ui/current/`
alongside the operator-app shell (evo-ui-eng build output) by the
distribution's install script.

Currently shipped:

- **`setup.html`** — first-boot setup surface. Handles the
  headless bootstrap-preseed pair ceremony and the first-run
  password-set flow, then hands off to the operator shell at `/`.
  Referenced from the acceptance walk for the headless WiFi and
  headless Ethernet device classes; the screened-Pi class does
  not need `setup.html` because the kiosk shell mints its own
  bearer via `mint_local_kiosk_session` on the Unix socket.

The overlay files use only:

- Inline CSS + JavaScript (no external dependencies).
- WebSocket to `/api/v1/ws` at the same origin as the served
  page — the distribution's port-80 UI runtime proxies to the
  framework's HTTPS listener at `:8443`.
- Cookies for bearer persistence (`evo_bearer=<token>`; Path=/,
  Secure, SameSite=Strict).

The overlay is distribution-scoped, not framework-scoped: another
distribution shipping the same framework may replace or extend
`setup.html` with its own consumer-onboarding flow without
touching the framework primitives (`pair_complete`,
`set_kiosk_password`) that back it.
