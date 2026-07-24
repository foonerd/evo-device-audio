// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Shared HTTPS client, per-provider rate limiting, and
//! MusicBrainz API access primitives for online-metadata plugins
//! in the evo audio reference distribution.
//!
//! This crate is the single point at which the audio
//! distribution builds its outbound HTTPS client + throttles
//! per-provider request rates. Both `artwork.online` (cover
//! bytes) and `metadata.online` (bio, notes, lyrics, MusicBrainz
//! reconciliation) consume it — one connection pool, one DNS
//! cache, one shared token bucket per provider. Without this
//! sharing, two plugins hitting MusicBrainz independently would
//! bust the 1 req/sec API policy under any browse burst.
//!
//! ## Contents
//!
//! - [`build_http_client`] — reqwest client factory with the
//!   distribution's canonical TLS + timeout + redirect posture.
//! - [`RateLimiter`] — single-provider token bucket. Refill rate
//!   is per-second; burst capacity is 1 (strict rate limiting,
//!   no bursty compensation). Multiple in-flight callers await
//!   sequentially.
//! - [`musicbrainz`] — MusicBrainz JSON API client (search
//!   releases, look up release+release-group). Governs the API's
//!   1 req/sec cap via a shared `RateLimiter`, threads the
//!   distribution's User-Agent, parses the salient response
//!   fields into strongly-typed structs.
//!
//! Cover Art Archive (CAA) is deliberately NOT rate-limited
//! here: it's a static-file service (imagedelivery from the
//! MetaBrainz CDN, not `musicbrainz.org`) and has no
//! per-second policy. Only calls to `musicbrainz.org/ws/2/…`
//! flow through the [`musicbrainz`] client.

pub mod discogs;
pub mod genius;
pub mod http;
pub mod lastfm;
pub mod lrclib;
pub mod musicbrainz;
pub mod rate_limit;
pub mod wikidata;
pub mod wikipedia;

pub use discogs::{
    ArtistProfileHit, DiscogsClient, DiscogsError, ReleaseDetailHit,
};
pub use genius::{
    ArtistDescriptionHit, GeniusClient, GeniusError, TrackAnnotationHit,
};
pub use http::build_http_client;
pub use lastfm::{
    is_notfound_code as lastfm_is_notfound_code,
    AlbumNotesHit as LastfmAlbumNotesHit, BioHit as LastfmBioHit, LastfmClient,
    LastfmError,
};
pub use lrclib::{LrclibClient, LrclibError, LyricsHit};
pub use musicbrainz::{
    ArtistLookup, ArtistSearchHit, MusicBrainzClient, MusicBrainzError,
    ReleaseCreditsLookup, TrackCredits, WorkLookup, WorkSearchHit,
};
pub use rate_limit::RateLimiter;
/// Re-export of the concrete HTTPS client type
/// [`build_http_client`] returns, so callers can name the type in
/// helper signatures without taking a direct `reqwest` dep.
pub use reqwest::Client as HttpClient;
pub use wikidata::{WikidataClient, WikidataEntityHit, WikidataError};
pub use wikipedia::{WikipediaClient, WikipediaError, WikipediaSummaryHit};
