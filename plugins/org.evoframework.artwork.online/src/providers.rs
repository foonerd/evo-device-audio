// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Online artwork providers.
//!
//! Each provider implements a thin `fetch` async function that
//! takes (artist, album, http client, config) and returns either
//! image bytes on hit or a structured no-result on miss. The
//! cascade walker invokes providers in priority order; the first
//! provider returning Ok-with-bytes wins.
//!
//! ## Provider summary
//!
//! - [`cover_art_archive`] — Cover Art Archive (MusicBrainz).
//!   Free, requires only a properly-identifying User-Agent per
//!   MusicBrainz TOS. Two-step query: search MusicBrainz for
//!   matching release MBIDs, then fetch the front cover from
//!   `coverartarchive.org/release/{mbid}/front`.
//! - [`lastfm`] — Last.fm `album.getinfo`. Requires an operator-
//!   supplied API key. Image URL embedded in the response.
//! - [`itunes`] — Apple iTunes Search API. No key required.
//!   Returns 100x100 thumbnail URLs that we rewrite to 600x600
//!   (a documented URL-pattern trick).
//! - [`volumio_meta`] — Volumio's hosted meta proxy
//!   (`meta.volumio.org`). No key; takes a `variant` parameter
//!   (`community` vs `commercial`).
//!
//! Each provider returns a [`ProviderOutcome`] classifying the
//! result as Hit / Miss / Unavailable. Only a clean Miss (the
//! provider's canonical "no such release" answer) counts toward
//! a cascade-level `GenuinelyEmpty` outcome; any Unavailable
//! (network error, HTTP 5xx, 429, timeout, transcode failure)
//! propagates to the cascade as [`CascadeResult::Unavailable`]
//! so upstream callers can distinguish "definitively absent" —
//! cacheable — from "we could not reach the provider right now"
//! — never cacheable.
//!
//! This distinction is load-bearing: framework-side negative
//! memoisation must not remember a transient upstream failure
//! as if it were a permanent absence, or an operator who adds
//! a cover waits until the memo expires.

use reqwest::{Client, StatusCode};

use crate::config::PluginConfig;

/// Per-provider outcome after one fetch attempt.
///
/// `Miss` means the provider answered cleanly with "no such
/// release" (empty search result set, 404 on canonical
/// endpoint). `Unavailable` means we could not obtain that
/// answer — transport error, rate-limit, 5xx, timeout,
/// unsupported provider protocol behaviour.
pub(crate) enum ProviderOutcome {
    /// Provider returned artwork bytes.
    Hit(ProviderHit),
    /// Provider was reachable and answered cleanly with no
    /// artwork for the requested target.
    Miss,
    /// Provider was NOT reachable / returned a transient
    /// failure. Carries a short human-readable reason for
    /// telemetry + wire-response `detail`.
    Unavailable(String),
}

/// Aggregate outcome across the full provider cascade.
///
/// `GenuinelyEmpty` requires that EVERY attempted (enabled +
/// configured) provider returned [`ProviderOutcome::Miss`].
/// If any provider returned [`ProviderOutcome::Unavailable`]
/// and no provider Hit, the aggregate is `Unavailable` — the
/// caller must not cache this outcome negatively.
pub(crate) enum CascadeResult {
    /// The first provider to succeed.
    Hit(ProviderHit),
    /// Every enabled provider was reached and each answered
    /// with a clean Miss. Safe to cache as not_found.
    GenuinelyEmpty,
    /// At least one provider was Unavailable and no provider
    /// Hit. The aggregate detail lists which providers were
    /// unavailable and why.
    Unavailable(String),
}

/// Classify a reqwest error as Unavailable — every transport
/// / decode / IO error is transient by construction.
fn classify_reqwest_error(
    provider: &str,
    err: &anyhow::Error,
) -> ProviderOutcome {
    ProviderOutcome::Unavailable(format!("{provider}: {err}"))
}

/// Classify an HTTP response status.
///
/// 2xx → success (caller consumes body).
/// 404 / 410 → clean Miss (release is definitively absent from
///   this provider's catalogue).
/// 401 / 403 → Unavailable (auth/permission issue; not a
///   catalogue answer — the operator would fix it).
/// 429 / 5xx / 408 → Unavailable (rate-limit or upstream
///   fault).
/// Other 4xx → Unavailable (request-shape defect at the
///   provider; treat conservatively as transient rather than
///   burning a false negative).
pub(crate) enum StatusClass {
    Success,
    Miss,
    Unavailable(String),
}

pub(crate) fn classify_status(
    provider: &str,
    status: StatusCode,
) -> StatusClass {
    if status.is_success() {
        StatusClass::Success
    } else if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        StatusClass::Miss
    } else {
        StatusClass::Unavailable(format!(
            "{provider}: HTTP {}",
            status.as_u16()
        ))
    }
}

// Reuse the shared distribution client factory. artwork.online +
// metadata.online share connection pool + DNS cache + TLS
// posture. Local `build_http_client` re-exported so existing
// call sites in this crate compile without churn — but the
// implementation is the shared crate's.
pub(crate) use evo_online_providers::build_http_client;

/// One successful provider hit.
pub(crate) struct ProviderHit {
    /// Image bytes (JPEG / PNG / WebP, per the upstream
    /// provider's encoding).
    pub(crate) bytes: Vec<u8>,
    /// MIME type when the upstream provided it; `None` when the
    /// caller should sniff from leading bytes.
    pub(crate) mime: Option<String>,
    /// Stable provider identifier for telemetry + diagnostics.
    /// Round-trips on the wire-shape response so the operator UI
    /// can show "from MusicBrainz" / "from iTunes" etc.
    pub(crate) provider_id: &'static str,
}

/// Cascade walker.
///
/// # Shape
///
/// - **Input sanitisation**: `artist` and `album` are
///   laundered through
///   [`evo_device_audio_shared::artist_name::artist_display_form`]
///   and
///   [`evo_device_audio_shared::album_name::clean_album_title`]
///   before any provider is queried. Raw MPD tag drift
///   (`(Disc 1)` / `[Explicit]` / `(Remastered Version)` on
///   the album; watermarks and sort-form drift on the
///   artist) would otherwise miss every provider's
///   catalogue on well-known releases.
/// - **Tier 1 race**: `cover_art_archive` and `itunes`
///   dispatch in parallel via a `FuturesUnordered` race. The
///   first provider returning [`ProviderOutcome::Hit`] wins;
///   the other in-flight request is dropped. Every provider
///   call is wrapped in a per-provider hard timeout
///   ([`PluginConfig::request_timeout`]); a hung upstream
///   cannot stall the cascade. Deezer is not consulted here:
///   Cover Art Archive and iTunes cover the practical case
///   for album covers. A provider-set choice, not a caching
///   restriction.
/// - **Tier 2 sequential**: `lastfm` (requires operator API
///   key) and `volumio_meta` (operator-opt-in). Attempted in
///   order only when Tier 1 exhausted without a hit; they
///   are not raced so operator upstreams see traffic only
///   when the free canonical Tier 1 could not resolve.
/// - **Circuit breaker per provider**: three consecutive
///   [`ProviderOutcome::Unavailable`] outcomes inside a 60-s
///   window opens the breaker for 5 minutes; open providers
///   are skipped entirely and the cascade continues with the
///   remaining set. A single probe request re-closes the
///   breaker on success or extends the open window on
///   failure.
///
/// # Return
///
/// - [`CascadeResult::Hit`] on the first provider hit;
/// - [`CascadeResult::GenuinelyEmpty`] when every attempted
///   provider returned [`ProviderOutcome::Miss`] — the album
///   is definitively absent from every catalogue we consulted;
/// - [`CascadeResult::Unavailable`] when any provider was
///   [`ProviderOutcome::Unavailable`] and no provider Hit —
///   we could not obtain complete negative evidence and the
///   caller MUST NOT cache this outcome as absence.
pub(crate) async fn run_cascade(
    artist: &str,
    album: &str,
    client: &Client,
    config: &PluginConfig,
) -> CascadeResult {
    // Sanitise inputs before ANY provider sees them. MPD tag
    // drift on well-known releases (Elton's
    // "Goodbye Yellow Brick Road (Disc 1)" → CAA/iTunes/Deezer
    // all miss on the disc suffix; `[Explicit]` /
    // `(Remastered Version)` similarly poison the search
    // terms) is fixed here at the cascade entry so every
    // provider queries with the canonical release title.
    let clean_artist =
        evo_device_audio_shared::artist_name::artist_display_form(artist);
    let clean_album =
        evo_device_audio_shared::album_name::clean_album_title(album);
    let artist_ref = if clean_artist.is_empty() {
        artist
    } else {
        clean_artist.as_str()
    };
    let album_ref = if clean_album.is_empty() {
        album
    } else {
        clean_album.as_str()
    };
    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        artist_raw = artist,
        artist_clean = artist_ref,
        album_raw = album,
        album_clean = album_ref,
        "artwork.online.cascade.begin",
    );

    let mut unavailable_reasons: Vec<String> = Vec::new();
    let mut any_miss = false;

    // ------- Tier 1: race the free canonical providers ------
    //
    // FuturesUnordered gives us first-completed semantics: the
    // fastest provider that returns a Hit wins; the rest are
    // dropped as their futures unwind. Provider outcomes that
    // are not a Hit fold into the miss/unavailable
    // accumulators so the aggregate result is still faithful.

    // Tier 1 futures return `Option<ProviderOutcome>`: `None`
    // means the provider abstained (disabled by config — e.g.
    // CAA when the operator explicitly cleared
    // `musicbrainz_user_agent` to ""). Abstained outcomes do
    // not vote toward `any_miss` (DEFECT-5).
    type Tier1Fut = std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = (&'static str, Option<ProviderOutcome>),
                > + Send,
        >,
    >;
    let tier1: futures_util::stream::FuturesUnordered<Tier1Fut> =
        futures_util::stream::FuturesUnordered::new();
    let mut tier1 = tier1;

    if config.providers.cover_art_archive.enabled
        && circuit_breaker::allow("cover_art_archive")
    {
        let a = artist_ref.to_string();
        let b = album_ref.to_string();
        let c = client.clone();
        let cfg = config.clone();
        tier1.push(Box::pin(async move {
            let out = timed(
                "cover_art_archive",
                cfg.request_timeout,
                cover_art_archive::fetch(&a, &b, &c, &cfg),
            )
            .await;
            ("cover_art_archive", out)
        }));
    }
    if config.providers.itunes.enabled && circuit_breaker::allow("itunes") {
        let a = artist_ref.to_string();
        let b = album_ref.to_string();
        let c = client.clone();
        let to = config.request_timeout;
        tier1.push(Box::pin(async move {
            let out =
                Some(timed_raw("itunes", to, itunes::fetch(&a, &b, &c)).await);
            ("itunes", out)
        }));
    }
    // Deezer is not consulted by the album cascade. Cover Art
    // Archive and iTunes cover the practical case for album
    // covers, and this cascade has never needed a third.
    //
    // This exclusion was originally justified by a
    // never-persist reading of Deezer's terms. That reading is
    // retired — Deezer bytes are now cached like any other
    // provider's, governed by the operator's artwork-caching
    // setting — so what remains here is a provider-set choice
    // about album covers, nothing more. The
    // `[providers.deezer]` toggle governs the artist path and
    // has no effect on this cascade.

    use futures_util::StreamExt;
    while let Some((name, maybe_outcome)) = tier1.next().await {
        let Some(outcome) = maybe_outcome else {
            // Abstained (DEFECT-5): provider did not run
            // because a required config is absent. Skip
            // without counting toward `any_miss` or
            // `unavailable_reasons`.
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = name,
                "abstained (required config absent)",
            );
            continue;
        };
        match outcome {
            ProviderOutcome::Hit(hit) => {
                circuit_breaker::record_success(name);
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    provider = name,
                    tier = "1",
                    winner = true,
                    "artwork.online.cascade.result",
                );
                return CascadeResult::Hit(hit);
            }
            ProviderOutcome::Miss => {
                circuit_breaker::record_success(name);
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = name,
                    "clean miss",
                );
                any_miss = true;
            }
            ProviderOutcome::Unavailable(reason) => {
                circuit_breaker::record_failure(name);
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = name,
                    reason = %reason,
                    "provider unavailable; cascading (result will not be cached negatively)",
                );
                unavailable_reasons.push(reason);
            }
        }
    }

    // ------- Tier 2: sequential operator-opt-in providers ---

    if config.providers.lastfm.enabled && circuit_breaker::allow("lastfm") {
        let maybe_outcome = timed(
            "lastfm",
            config.request_timeout,
            lastfm::fetch(artist_ref, album_ref, client, config),
        )
        .await;
        // Abstained (no API key) → skip without voting toward
        // `any_miss` (DEFECT-5).
        if let Some(outcome) = maybe_outcome {
            match outcome {
                ProviderOutcome::Hit(hit) => {
                    circuit_breaker::record_success("lastfm");
                    tracing::info!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "lastfm",
                        tier = "2",
                        winner = true,
                        "artwork.online.cascade.result",
                    );
                    return CascadeResult::Hit(hit);
                }
                ProviderOutcome::Miss => {
                    circuit_breaker::record_success("lastfm");
                    any_miss = true;
                }
                ProviderOutcome::Unavailable(reason) => {
                    circuit_breaker::record_failure("lastfm");
                    unavailable_reasons.push(reason);
                }
            }
        }
    }

    if config.providers.volumio_meta.enabled
        && circuit_breaker::allow("volumio_meta")
    {
        let outcome = timed_raw(
            "volumio_meta",
            config.request_timeout,
            volumio_meta::fetch(artist_ref, album_ref, client, config),
        )
        .await;
        match outcome {
            ProviderOutcome::Hit(hit) => {
                circuit_breaker::record_success("volumio_meta");
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "volumio_meta",
                    tier = "3",
                    winner = true,
                    "artwork.online.cascade.result",
                );
                return CascadeResult::Hit(hit);
            }
            ProviderOutcome::Miss => {
                circuit_breaker::record_success("volumio_meta");
                any_miss = true;
            }
            ProviderOutcome::Unavailable(reason) => {
                circuit_breaker::record_failure("volumio_meta");
                unavailable_reasons.push(reason);
            }
        }
    }

    // DEFECT-1 fix: `GenuinelyEmpty` requires at least one
    // provider to have actually reached and cleanly missed. An
    // empty consultation set (every provider disabled, or every
    // Tier-1 breaker open, or every provider both) is NOT
    // "definitively absent" — it's "we didn't check". Returning
    // GenuinelyEmpty in that case propagates as NotFound and the
    // framework negatively caches it, poisoning every album
    // resolved during the breaker window until manual cache
    // invalidation. Treat as Unavailable so the framework MUST
    // NOT cache negatively — the honest signal for
    // "we-do-not-know".
    if any_miss && unavailable_reasons.is_empty() {
        tracing::info!(
            plugin = crate::PLUGIN_NAME,
            "artwork.online.cascade.result exhausted (genuinely_empty)",
        );
        CascadeResult::GenuinelyEmpty
    } else if unavailable_reasons.is_empty() {
        tracing::warn!(
            plugin = crate::PLUGIN_NAME,
            "artwork.online.cascade.result exhausted (no provider consulted)",
        );
        CascadeResult::Unavailable(
            "every enabled provider was skipped by a circuit-breaker OPEN state; no provider was consulted for this request"
                .to_string(),
        )
    } else {
        tracing::warn!(
            plugin = crate::PLUGIN_NAME,
            reasons = %unavailable_reasons.join("; "),
            "artwork.online.cascade.result exhausted (unavailable)",
        );
        CascadeResult::Unavailable(unavailable_reasons.join("; "))
    }
}

/// Wrap an Option-returning provider fetch in a hard per-
/// provider timeout.
///
/// - `Ok(Some(outcome))` — provider ran and returned a result;
///   propagate to the cascade.
/// - `Ok(None)` — provider ABSTAINED (disabled by config: no
///   API key, empty UA, etc.). Returned as `None` here so the
///   cascade skips it without counting as a Miss.
/// - `Err(_)` — hard timeout. Returned as
///   `Some(Unavailable)` so the circuit breaker sees it.
///
/// DEFECT-5 fix: an abstained provider must NOT vote toward
/// `any_miss` — an empty UA on CAA (operator explicitly
/// cleared the string) would otherwise contribute to
/// `GenuinelyEmpty` and negatively cache a false "no such
/// release" for every album resolved during that config
/// state.
async fn timed<F>(
    provider: &'static str,
    limit: std::time::Duration,
    fut: F,
) -> Option<ProviderOutcome>
where
    F: std::future::Future<Output = Option<ProviderOutcome>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(outcome) => outcome,
        Err(_) => Some(ProviderOutcome::Unavailable(format!(
            "{provider}: hard timeout after {} ms",
            limit.as_millis(),
        ))),
    }
}

/// Same as [`timed`] but for providers whose fetch signature
/// is `-> ProviderOutcome` (no Option).
async fn timed_raw<F>(
    provider: &'static str,
    limit: std::time::Duration,
    fut: F,
) -> ProviderOutcome
where
    F: std::future::Future<Output = ProviderOutcome>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(outcome) => outcome,
        Err(_) => ProviderOutcome::Unavailable(format!(
            "{provider}: hard timeout after {} ms",
            limit.as_millis(),
        )),
    }
}

/// Per-provider circuit breaker.
///
/// Tracks consecutive [`ProviderOutcome::Unavailable`]
/// outcomes per provider name inside a rolling 60-second
/// window. Three failures opens the breaker for 5 minutes:
/// the provider is skipped by `run_cascade` until the window
/// elapses, then a single probe request runs and closes the
/// breaker on success or resets the open window on failure.
///
/// Scope: process-local, `Mutex`-guarded `HashMap` — no
/// cross-process state, no persistence. On plugin reload the
/// state resets, which is the correct behaviour: a reload
/// probably came from an operator gesture that wants a fresh
/// attempt.
mod circuit_breaker {
    use once_cell::sync::Lazy;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Number of consecutive failures inside `WINDOW` before
    /// the breaker opens.
    const THRESHOLD: u32 = 3;
    /// Rolling window during which failures are counted
    /// toward `THRESHOLD`. Older failures are discarded.
    const WINDOW: Duration = Duration::from_secs(60);
    /// How long the breaker stays open before allowing one
    /// probe request through.
    const OPEN_FOR: Duration = Duration::from_secs(5 * 60);

    /// Per-provider phase.
    ///
    /// - `Closed` — normal operation; every request runs.
    /// - `Open` — every request is skipped until `open_until`
    ///   elapses; then the next `allow()` call may promote to
    ///   `Probing` (single request under a probe-holder guard).
    /// - `Probing` — one in-flight probe request; subsequent
    ///   `allow()` calls return false until the probe records
    ///   an outcome. The probe promotes the breaker back to
    ///   `Closed` on success or to a fresh `Open` window on
    ///   failure.
    #[derive(Debug, Clone)]
    enum Phase {
        Closed,
        Open { open_until: Instant },
        Probing,
    }

    #[derive(Debug, Clone)]
    struct State {
        phase: Phase,
        recent_failures: Vec<Instant>,
    }

    impl State {
        fn new() -> Self {
            Self {
                phase: Phase::Closed,
                recent_failures: Vec::new(),
            }
        }
    }

    static STATE: Lazy<Mutex<HashMap<&'static str, State>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    /// True when the provider is allowed to attempt a request
    /// right now.
    ///
    /// - `Closed` → true.
    /// - `Open` with time elapsed → promote to `Probing` under
    ///   the mutex and return true (single-probe admission).
    /// - `Open` still active → false.
    /// - `Probing` → false (a probe is already in flight).
    ///
    /// DEFECT-2 fix: the phase enum makes the single-probe
    /// invariant explicit and holds across concurrent
    /// `allow()` callers. The prior shape cleared
    /// `recent_failures` speculatively before the probe
    /// resolved, so a persistently-down provider was re-
    /// admitted in bursts of THRESHOLD after each 5-minute
    /// tick — and multiple concurrent cascades all passed
    /// simultaneously through the "elapsed" branch. Now
    /// exactly one probe is in flight at a time; the failure-
    /// count clear happens only when the probe reports
    /// success.
    pub(super) fn allow(provider: &'static str) -> bool {
        let mut map = STATE.lock().expect("circuit-breaker mutex poisoned");
        let state = map.entry(provider).or_insert_with(State::new);
        match &state.phase {
            Phase::Closed => true,
            Phase::Open { open_until } => {
                if Instant::now() >= *open_until {
                    state.phase = Phase::Probing;
                    tracing::info!(
                        plugin = crate::PLUGIN_NAME,
                        provider,
                        "artwork.online.circuit_breaker PROBING",
                    );
                    true
                } else {
                    false
                }
            }
            Phase::Probing => false,
        }
    }

    pub(super) fn record_success(provider: &'static str) {
        let mut map = STATE.lock().expect("circuit-breaker mutex poisoned");
        let state = map.entry(provider).or_insert_with(State::new);
        let was_probing = matches!(state.phase, Phase::Probing);
        state.phase = Phase::Closed;
        state.recent_failures.clear();
        if was_probing {
            tracing::info!(
                plugin = crate::PLUGIN_NAME,
                provider,
                "artwork.online.circuit_breaker CLOSED (probe succeeded)",
            );
        }
    }

    pub(super) fn record_failure(provider: &'static str) {
        let mut map = STATE.lock().expect("circuit-breaker mutex poisoned");
        let state = map.entry(provider).or_insert_with(State::new);
        let now = Instant::now();
        // DEFECT-2 fix: on a failed probe, re-open for the full
        // OPEN_FOR window. Do NOT allow the failure to accrue
        // into a fresh THRESHOLD-count window (which would
        // re-admit after 3 more failures — a burst-open
        // anti-pattern for a persistently-down upstream). The
        // failed probe is proof the upstream is still bad;
        // hold it off for the clean 5-minute window.
        if matches!(state.phase, Phase::Probing) {
            state.phase = Phase::Open {
                open_until: now + OPEN_FOR,
            };
            state.recent_failures.clear();
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider,
                open_for_secs = OPEN_FOR.as_secs(),
                "artwork.online.circuit_breaker RE-OPENED (probe failed)",
            );
            return;
        }
        state
            .recent_failures
            .retain(|t| now.duration_since(*t) < WINDOW);
        state.recent_failures.push(now);
        if state.recent_failures.len() >= THRESHOLD as usize
            && matches!(state.phase, Phase::Closed)
        {
            state.phase = Phase::Open {
                open_until: now + OPEN_FOR,
            };
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider,
                threshold = THRESHOLD,
                window_secs = WINDOW.as_secs(),
                open_for_secs = OPEN_FOR.as_secs(),
                "artwork.online.circuit_breaker OPEN",
            );
        }
    }
}

pub(crate) mod cover_art_archive {
    //! Cover Art Archive provider.
    //!
    //! Two-step lookup:
    //! 1. Query MusicBrainz `release` search for the
    //!    artist+album pair; collect the top N release MBIDs.
    //! 2. For each MBID, hit `coverartarchive.org/release/{mbid}/front`.
    //!    The endpoint returns the front cover image bytes (after
    //!    a 307 redirect to the CDN-hosted URL); the first MBID
    //!    that yields bytes wins.
    //!
    //! MusicBrainz refuses requests without an identifying UA
    //! per their TOS; the provider silently disables when the
    //! operator has not configured one.

    use super::*;
    use anyhow::{Context, Result};

    const MUSICBRAINZ_BASE: &str = "https://musicbrainz.org/ws/2";
    const COVER_ART_BASE: &str = "https://coverartarchive.org";
    /// Cap MBIDs tried per album to keep upstream load bounded.
    /// First MBID is most-relevant per MusicBrainz scoring;
    /// retrying a small number handles the case where the top
    /// hit lacks front-cover artwork.
    const MAX_MBIDS_TRIED: usize = 3;

    /// Fetch attempt against the CAA cascade.
    ///
    /// Returns `None` ONLY when the provider is disabled by
    /// configuration (missing/empty musicbrainz_user_agent) —
    /// disabled providers do not vote toward the aggregate
    /// cascade result. When the provider is enabled, returns
    /// [`ProviderOutcome`] carrying Hit / Miss / Unavailable
    /// per the classification rules on this module.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Option<ProviderOutcome> {
        let ua = match config.musicbrainz_user_agent.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "cover_art_archive",
                    "skipped: musicbrainz_user_agent not configured"
                );
                return None;
            }
        };
        // Step 1: MusicBrainz release search.
        let mbids = match mb_release_search(artist, album, client, ua).await {
            Ok(SearchOutcome::Mbids(m)) => m,
            Ok(SearchOutcome::CleanMiss) => return Some(ProviderOutcome::Miss),
            Err(e) => {
                return Some(classify_reqwest_error(
                    "cover_art_archive (mb release search)",
                    &e,
                ));
            }
        };
        if mbids.is_empty() {
            // MB returned 200 with an empty release array — the
            // release is definitively not indexed. Clean miss.
            return Some(ProviderOutcome::Miss);
        }
        // Step 2: CAA front-cover fetch per MBID. First hit
        // wins. If EVERY MBID cleanly returned "no front cover"
        // (404 on the /front endpoint), that is a clean Miss:
        // the album exists but has no front art in CAA. If any
        // MBID returned Unavailable, propagate.
        let mut any_unavailable: Option<String> = None;
        for mbid in mbids.into_iter().take(MAX_MBIDS_TRIED) {
            match fetch_front_cover(client, &mbid, ua).await {
                Ok(FrontCoverOutcome::Hit(bytes)) => {
                    return Some(ProviderOutcome::Hit(ProviderHit {
                        bytes,
                        mime: Some("image/jpeg".to_string()),
                        provider_id: "cover_art_archive",
                    }));
                }
                Ok(FrontCoverOutcome::Miss) => continue,
                Err(e) => {
                    any_unavailable = Some(format!(
                        "cover_art_archive (front mbid={mbid}): {e}"
                    ));
                }
            }
        }
        Some(match any_unavailable {
            Some(reason) => ProviderOutcome::Unavailable(reason),
            None => ProviderOutcome::Miss,
        })
    }

    enum SearchOutcome {
        Mbids(Vec<String>),
        CleanMiss,
    }

    enum FrontCoverOutcome {
        Hit(Vec<u8>),
        Miss,
    }

    async fn mb_release_search(
        artist: &str,
        album: &str,
        client: &Client,
        ua: &str,
    ) -> Result<SearchOutcome> {
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist),
            escape_lucene(album),
        );
        let url = format!(
            "{}/release?query={}&fmt=json&limit=5",
            MUSICBRAINZ_BASE,
            urlencode(&query),
        );
        let resp = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, ua)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .context("musicbrainz release search request failed")?;
        match classify_status("mb release search", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Ok(SearchOutcome::CleanMiss),
            StatusClass::Unavailable(reason) => {
                anyhow::bail!(reason);
            }
        }
        let json: serde_json::Value =
            resp.json().await.context("musicbrainz response decode")?;
        let mbids: Vec<String> = json
            .get("releases")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        r.get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(SearchOutcome::Mbids(mbids))
    }

    async fn fetch_front_cover(
        client: &Client,
        mbid: &str,
        ua: &str,
    ) -> Result<FrontCoverOutcome> {
        let url = format!("{}/release/{}/front", COVER_ART_BASE, mbid);
        let resp = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, ua)
            .send()
            .await
            .context("cover-art-archive front fetch failed")?;
        match classify_status("caa front", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Ok(FrontCoverOutcome::Miss),
            StatusClass::Unavailable(reason) => anyhow::bail!(reason),
        }
        let bytes = resp.bytes().await.context("cover bytes read")?;
        Ok(FrontCoverOutcome::Hit(bytes.to_vec()))
    }

    /// Escape Lucene query special characters per
    /// MusicBrainz's query language. Conservative: covers the
    /// characters that would otherwise corrupt the query shape.
    pub(crate) fn escape_lucene(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' | '"' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '^'
                | '~' | '*' | '?' | '+' | '-' | '!' | '&' | '|' => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out
    }
}

pub(crate) mod lastfm {
    //! Last.fm `album.getinfo` provider.
    //!
    //! Requires an operator-supplied API key (`api_key` in the
    //! `[providers.lastfm]` table). The API returns a JSON
    //! payload with an `image` array carrying multiple sizes;
    //! we pick the largest (`mega` / `extralarge`) and fetch.

    use super::*;
    // anyhow no longer used since fetch returns ProviderOutcome directly.

    const LASTFM_BASE: &str = "https://ws.audioscrobbler.com/2.0/";

    /// Fetch attempt against Last.fm album.getinfo.
    ///
    /// Returns `None` when the provider is disabled (no API key
    /// configured). Otherwise returns [`ProviderOutcome`].
    /// Last.fm's API returns a 200 OK JSON body with an
    /// `error` field on catalogue misses; we treat that as a
    /// clean Miss. HTTP-level 5xx / 429 / transport errors are
    /// Unavailable.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Option<ProviderOutcome> {
        let api_key = match config.providers.lastfm.api_key.as_deref() {
            Some(k) if !k.is_empty() => k,
            _ => return None,
        };
        let url = format!(
            "{}?method=album.getinfo&api_key={}&artist={}&album={}&format=json",
            LASTFM_BASE,
            urlencode(api_key),
            urlencode(artist),
            urlencode(album),
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm: {e}"
                )));
            }
        };
        match classify_status("lastfm", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Some(ProviderOutcome::Miss),
            StatusClass::Unavailable(r) => {
                return Some(ProviderOutcome::Unavailable(r));
            }
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm: json decode: {e}"
                )));
            }
        };
        // Last.fm signals catalogue misses as `{ error: N,
        // message: "..." }` at 200 OK. Treat as clean Miss.
        if json.get("error").is_some() {
            return Some(ProviderOutcome::Miss);
        }
        // Last.fm's image array carries entries shaped
        // `{ size: "mega" | "extralarge" | ..., "#text": "<url>" }`.
        let url = json
            .get("album")
            .and_then(|a| a.get("image"))
            .and_then(serde_json::Value::as_array)
            .and_then(|imgs| {
                ["mega", "extralarge", "large", "medium", "small"]
                    .iter()
                    .find_map(|want| {
                        imgs.iter().find_map(|img| {
                            let size = img
                                .get("size")
                                .and_then(serde_json::Value::as_str)?;
                            if size == *want {
                                img.get("#text")
                                    .and_then(serde_json::Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                            } else {
                                None
                            }
                        })
                    })
            });
        let Some(image_url) = url else {
            return Some(ProviderOutcome::Miss);
        };
        let img_resp = match client.get(&image_url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm image: {e}"
                )));
            }
        };
        match classify_status("lastfm image", img_resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Some(ProviderOutcome::Miss),
            StatusClass::Unavailable(r) => {
                return Some(ProviderOutcome::Unavailable(r));
            }
        }
        let bytes = match img_resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm image bytes: {e}"
                )));
            }
        };
        Some(ProviderOutcome::Hit(ProviderHit {
            bytes,
            mime: None,
            provider_id: "lastfm",
        }))
    }
}

pub(crate) mod itunes {
    //! Apple iTunes Search API provider.
    //!
    //! No API key required. Returns 100x100 thumbnail URLs in
    //! `artworkUrl100`; URL-pattern trick swaps the size segment
    //! to fetch a larger variant (`/100x100bb/` →
    //! `/600x600bb/`).

    use super::*;
    // anyhow no longer used since fetch returns ProviderOutcome directly.

    const ITUNES_BASE: &str = "https://itunes.apple.com/search";

    /// Fetch attempt against iTunes Search API.
    ///
    /// Always attempted (no key). Miss = 200 OK with empty
    /// `results` or missing `artworkUrl100`. Unavailable =
    /// transport / 5xx / 429 / decode errors.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
    ) -> ProviderOutcome {
        let term = format!("{artist} {album}");
        let url = format!(
            "{}?term={}&entity=album&limit=1",
            ITUNES_BASE,
            urlencode(&term),
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!("itunes: {e}"));
            }
        };
        match classify_status("itunes", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "itunes json: {e}"
                ));
            }
        };
        let thumb_url = json
            .get("results")
            .and_then(serde_json::Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|hit| hit.get("artworkUrl100"))
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        let Some(thumb_url) = thumb_url else {
            return ProviderOutcome::Miss;
        };
        // Upscale the thumbnail URL to 600x600 via the
        // documented URL-pattern trick.
        let upscaled = thumb_url.replace("/100x100bb.", "/600x600bb.");
        let img_resp = match client.get(&upscaled).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "itunes image: {e}"
                ));
            }
        };
        match classify_status("itunes image", img_resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let bytes = match img_resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "itunes image bytes: {e}"
                ));
            }
        };
        ProviderOutcome::Hit(ProviderHit {
            bytes,
            mime: None,
            provider_id: "itunes",
        })
    }
}

pub(crate) mod volumio_meta {
    //! Volumio's hosted meta proxy provider.
    //!
    //! Endpoint: `https://meta.volumio.org/metas/v1/getDatas`
    //! with `mode=albumArt`, `artist`, `album`, `variant`. The
    //! `variant` selects between Volumio's community + commercial
    //! distribution paths.

    use super::*;
    // anyhow no longer used since fetch returns ProviderOutcome directly.

    const VOLUMIO_META_BASE: &str =
        "https://meta.volumio.org/metas/v1/getDatas";

    /// Fetch attempt against Volumio meta-proxy.
    ///
    /// Miss = 200 OK with empty `data`. Unavailable = transport
    /// / 5xx / 429 / decode errors.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> ProviderOutcome {
        let variant = &config.providers.volumio_meta.variant;
        let url = format!(
            "{}?mode=albumArt&artist={}&album={}&variant={}",
            VOLUMIO_META_BASE,
            urlencode(artist),
            urlencode(album),
            urlencode(variant),
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta: {e}"
                ));
            }
        };
        match classify_status("volumio_meta", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta json: {e}"
                ));
            }
        };
        let image_url = json
            .get("data")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let Some(image_url) = image_url else {
            return ProviderOutcome::Miss;
        };
        let img_resp = match client.get(&image_url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta image: {e}"
                ));
            }
        };
        match classify_status("volumio_meta image", img_resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let bytes = match img_resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta image bytes: {e}"
                ));
            }
        };
        ProviderOutcome::Hit(ProviderHit {
            bytes,
            mime: None,
            provider_id: "volumio_meta",
        })
    }
}

/// Minimal percent-encoder for URL query values. Mirrors the
/// helper in the shared crate's artwork_target_url; inlined here
/// to keep the plugin's outbound URL construction self-contained.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_art_archive_lucene_escape() {
        let escaped = cover_art_archive::escape_lucene(
            r#"Sting: A "Test" Album (Deluxe)"#,
        );
        assert!(escaped.contains(r#"\""#));
        assert!(escaped.contains(r#"\:"#));
        assert!(escaped.contains(r#"\("#));
        assert!(escaped.contains(r#"\)"#));
    }

    #[test]
    fn urlencode_unreserved_passthrough() {
        assert_eq!(urlencode("Beatles_Revolver"), "Beatles_Revolver");
        assert_eq!(urlencode("AC&DC"), "AC%26DC");
        assert_eq!(urlencode("Sigur Rós"), "Sigur%20R%C3%B3s");
    }
}
