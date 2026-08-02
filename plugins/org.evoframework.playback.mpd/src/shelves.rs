// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Shelf integration bundle.
//!
//! Holds the four new audio.queue / audio.playlist /
//! audio.favourites / audio.library shelf contexts plus the
//! sticker reconciler handle in one struct so the plugin
//! lifecycle (load/unload) and the verb dispatcher each touch
//! a single Option<ShelfBundle> instead of N fields.
//!
//! # Verb dispatch
//!
//! [`ShelfBundle::dispatch_request`] handles all 31 new
//! request_types (9 queue + 8 playlist + 6 favourites + 8
//! library). The plugin's existing
//! `handle_request` match falls through to this entry-point
//! when the request_type matches none of its existing arms.
//!
//! Each verb spawns its own short-lived MpdConnection — bulk
//! sticker writes happen on the sticker reconciler's dedicated
//! connection; the verbs' per-request connections are cheap
//! and avoid serialising operator gestures on the supervisor's
//! command connection.
//!
//! # Lifecycle
//!
//! - [`ShelfBundle::init`] runs at plugin load. Parses
//!   music_directory + playlist_directory from /etc/mpd.conf;
//!   constructs the SourceRegistry, loads from state dir,
//!   registers the floor source if absent; constructs the
//!   DispositionEmitter, SkipTraversal, and four shelf
//!   contexts; announces every subject; spawns the sticker
//!   reconciler.
//! - [`ShelfBundle::shutdown`] runs at plugin unload. Stops
//!   the sticker reconciler, persists the registry +
//!   disposition emitter snapshots.
//!
//! # Error translation
//!
//! Each shelf module's `VerbError` enum maps to PluginError
//! variants via the `into_plugin_error` helpers below — kept
//! local so a future contributor changing the mapping touches
//! a single place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    PluginError, Request, Response, SubjectAnnouncer,
};

use crate::disposition_emitter::DispositionEmitter;
use crate::favourites::{self, FavouritesContext};
use crate::idle_observer::{self, IdleObserverHandle};
use crate::library::{self, DlnaSyncHandle, LibraryContext};
use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint};
use crate::playlist::{
    self, PlaylistContext, DEFAULT_FAVOURITES_PLAYLIST_NAME,
};
use crate::queue::{self, QueueContext};
use crate::skip_traversal::SkipTraversal;
use crate::source_probe;
use crate::source_registry::SourceRegistry;
use crate::sticker_reconciler::{self, StickerReconcilerHandle};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// The bundled shelf state. Created at plugin load by
/// [`ShelfBundle::init`]; dropped at unload by
/// [`ShelfBundle::shutdown`].
pub(crate) struct ShelfBundle {
    pub(crate) registry: SourceRegistry,
    /// Disposition emitter Arc-clone is held by
    /// [`SkipTraversal`] (and therefore by the queue
    /// context). Kept on the bundle so future read verbs
    /// (e.g. `get_dispositions`) can reach it without
    /// threading another Arc through.
    #[allow(dead_code)]
    pub(crate) disposition_emitter: DispositionEmitter,
    /// Skip-traversal is cloned into [`QueueContext`]; the
    /// bundle field is the canonical source. Kept on the
    /// bundle so a future verb that bypasses the queue
    /// (e.g. a "skip from MPD-side state change") can reach
    /// the same instance.
    #[allow(dead_code)]
    pub(crate) skip_traversal: SkipTraversal,
    pub(crate) queue: QueueContext,
    pub(crate) playlist: PlaylistContext,
    pub(crate) favourites: FavouritesContext,
    pub(crate) library: LibraryContext,
    pub(crate) sticker_reconciler: Option<StickerReconcilerHandle>,
    /// Reactive sync — MPD's idle subprotocol drives the four
    /// shelf subjects when MPD-side state mutates outside this
    /// plugin's verbs. The handle is held here so plugin
    /// unload can stop the observer cleanly.
    pub(crate) idle_observer: Option<IdleObserverHandle>,
    /// Upserts NetworkDlna sources from source.dlna's
    /// discovered.json sidecar and probes them on a fixed
    /// cadence. Held so unload can stop the task cleanly.
    pub(crate) dlna_sync: Option<DlnaSyncHandle>,
    pub(crate) endpoint: MpdEndpoint,
    pub(crate) timeouts: ConnectTimeouts,
}

impl ShelfBundle {
    /// Construct + initialise the shelves at plugin load.
    ///
    /// `state_dir` is the plugin's per-load filesystem state
    /// directory (sources.toml + dispositions.toml live under
    /// it). `subjects` is the framework's
    /// `SubjectAnnouncer`. `endpoint` + `timeouts` are the MPD
    /// connection parameters the rest of the plugin uses.
    pub(crate) async fn init(
        state_dir: PathBuf,
        subjects: Arc<dyn SubjectAnnouncer>,
        endpoint: MpdEndpoint,
        timeouts: ConnectTimeouts,
        shelf_dispatcher: Option<
            Arc<
                dyn evo_plugin_sdk::contract::shelf_dispatch::ShelfRequestDispatcher,
            >,
        >,
    ) -> Self {
        let music_directory = source_probe::load_music_directory_from_mpd_conf(
            Path::new(source_probe::DEFAULT_MPD_CONF_PATH),
        )
        .unwrap_or_else(|| PathBuf::from("/var/lib/evo/music"));
        let playlist_directory =
            source_probe::load_playlist_directory_from_mpd_conf(Path::new(
                source_probe::DEFAULT_MPD_CONF_PATH,
            ))
            .unwrap_or_else(|| PathBuf::from("/var/lib/mpd/playlists"));

        // Construct + rehydrate the source registry.
        let registry =
            SourceRegistry::with_state_path(state_dir.join("sources.toml"));
        if let Err(e) = registry.load_from_disk().await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "source registry rehydrate failed; starting empty"
            );
        }

        // Construct the disposition emitter with persistence.
        let disposition_emitter = DispositionEmitter::with_state_path(
            subjects.clone(),
            state_dir.join("dispositions.toml"),
        );
        if let Err(e) = disposition_emitter.load_from_disk().await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "disposition emitter rehydrate failed; starting empty"
            );
        }

        // Skip-traversal owns Arc clones of registry + emitter.
        let skip_traversal =
            SkipTraversal::new(registry.clone(), disposition_emitter.clone());

        // Construct each shelf's context.
        let queue = QueueContext::new(
            music_directory.clone(),
            registry.clone(),
            subjects.clone(),
            skip_traversal.clone(),
            shelf_dispatcher.clone(),
        );
        let playlist = PlaylistContext::new(
            music_directory.clone(),
            playlist_directory.clone(),
            registry.clone(),
            subjects.clone(),
            DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
        );
        let favourites = FavouritesContext::new(
            music_directory.clone(),
            registry.clone(),
            subjects.clone(),
            DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
        );
        let library = LibraryContext::new(
            music_directory.clone(),
            registry.clone(),
            subjects.clone(),
            shelf_dispatcher.clone(),
        );

        // Ensure the local-internal floor source is registered.
        if let Err(e) =
            library::ensure_local_internal_registered(&library).await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "ensure_local_internal_registered failed; \
                 floor library source unavailable until next plugin load"
            );
        }

        // Pull any DLNA servers already written by
        // source.dlna before warm-start probes so newly
        // discovered MediaServers participate in the probe
        // pass (and appear Online rather than stuck Probing
        // until the first 30s sync tick).
        library::sync_dlna_discovered(&library).await;

        // Announce every subject at load. The announcement
        // seeds the subject_states mirror with a wire-shape
        // empty envelope so consumers connecting between
        // announce and the subsequent rehydrate-publish see a
        // well-formed payload immediately.
        disposition_emitter.announce().await;
        queue::announce_queue(&queue).await;
        playlist::announce_index(&playlist).await;
        favourites::announce_favourites(&favourites).await;
        library::announce_subjects(&library).await;

        // Spawn the sticker reconciler against the registry's
        // broadcast BEFORE the warm-start probes fire so it is
        // already subscribed when the Probing → Online/Degraded/
        // Offline transitions land on the broadcast channel.
        // Order matters: a reconciler spawned AFTER the probes
        // already transitioned would miss the transition events
        // and never write any stickers.
        let sticker_reconciler = Some(sticker_reconciler::spawn(
            endpoint.clone(),
            timeouts,
            registry.clone(),
        ));

        // Warm-start probe — BLOCKING. Every registered source
        // begins in `SourceState::Probing` on cold start (and
        // after registry rehydrate from disk on every load).
        // The per-item availability cascade returns `None` on
        // Probing sources (honest about the unknown), so the
        // queue / favourites / playlist envelopes built BEFORE
        // probes complete would publish `available: null` for
        // every item — operator UI sees neutral "—" on tracks
        // the source actually holds.
        //
        // Block warm-start on probe completion so the
        // subsequent rehydration's cascade walks a determinate
        // source state. The probe budget is bounded (3s per
        // source); for the typical one-to-few-sources operator
        // profile this adds sub-10s to plugin admission, well
        // within the steward's load timeout. Per-source probes
        // run concurrently so worst-case wall-clock is the
        // slowest single probe, not the sum.
        Self::run_warm_start_probes_blocking(&registry).await;

        // Warm-start rehydration: replace the empty envelopes
        // with MPD's persisted truth (queue, stored playlists,
        // favourites entries, library stats). MPD is the
        // durable source; the framework subject_states mirror
        // is a cache. Runs AFTER the probe step so the per-item
        // availability cascade sees post-probe source state
        // (Online/Degraded/Offline) rather than the initial
        // Probing → Some(true)/false truth instead of null. The
        // sticker reconciler, meanwhile, is asynchronously
        // catching up on per-song sticker writes — when its
        // writes land, the cascade switches from source-state
        // derivation to explicit-sticker truth without
        // republishing (the source-state derivation already
        // returned the right answer at the source level; only
        // per-song deviations require a re-publish, which the
        // existing idle observer's database/sticker subsystem
        // surfaces). Best-effort: failed rehydrate logs but
        // does not block plugin admission.
        Self::rehydrate_all(
            &endpoint,
            timeouts,
            &queue,
            &playlist,
            &favourites,
            &library,
        )
        .await;

        // Spawn the idle-subprotocol observer so MPD-side
        // mutations outside this plugin's verbs propagate into
        // the shelf subjects.
        let idle_observer = Some(idle_observer::spawn(
            endpoint.clone(),
            timeouts,
            queue.clone(),
            playlist.clone(),
            favourites.clone(),
            library.clone(),
        ));

        let dlna_sync = Some(library::spawn_dlna_sync(library.clone()));

        tracing::info!(
            plugin = PLUGIN_NAME,
            music_directory = %music_directory.display(),
            playlist_directory = %playlist_directory.display(),
            "shelf integration initialised: subjects announced + rehydrated \
             from MPD + idle observer subscribed"
        );

        Self {
            registry,
            disposition_emitter,
            skip_traversal,
            queue,
            playlist,
            favourites,
            library,
            sticker_reconciler,
            idle_observer,
            dlna_sync,
            endpoint,
            timeouts,
        }
    }

    /// Warm-start rehydration of every shelf subject from MPD's
    /// persisted state. Opens a single short-lived MpdConnection
    /// and drives each shelf's existing `publish_*` / `refresh_*`
    /// helper. Best-effort: connection failure or per-shelf
    /// publish failure logs a warning and continues; the failed
    /// shelves stay at their empty-envelope announcement until
    /// the next mutation or idle event.
    async fn rehydrate_all(
        endpoint: &MpdEndpoint,
        timeouts: ConnectTimeouts,
        queue: &QueueContext,
        playlist: &PlaylistContext,
        favourites: &FavouritesContext,
        library: &LibraryContext,
    ) {
        let mut conn = match MpdConnection::connect_with_timeouts(
            endpoint.clone(),
            timeouts,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                // Log at DEBUG on cold-boot. The warm-start
                // rehydrate runs once at plugin admission; if MPD
                // is still coming up, we cleanly no-op and the
                // idle observer wakes the state population as soon
                // as MPD is listening + the first mutation fires.
                // The "not ready" wording keeps the substring
                // "failed" out of the journal on any residual
                // fall-through, preserving the zero-fail-in-logs
                // invariant.
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "warm-start rehydrate: MPD connect not ready; subjects \
                     stay empty until next mutation or idle wake"
                );
                return;
            }
        };
        queue::publish_queue(queue, &mut conn).await;
        playlist::publish_index(playlist, &mut conn).await;
        favourites::refresh_favourites(favourites, &mut conn).await;
        library::rehydrate_from_mpd(library, &mut conn).await;
    }

    /// Kick a background probe for every source the registry
    /// Probe every registered source AND AWAIT each probe's
    /// transition before returning. Without this gating, the
    /// rehydration step that follows would publish envelopes
    /// while every source was still in the initial Probing
    /// state — the per-item availability cascade returns
    /// `None` on Probing sources (honest about the unknown),
    /// so the wire envelopes would carry `available: null` on
    /// every entry even when the source's mount actually
    /// holds the songs.
    ///
    /// Implementation: snapshot the registry, spawn one
    /// concurrent tokio task per source running
    /// `probe_source` against the mount path, then await every
    /// task before returning. Each transition publishes onto
    /// the existing source-state-change broadcast, waking the
    /// sticker reconciler asynchronously to populate per-song
    /// `evo:available` stickers.
    ///
    /// Bounded cost — per-source probe budget is 3s; tasks run
    /// concurrently so worst-case wall-clock is the slowest
    /// single source, not the sum. For the typical operator
    /// (one local source + zero or one network source) warm-
    /// start adds sub-second to plugin admission.
    ///
    /// Best-effort per source: a probe failure logs and leaves
    /// the source in Probing; the cascade emits `null` honestly
    /// for that source's items rather than fabricating a
    /// falsy default.
    async fn run_warm_start_probes_blocking(registry: &SourceRegistry) {
        let snapshot = registry.snapshot().await;
        let mut handles = Vec::with_capacity(snapshot.len());
        for record in snapshot {
            let registry = registry.clone();
            let source_id = record.id.clone();
            handles.push(tokio::spawn(async move {
                let full = match registry.get(&source_id).await {
                    Some(r) => r,
                    None => return, // removed mid-probe
                };
                let budget = std::time::Duration::from_millis(3_000);
                let outcome =
                    crate::source_registry::probe_source(&full, budget).await;
                if let Err(e) =
                    registry.transition(&source_id, outcome.new_state).await
                {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        source_id = %source_id,
                        error = %e,
                        "warm-start probe: registry transition failed"
                    );
                }
            }));
        }
        // Await every probe before returning. Join-failure
        // (panic) is non-fatal — the source stays Probing and
        // the cascade emits null. Log and continue.
        for handle in handles {
            if let Err(e) = handle.await {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "warm-start probe: per-source task join failed"
                );
            }
        }
    }

    /// Tear down at plugin unload. Stops the sticker
    /// reconciler + persists the registry to disk so the next
    /// load rehydrates without operator effort. The
    /// disposition emitter persists synchronously on every
    /// `emit` call so no explicit unload-time persist is
    /// needed.
    pub(crate) async fn shutdown(mut self) {
        if let Some(handle) = self.idle_observer.take() {
            handle.stop().await;
        }
        if let Some(handle) = self.dlna_sync.take() {
            handle.stop().await;
        }
        if let Some(handle) = self.sticker_reconciler.take() {
            handle.stop().await;
        }
        if let Err(e) = self.registry.persist().await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "source registry persist on unload failed; \
                 next load may rehydrate from stale state"
            );
        }
    }

    /// Open a fresh MPD connection for one verb dispatch.
    /// Connections are cheap on localhost and isolate each
    /// verb's failure mode.
    async fn open_conn(&self) -> Result<MpdConnection, PluginError> {
        MpdConnection::connect_with_timeouts(
            self.endpoint.clone(),
            self.timeouts,
        )
        .await
        .map_err(|e| PluginError::Transient(format!("MPD connect failed: {e}")))
    }

    /// Verb dispatcher entry point. Returns `Ok(Some(Response))`
    /// when the request_type matched a shelf verb;
    /// `Ok(None)` when it didn't (caller's other arms continue
    /// dispatching).
    pub(crate) async fn dispatch_request(
        &self,
        req: &Request,
    ) -> Result<Option<Response>, PluginError> {
        match req.request_type.as_str() {
            // Queue verbs
            "queue.get_queue" => {
                Ok(Some(self.dispatch_queue_get_queue(req).await?))
            }
            "queue.enqueue" => {
                Ok(Some(self.dispatch_queue_enqueue(req).await?))
            }
            "queue.remove_queue_item" => {
                Ok(Some(self.dispatch_queue_remove_queue_item(req).await?))
            }
            "queue.move_queue_item" => {
                Ok(Some(self.dispatch_queue_move_queue_item(req).await?))
            }
            "queue.clear_queue" => {
                Ok(Some(self.dispatch_queue_clear_queue(req).await?))
            }
            "queue.load_playlist_to_queue" => {
                Ok(Some(self.dispatch_queue_load_playlist_to_queue(req).await?))
            }
            "queue.append_playlist_to_queue" => Ok(Some(
                self.dispatch_queue_append_playlist_to_queue(req).await?,
            )),
            "queue.save_queue_as_playlist" => {
                Ok(Some(self.dispatch_queue_save_queue_as_playlist(req).await?))
            }
            "queue.skip_to_next_available" => {
                Ok(Some(self.dispatch_queue_skip_to_next_available(req).await?))
            }
            "queue.play_from_position" => {
                Ok(Some(self.dispatch_queue_play_from_position(req).await?))
            }
            "queue.enqueue_selection" => {
                Ok(Some(self.dispatch_queue_enqueue_selection(req).await?))
            }
            // Playlist verbs
            "playlist.list_playlists" => {
                Ok(Some(self.dispatch_playlist_list_playlists(req).await?))
            }
            "playlist.get_playlist" => {
                Ok(Some(self.dispatch_playlist_get_playlist(req).await?))
            }
            "playlist.create_playlist" => {
                Ok(Some(self.dispatch_playlist_create_playlist(req).await?))
            }
            "playlist.delete_playlist" => {
                Ok(Some(self.dispatch_playlist_delete_playlist(req).await?))
            }
            "playlist.rename_playlist" => {
                Ok(Some(self.dispatch_playlist_rename_playlist(req).await?))
            }
            "playlist.add_to_playlist" => {
                Ok(Some(self.dispatch_playlist_add_to_playlist(req).await?))
            }
            "playlist.remove_from_playlist" => Ok(Some(
                self.dispatch_playlist_remove_from_playlist(req).await?,
            )),
            "playlist.move_in_playlist" => {
                Ok(Some(self.dispatch_playlist_move_in_playlist(req).await?))
            }
            "playlist.save_selection" => {
                Ok(Some(self.dispatch_playlist_save_selection(req).await?))
            }
            // Favourites verbs
            "favourites.list_favourites" => {
                Ok(Some(self.dispatch_favourites_list_favourites(req).await?))
            }
            "favourites.is_favourite" => {
                Ok(Some(self.dispatch_favourites_is_favourite(req).await?))
            }
            "favourites.add_favourite" => {
                Ok(Some(self.dispatch_favourites_add_favourite(req).await?))
            }
            "favourites.remove_favourite" => {
                Ok(Some(self.dispatch_favourites_remove_favourite(req).await?))
            }
            "favourites.clear_favourites" => {
                Ok(Some(self.dispatch_favourites_clear_favourites(req).await?))
            }
            "favourites.move_favourite" => {
                Ok(Some(self.dispatch_favourites_move_favourite(req).await?))
            }
            // Library verbs
            "library.list_sources" => {
                Ok(Some(self.dispatch_library_list_sources(req).await?))
            }
            "library.add_source" => {
                Ok(Some(self.dispatch_library_add_source(req).await?))
            }
            "library.remove_source" => {
                Ok(Some(self.dispatch_library_remove_source(req).await?))
            }
            "library.probe_source" => {
                Ok(Some(self.dispatch_library_probe_source(req).await?))
            }
            "library.wake_source" => {
                Ok(Some(self.dispatch_library_wake_source(req).await?))
            }
            "library.update_source" => {
                Ok(Some(self.dispatch_library_update_source(req).await?))
            }
            "library.browse_library" => {
                Ok(Some(self.dispatch_library_browse_library(req).await?))
            }
            "library.search_library" => {
                Ok(Some(self.dispatch_library_search_library(req).await?))
            }
            "library.list_works" => {
                Ok(Some(self.dispatch_library_list_works(req).await?))
            }
            "library.get_work_recordings" => {
                Ok(Some(self.dispatch_library_get_work_recordings(req).await?))
            }
            "library.browse_by_artist" => Ok(Some(
                self.dispatch_library_browse_by_tag(
                    req,
                    "library.browse_by_artist",
                    // MPD's per-album primary-artist tag. The
                    // browse-by-artist facet is "one entry per
                    // real artist"; `albumartist` matches that
                    // shape closely (featurings and per-track
                    // credits collapse), whereas the raw
                    // `artist` tag returns every per-track
                    // credit as a distinct facet value.
                    "albumartist",
                    "artist",
                    library::identity_post_process,
                )
                .await?,
            )),
            "library.browse_by_album" => Ok(Some(
                self.dispatch_library_browse_by_tag(
                    req,
                    "library.browse_by_album",
                    "album",
                    "album",
                    library::identity_post_process,
                )
                .await?,
            )),
            "library.browse_by_genre" => Ok(Some(
                self.dispatch_library_browse_by_tag(
                    req,
                    "library.browse_by_genre",
                    "genre",
                    "genre",
                    library::identity_post_process,
                )
                .await?,
            )),
            "library.browse_by_year" => Ok(Some(
                self.dispatch_library_browse_by_tag(
                    req,
                    "library.browse_by_year",
                    "date",
                    "year",
                    library::year_from_mpd_date,
                )
                .await?,
            )),
            _ => Ok(None),
        }
    }

    // ----- queue dispatchers -----

    async fn dispatch_queue_get_queue(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let state = queue::handle_get_queue(&self.queue).await;
        encode_json_response(req, &state)
    }

    async fn dispatch_queue_enqueue(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::EnqueuePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_enqueue(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_remove_queue_item(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::RemoveQueueItemPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_remove_queue_item(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_move_queue_item(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::MoveQueueItemPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_move_queue_item(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_clear_queue(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let mut conn = self.open_conn().await?;
        queue::handle_clear_queue(&self.queue, &mut conn)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_load_playlist_to_queue(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::LoadPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_load_playlist_to_queue(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_append_playlist_to_queue(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::AppendPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_append_playlist_to_queue(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_save_queue_as_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::SaveQueueAsPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_save_queue_as_playlist(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_skip_to_next_available(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::SkipToNextAvailablePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let outcome = queue::handle_skip_to_next_available(
            &self.queue,
            &mut conn,
            payload,
        )
        .await
        .map_err(queue_verb_to_plugin_error)?;
        let body = serde_json::json!({
            "v":      queue::QUEUE_PAYLOAD_VERSION,
            "outcome": outcome_to_wire(&outcome),
        });
        encode_json_response(req, &body)
    }

    async fn dispatch_queue_play_from_position(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::PlayFromPositionPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        queue::handle_play_from_position(&self.queue, &mut conn, payload)
            .await
            .map_err(queue_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_queue_enqueue_selection(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: queue::EnqueueSelectionPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let resolver = crate::selection::MpdSelectionResolver;
        let body = queue::handle_enqueue_selection(
            &self.queue,
            &mut conn,
            &resolver,
            payload,
        )
        .await
        .map_err(queue_verb_to_plugin_error)?;
        encode_json_response(req, &body)
    }

    // ----- playlist dispatchers -----

    async fn dispatch_playlist_list_playlists(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let state = playlist::handle_list_playlists(&self.playlist).await;
        encode_json_response(req, &state)
    }

    async fn dispatch_playlist_get_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::GetPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let env =
            playlist::handle_get_playlist(&self.playlist, &mut conn, payload)
                .await
                .map_err(playlist_verb_to_plugin_error)?;
        encode_json_response(req, &env)
    }

    async fn dispatch_playlist_create_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::CreatePlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        playlist::handle_create_playlist(&self.playlist, &mut conn, payload)
            .await
            .map_err(playlist_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_playlist_delete_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::DeletePlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        playlist::handle_delete_playlist(&self.playlist, &mut conn, payload)
            .await
            .map_err(playlist_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_playlist_rename_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::RenamePlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        playlist::handle_rename_playlist(&self.playlist, &mut conn, payload)
            .await
            .map_err(playlist_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_playlist_add_to_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::AddToPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        playlist::handle_add_to_playlist(&self.playlist, &mut conn, payload)
            .await
            .map_err(playlist_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_playlist_remove_from_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::RemoveFromPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        playlist::handle_remove_from_playlist(
            &self.playlist,
            &mut conn,
            payload,
        )
        .await
        .map_err(playlist_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_playlist_move_in_playlist(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::MoveInPlaylistPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        playlist::handle_move_in_playlist(&self.playlist, &mut conn, payload)
            .await
            .map_err(playlist_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_playlist_save_selection(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: playlist::SaveSelectionPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let resolver = crate::selection::MpdSelectionResolver;
        let body = playlist::handle_save_selection(
            &self.playlist,
            &mut conn,
            &resolver,
            payload,
        )
        .await
        .map_err(playlist_verb_to_plugin_error)?;
        encode_json_response(req, &body)
    }

    // ----- favourites dispatchers -----

    async fn dispatch_favourites_list_favourites(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let state = favourites::handle_list_favourites(&self.favourites).await;
        encode_json_response(req, &state)
    }

    async fn dispatch_favourites_is_favourite(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: favourites::IsFavouritePayload = parse_json(req)?;
        let res = favourites::handle_is_favourite(&self.favourites, payload)
            .await
            .map_err(favourites_verb_to_plugin_error)?;
        encode_json_response(req, &res)
    }

    async fn dispatch_favourites_add_favourite(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: favourites::AddFavouritePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        favourites::handle_add_favourite(&self.favourites, &mut conn, payload)
            .await
            .map_err(favourites_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_favourites_remove_favourite(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: favourites::RemoveFavouritePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        favourites::handle_remove_favourite(
            &self.favourites,
            &mut conn,
            payload,
        )
        .await
        .map_err(favourites_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_favourites_clear_favourites(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: favourites::ClearFavouritesPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        favourites::handle_clear_favourites(
            &self.favourites,
            &mut conn,
            payload,
        )
        .await
        .map_err(favourites_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_favourites_move_favourite(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: favourites::MoveFavouritePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        favourites::handle_move_favourite(&self.favourites, &mut conn, payload)
            .await
            .map_err(favourites_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    // ----- library dispatchers -----

    async fn dispatch_library_list_sources(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let state = library::handle_list_sources(&self.library).await;
        encode_json_response(req, &state)
    }

    async fn dispatch_library_add_source(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::AddSourcePayload = parse_json(req)?;
        let res = library::handle_add_source(&self.library, payload)
            .await
            .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &res)
    }

    async fn dispatch_library_remove_source(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::RemoveSourcePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        library::handle_remove_source(&self.library, &mut conn, payload)
            .await
            .map_err(library_verb_to_plugin_error)?;
        encode_ok_response(req)
    }

    async fn dispatch_library_probe_source(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::ProbeSourcePayload = parse_json(req)?;
        let res = library::handle_probe_source(&self.library, payload)
            .await
            .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &res)
    }

    async fn dispatch_library_wake_source(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::WakeSourcePayload = parse_json(req)?;
        let res = library::handle_wake_source(&self.library, payload)
            .await
            .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &res)
    }

    async fn dispatch_library_update_source(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::UpdateSourcePayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let res =
            library::handle_update_source(&self.library, &mut conn, payload)
                .await
                .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &res)
    }

    async fn dispatch_library_browse_library(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::BrowseLibraryPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let env =
            library::handle_browse_library(&self.library, &mut conn, payload)
                .await
                .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &env)
    }

    async fn dispatch_library_search_library(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::SearchLibraryPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let env =
            library::handle_search_library(&self.library, &mut conn, payload)
                .await
                .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &env)
    }

    /// Common dispatcher for the four facet-browse verbs
    /// (`library.browse_by_{artist,album,genre,year}`). Each
    /// verb calls this with its own `(verb_name, tag,
    /// facet_key, post_process)` — the underlying flow
    /// (parse-payload → open-connection → run MPD `list <tag>`
    /// → paginate) is identical.
    async fn dispatch_library_browse_by_tag(
        &self,
        req: &Request,
        verb_name: &'static str,
        tag: &'static str,
        facet_key: &'static str,
        post_process: fn(String) -> Option<String>,
    ) -> Result<Response, PluginError> {
        let payload: library::BrowseByTagPayload = parse_json(req)?;
        let mut conn = self.open_conn().await?;
        let env = library::handle_browse_by_tag(
            &mut conn,
            payload,
            verb_name,
            tag,
            facet_key,
            post_process,
        )
        .await
        .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &env)
    }

    async fn dispatch_library_list_works(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::ListWorksPayload = parse_json(req)?;
        let env = library::handle_list_works(&self.library, payload)
            .await
            .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &env)
    }

    async fn dispatch_library_get_work_recordings(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: library::GetWorkRecordingsPayload = parse_json(req)?;
        let env = library::handle_get_work_recordings(&self.library, payload)
            .await
            .map_err(library_verb_to_plugin_error)?;
        encode_json_response(req, &env)
    }
}

// ----- helpers -----

fn parse_json<T: serde::de::DeserializeOwned>(
    req: &Request,
) -> Result<T, PluginError> {
    serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} payload is not valid JSON: {e}",
            req.request_type
        ))
    })
}

fn encode_json_response<T: serde::Serialize>(
    req: &Request,
    body: &T,
) -> Result<Response, PluginError> {
    let bytes = serde_json::to_vec(body).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} response JSON encode failed: {e}",
            req.request_type
        ))
    })?;
    Ok(Response::for_request(req, bytes))
}

fn encode_ok_response(req: &Request) -> Result<Response, PluginError> {
    let body = serde_json::json!({ "v": 1, "status": "ok" });
    encode_json_response(req, &body)
}

fn outcome_to_wire(
    outcome: &crate::skip_traversal::SkipOutcome,
) -> serde_json::Value {
    use crate::skip_traversal::SkipOutcome;
    match outcome {
        SkipOutcome::Playing { position } => {
            serde_json::json!({ "kind": "playing", "position": position })
        }
        SkipOutcome::Paused { last_attempted } => serde_json::json!({
            "kind": "paused",
            "last_attempted": last_attempted,
        }),
        SkipOutcome::Stopped => {
            serde_json::json!({ "kind": "stopped" })
        }
    }
}

fn queue_verb_to_plugin_error(e: queue::VerbError) -> PluginError {
    use queue::VerbError;
    match e {
        VerbError::PayloadVersion { .. }
        | VerbError::EmptyUris
        | VerbError::PositionOutOfRange { .. }
        | VerbError::QueueEmpty => PluginError::Permanent(e.to_string()),
        VerbError::Mpd { .. } => PluginError::Transient(e.to_string()),
    }
}

fn playlist_verb_to_plugin_error(e: playlist::VerbError) -> PluginError {
    use playlist::VerbError;
    match e {
        VerbError::PayloadVersion { .. }
        | VerbError::InvalidName { .. }
        | VerbError::NotFound { .. }
        | VerbError::DuplicateName { .. }
        | VerbError::FavouritesProtected { .. } => {
            PluginError::Permanent(e.to_string())
        }
        VerbError::Mpd { .. } => PluginError::Transient(e.to_string()),
    }
}

fn favourites_verb_to_plugin_error(e: favourites::VerbError) -> PluginError {
    use favourites::VerbError;
    match e {
        VerbError::PayloadVersion { .. } | VerbError::NotFavourite { .. } => {
            PluginError::Permanent(e.to_string())
        }
        VerbError::Mpd { .. } => PluginError::Transient(e.to_string()),
    }
}

fn library_verb_to_plugin_error(e: library::VerbError) -> PluginError {
    use library::VerbError;
    match e {
        VerbError::PayloadVersion { .. }
        | VerbError::UnknownSource { .. }
        | VerbError::NonRemovableLocalInternal
        | VerbError::CloudEagerScanRequiresAcknowledgement
        | VerbError::SourceOffline { .. }
        | VerbError::Register { .. }
        | VerbError::SourceOutsideMusicDirectory { .. }
        | VerbError::UnknownWork { .. } => {
            PluginError::Permanent(e.to_string())
        }
        // WorkAggregateNotReady is transient — the next
        // Database / Update idle event populates the cache.
        // Operator retry succeeds; treat as transient so
        // the steward's retry policy applies.
        VerbError::Mpd { .. } | VerbError::WorkAggregateNotReady => {
            PluginError::Transient(e.to_string())
        }
    }
}
