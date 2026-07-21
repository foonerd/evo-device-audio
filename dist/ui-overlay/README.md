# UI overlay files served by the reference distribution

Files in this directory are installed to the framework's HTTPS
static-asset root (`/opt/evo/ui/current/`) by the distribution's
install script.

## Status

### `setup.html` — **PROTOTYPE. NOT VERIFIED IN A BROWSER.**

Single-file inline CSS + JS demonstration of the bootstrap-preseed
pair + first-run set-password flow. The wire calls it makes match
the framework's live wire surface, but the browser-side page
itself has never completed a pairing end-to-end.

Known limitations that make this UNSUITABLE as the canonical
reference:

- Must be served from `https://<player>:8443/` — the framework's
  HTTPS listener. Serving via a port-80 nginx front breaks the
  `wss://` upgrade (defaults to :443, closed) and the browser
  drops the `Secure` cookie the pair step sets. The current
  build points the WSS URL at `:8443` explicitly, but the static
  page still needs the framework's HTTPS root to host it.
- Self-signed cert on the framework's HTTPS listener triggers a
  browser warning on first hit. A real distribution ships a
  per-device cert chain (out of scope for this overlay).

## Canonical reference for wire-op integration

The canonical reference for wire shape, frame envelope, error
subclasses, and cookie / step-up token handling is the framework
wire surface itself, exercised through either transport:

- Unix-socket transport (kiosk shell / evo-plugin-tool class).
- WSS transport (browser-equivalent). Every op is wrapped in the
  framework's `{frame_type: "request", request_id, op, payload}`
  envelope; the bearer is negotiated via
  `Sec-WebSocket-Protocol: evo.bearer.<encoded-token>`.

The framework's own integration tests and the wire-op reference
documentation are the ground truth for what the framework returns
and what subclass strings surface for each refusal.

## The framework primitives back both paths

Whether an operator app reaches the framework through a browser
(WSS via `Sec-WebSocket-Protocol` bearer) or through the kiosk
shell (Unix socket via `mint_local_kiosk_session`), the framework
side is identical. This overlay is a demonstration of one path;
the framework wire surface is the reference.

## The overlay is distribution-scoped

Another distribution shipping the same framework may replace or
extend `setup.html` with its own consumer-onboarding flow without
touching the framework primitives (`pair_complete`,
`set_kiosk_password`, `step_up_auth_verify`) that back it. The
overlay layer is a distribution choice; the wire surface is
framework contract.
