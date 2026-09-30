// Tier 2 native shell. UI lives in ui/app.slint; real data comes from
// lightify-core (the same cached Spotify session the shipped host uses).
//
// Threading: Slint's event loop owns the main thread (app.run()). A dedicated
// tokio thread runs the network worker and marshals results back to the UI with
// slint::Weak::upgrade_in_event_loop. UI → worker goes over an mpsc channel.
//
// Modes:
//   (default)         interactive window (winit + software renderer)
//   --shot <out.png>  render one frame headlessly to a PNG and exit (self-verify)
//
// Release builds run under the `windows` subsystem (matches the shipped Tauri
// host's own `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`)
// so a real double-click / shortcut launch never flashes a console window.
// This only gates Windows' auto-allocation of a *new* console when the process
// has no parent one — it does not affect stdio when a parent process explicitly
// redirects it (piping `--probe`/`--shot*`/`--selftest*` output still works fine,
// which is how every one of those flags is meant to be used and verified).
// `cargo run` (debug) keeps the console for interactive development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
slint::include_modules!();

mod audio_engine;
mod audio_output;
mod downloader;
mod engine;
#[cfg(windows)]
mod media_controls;
#[cfg(windows)]
mod single_instance;
#[cfg(windows)]
mod taskbar;
mod thumbs;
#[cfg(windows)]
mod tray;
mod visibility;

use std::rc::Rc;
use std::time::Duration;

use lightify_core::{BeatportTrack, Device, PlaybackState, SearchResults, Session, Track};
// `Model` brings row_count/row_data/set_row_data/as_any onto the ModelRc handles —
// how the presence marks are edited in place without replacing the model.
use slint::Model;

/// Commands from the UI thread to the async worker. `Clone` so the command
/// palette can re-inject a stored command when the user runs it.
#[derive(Clone)]
enum Cmd {
    /// A row click or menu action, stamped with the version of the list it was made
    /// against. See `unstamp` / "List versions".
    Stamped { surface: usize, version: u64, inner: Box<Cmd> },
    /// What a `Stamped` command becomes when its list changed underneath it.
    StaleClick,
    /// Sign-in page: Authorise (with the Client ID as typed), Retry (same, but while
    /// a sign-in is pending it re-opens the browser instead), and the dashboard link.
    SignInAuthorise(String),
    SignInRetry(String),
    SignInOpenDashboard,
    TogglePlay,
    Next,
    Prev,
    Drill(usize),   // open library row index (0 = Liked Songs, else playlist)
    Back,           // leave the drilled track list
    // Track-list row actions carry the row's uri as well as its display index. The
    // index alone went stale whenever a page load re-sorted the list between the
    // click and the worker getting to it (the default "recent" sort puts the next
    // page's newer tracks on top, shifting every row by ~100) — the click then
    // played a track far from the one clicked. See `resolve_track_row`.
    PlayTrack { row: usize, uri: String },
    Search(String),
    SearchFilter(String),
    OpenSearch(usize),
    ToggleShuffle,
    CycleRepeat,
    AddQueue,
    PlayCurrentTrack,        // "Play" in the now-playing menu (restarts the track)
    ShareCurrentTrack,
    StartStation,            // #btn-station: song radio seeded from the current track
    StationFromUri(String),  // ...or from a track row's menu
    ToggleLike,         // save / un-save the current track in Liked Songs
    SortRecent,         // #btn-library-sort-recent (visible library surface)
    SortAlpha,          // #btn-library-sort-alpha (2nd press flips asc/desc)
    Seek(f32),          // 0..1 fraction of the current track
    SetVolume(f32),     // 0..1
    ToggleSidebar(i32), // 1=queue 2=recent 3=downloads (same mode toggles closed)
    CloseSidebar,
    ClearQueue, // `#btn-sidebar-clear` in QUEUE mode: reset the device queue to just the current track
    /// Internal-only, not exposed via any UI callback: re-reads `me/player/queue` into
    /// the sidebar if it's open in QUEUE mode. Self-sent, delayed, after a batch
    /// `me/player/queue` write — see the `QUEUE_SETTLE` comment. Spotify's own queue
    /// GET can otherwise return an incomplete snapshot immediately after a batch write
    /// (confirmed live), which reads as "the queue didn't fully load" even though every
    /// POST succeeded. `refresh_sidebar` already no-ops outside QUEUE mode, so this is
    /// always safe to fire regardless of what the sidebar is currently showing.
    RefreshQueueSidebar,
    /// Self-sent by `spawn_sidebar_fetch`'s spawned task once a QUEUE/RECENT network
    /// read finishes. Runs on its own `Session` (same reasoning as `BpRefillDone`) so
    /// opening or refreshing the sidebar never blocks the command loop from handling
    /// anything else meanwhile. `generation` is checked against the worker's own
    /// `sidebar_generation` before being applied — a result for a panel the user has
    /// since closed, switched away from, or re-triggered a newer fetch for is simply
    /// dropped rather than overwriting what's now showing.
    SidebarFetched { generation: u64, mode: i32, result: Result<Vec<Track>, String> },
    /// Per-row × in the QUEUE sidebar (index into `sidebar_tracks`, the current
    /// snapshot of `me/player/queue`). No Spotify/librespot primitive removes one
    /// specific queued item, so this clears the whole queue and re-adds every
    /// *other* track — see the handler for the real cost of that.
    RemoveQueuedTrack(usize),
    SidebarPlay(usize),
    BpEnsure,           // load the default chart the first time the tab is shown
    BpSelectGenre(usize),
    BpSetMode(String),  // tracks | hype | releases
    BpPlay(usize),
    BpOpenExternal,     // open the current chart URL in the default browser
    /// Self-sent by `bp_refill_background`'s spawned task once a refill pass
    /// finishes. Runs on its own `Session` (see the function's doc comment) so the
    /// command loop stays responsive to everything else — clicking a sidebar,
    /// searching, anything — while match+queue network calls are in flight, instead
    /// of the whole app pausing for however long a batch of them takes.
    BpRefillDone {
        seq_id: u64,
        queued: Vec<(String, String)>, // (uri, id) pairs, in order, successfully queued
        seen: std::collections::HashSet<String>,
        next_index: usize,
        completed: bool,
        error: Option<String>,
    },
    OpenSettings,
    RefreshDevices,
    SelectDevice(usize),
    SetPollRate(i32),   // poll interval in ms
    CollapseLeft,       // toggle the left panel
    PaletteOpen,        // (re)build the command list and show the empty-query rows
    PaletteQuery(String),
    PaletteRun(usize),  // run the command at this filtered-row index
    PaletteSearch(String), // "Search for …" — switch to Search tab + run the query
    SetMiniMode(String),   // enter/switch/exit a mini-player mode (from the palette)
    /// One line of stdout from the bundled playback engine, forwarded onto this
    /// same channel so the worker handles device/transport news in one place.
    Engine(engine::Event),
    RestartEngine,
    SelectOutput(usize), // pick the engine's audio output (row 0 = system default)
    /// Self-sent after a downloader sign-in attempt finishes (success, failure, or
    /// timeout) — see the handler for why. Always safe to fire: it only acts when
    /// the engine has actually stopped running.
    ReclaimPlaybackAfterDownloaderLogin,
    // ── Row context menus (`.ctx-menu`) ──
    OpenContext { kind: i32, index: usize }, // kind 0 = library row, 1 = drilled track
    RunContext(usize),        // run the menu row at this index
    PlayRow(usize),           // play a whole library row (double-click / menu "Play")
    FollowPlaylist(usize),    // "Save to library"
    ConfirmDeletePlaylist(usize), // "Delete" → open the confirm dialog
    // …and what the dialog's Yes runs: (playlist id, name), resolved when the dialog
    // opened. NOT a row index — the delete runs later, after any re-sort or library
    // reload in between, and a row index would then unfollow a different playlist.
    DeletePlaylist(String, String),
    ConfirmAccept,            // the dialog's Yes → run whatever it was armed with
    QueueTrack { row: usize, uri: String },        // "Add to queue" on a drilled track row
    ShareTrack(usize),        // "Share" → copy the open.spotify.com link
    /// A menu row that exists in the shipped app but not here (host-only download
    /// bridge, unported station): say so in the status bar instead of doing nothing.
    Note(String),
    // ── Track-list multi-select ──
    /// Ctrl+click toggles one row; Shift+click extends from the last clicked row.
    /// `scope` is the surface the row belongs to (see `SEL_*`); selecting in a new
    /// surface drops the old selection, like the original's per-container scopes.
    SelectClick { scope: i32, row: usize, ctrl: bool, shift: bool },
    ClearSelection,
    PlaySelection,
    QueueSelection,
    CreatePlaylistFromSelection,
    // ── Search result menus ──
    QueueSearchTrack(usize),
    LikeSearchTrack(usize),
    ShareSearchTrack(usize),
    PlaySearchContext(usize), // play a playlist/album hit from its first track
    SaveSearchItem(usize),    // follow a playlist / save an album
    // ── Beatport menus ──
    BpQueue(usize),
    BpPlaySelection,
    BpQueueSelection,
    BpCreatePlaylist,
    BpSelectAll,
    LoadMoreTracks, // the drilled track list scrolled near its end
    // ── Search tab drill-in (stays inside the search tab) ──
    SearchBack,
    PlaySearchDrillTrack(usize),
    QueueSearchDrillTrack(usize),
    ShareSearchDrillTrack(usize),
    LoadMoreSearchTracks,
    LoadMoreSearchResults, // more playlist hits (PLAYLISTS filter only)
    // ── Downloads (the bundled OnTheSpot bridge, `src/downloader.rs`) ──
    /// "Download" on anything that resolves to a plain open.spotify.com URL. The
    /// second field is the human label the status line echoes back.
    Download { url: String, label: String },
    /// "Download" on a left-panel library row (0 = Liked Songs, else a playlist) —
    /// resolved here because the worker owns the playlist vector.
    DownloadLibraryRow(usize),
    /// "Download" on a search-result row — resolved against `search_actions`.
    DownloadSearchItem(usize),
    /// "Download" on a Beatport row: matched to a Spotify track first, exactly like
    /// `BpPlay`/`BpQueue` do (`downloadBeatportTrack`, app.js:3967).
    DownloadBeatportRow(usize),
    /// Re-read the bridge's queue into the DOWNLOADS sidebar.
    RefreshDownloads,
    /// `#btn-sidebar-clear` in DOWNLOADS mode: drop every finished row.
    ClearDownloads,
    /// Per-row menu actions; the index is into the last pushed downloads snapshot.
    // Download row actions carry the item's own `local_id` (and path), captured when
    // the menu was built. The list is re-read and re-sorted every poll while the menu
    // is open, so a row index could resolve to a different item by click time —
    // "Delete file" then deleted some other track's file.
    DownloadRetry(String),
    DownloadCancel(String),
    DownloadDelete(String),
    DownloadOpenFolder(String),
    /// Settings: connect the downloader's own Spotify account, and set where it writes.
    DownloaderLogin,
    SetDownloadPath(String),
    SidebarCreatePlaylist, // #btn-sidebar-create-pl (the whole queue, not a selection)
    // ── Sidebar menus ──
    SidebarQueueRow(usize),
    ShareSidebarTrack(usize),
    SidebarPlayFirstSelected,
    // ── Type-to-filter (D7): the visible left list's filter text ──
    ListFilter(String),
    // ── Search: Enter in the box opens the top result ──
    SearchEnter,
}

/// What clicking a search-result row should do (parallel to the displayed rows).
#[derive(Clone)]
enum SearchAction {
    Track { uri: String, id: String },
    Playlist { id: String, uri: String, name: String },
    Album { id: String, uri: String, name: String },
    Artist { id: String, name: String },
    /// A section title; clicking it switches to that filter ("See all").
    Header { see_all: String },
    /// A past query; clicking it runs it again.
    Recent(String),
    /// The "Recent searches" title; its link clears the list.
    ClearRecents,
}


fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--shot") {
        // --shot <out.png> [WxH]
        // The optional size matters for anything whose position is derived from the
        // window rather than fixed - the now-playing glow halo tracks the artwork, and
        // a single hard-coded size cannot show that it still lines up when the pane
        // grows or shrinks.
        let path = args.get(pos + 1).cloned().unwrap_or_else(|| "shot.png".into());
        let (w, h) = args
            .get(pos + 2)
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .unwrap_or((980u32, 660u32));
        render_to_png(&path, w.clamp(320, 4096), h.clamp(240, 4096), None);
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-live") {
        // --shot-live <out.png> [alpha|alpha-desc]
        let path = args.get(pos + 1).cloned().unwrap_or_else(|| "shot-live.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let mut snap = rt.block_on(fetch_snapshot());
        snap.sort = parse_sort_arg(args.get(pos + 2));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-drill") {
        // --shot-drill <row-index> <out.png> [alpha|alpha-desc]  (0 = Liked Songs)
        let index: usize = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(1);
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-drill.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let mut snap = rt.block_on(fetch_drill_snapshot(index));
        snap.sort = parse_sort_arg(args.get(pos + 3));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-search") {
        let query = args.get(pos + 1).cloned().unwrap_or_else(|| "test".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-search.png".into());
        // Optional filter: all | tracks | artists | albums | playlists
        let filter = args.get(pos + 3).cloned().unwrap_or_else(|| "all".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        if let Ok(mut g) = SHOT_SEARCH.lock() {
            *g = Some((query.clone(), filter.clone()));
        }
        let snap = rt.block_on(fetch_search_snapshot(&query, &filter));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-sidebar") {
        // --shot-sidebar <mode> <out.png>  (1=queue 2=recent 3=downloads)
        let mode: i32 = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(1);
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-sidebar.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_sidebar_snapshot(mode));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-beatport") {
        // --shot-beatport <kind> <out.png>  (kind = tracks | hype | releases)
        let kind = args.get(pos + 1).cloned().unwrap_or_else(|| "tracks".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-beatport.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_beatport_snapshot(&kind));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-palette") {
        let path = args.get(pos + 1).cloned().unwrap_or_else(|| "shot-palette.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_palette_snapshot());
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-search-drill") {
        // --shot-search-drill <query> <out.png>
        let query = args.get(pos + 1).cloned().unwrap_or_else(|| "m83".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-search-drill.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_search_drill_snapshot(&query));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-ctx") {
        // --shot-ctx <kind> <out.png>  (kind = library | liked | track | confirm)
        let kind = args.get(pos + 1).cloned().unwrap_or_else(|| "library".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-ctx.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_ctx_snapshot(&kind));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-settings") {
        let path = args.get(pos + 1).cloned().unwrap_or_else(|| "shot-settings.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_settings_snapshot());
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-mini") {
        // --shot-mini <mode> <out.png>  (mode = square | bar | nano)
        let mode = args.get(pos + 1).cloned().unwrap_or_else(|| "square".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-mini.png".into());
        let (w, h) = mini_size(&mode);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_mini_snapshot(&mode));
        render_to_png(&path, w as u32, h as u32, Some(snap));
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe());
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--probe-search") {
        // --probe-search <query>: which relevance fields this token's search returns.
        let q = args.get(pos + 1).cloned().unwrap_or_else(|| "toby".into());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(probe_search(&q));
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe-downloader") {
        // Start the bridge, then let the idle monitor stop it (set
        // LIGHTIFY_DOWNLOADER_IDLE_SECS to keep this short) and report.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        let t0 = std::time::Instant::now();
        match rt.block_on(downloader::status()) {
            Ok(st) => println!("bridge up in {:.1}s: {}", t0.elapsed().as_secs_f32(), st.summary()),
            Err(e) => return Ok(println!("bridge failed: {e}")),
        }
        let t1 = std::time::Instant::now();
        while downloader::running() && t1.elapsed() < Duration::from_secs(900) {
            std::thread::sleep(Duration::from_millis(250));
        }
        println!("bridge stopped by the idle monitor after {:.1}s: {}", t1.elapsed().as_secs_f32(), !downloader::running());
        std::thread::sleep(Duration::from_secs(3)); // let the temp-folder cleanup finish
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe-bp") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_beatport());
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--probe-clear-sync") {
        // --probe-clear-sync <seek|play> [paused]
        let strategy = args.get(pos + 1).cloned().unwrap_or_else(|| "seek".into());
        let paused = args.iter().any(|a| a == "paused");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(probe_clear_sync(&strategy, paused));
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-queue-order") {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(selftest_queue_order());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-station-clear") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(selftest_station_clear());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-queue") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(selftest_queue_authority());
        engine::shutdown();
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--probe-station") {
        // --probe-station <track uri or id>
        let seed = args.get(pos + 1).cloned().unwrap_or_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_station(&seed));
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe-engine") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_engine());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe-queue") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_queue());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe-output") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_output());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe-play") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_play());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-paging") {
        selftest_paging();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-menus") {
        selftest_menus();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-dismiss") {
        selftest_dismiss();
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--selftest-filter") {
        selftest_filter(args.get(pos + 1).map(String::as_str));
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-fade") {
        selftest_fade();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-presence") {
        selftest_presence();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-sidebar") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(selftest_sidebar());
        return Ok(());
    }

    // One interactive instance per user: a second launch focuses the first window
    // and exits instead of registering a second Connect device.
    #[cfg(windows)]
    let _single = match single_instance::claim() {
        Some(g) => g,
        None => return Ok(()),
    };

    let app = MainWindow::new()?;
    #[cfg(windows)]
    single_instance::listen(app.as_weak());
    app.set_app_version(env!("CARGO_PKG_VERSION").into());
    app.set_ui_font(ui_font().into());
    app.global::<Motion>().set_reduced(reduce_motion());

    // Recall the last mini-player layout for the titlebar toggle (persisted on disk).
    app.set_mini_last(load_mini_mode().into());

    // UI → worker channel.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();

    app.on_close_clicked(|| {
        let _ = slint::quit_event_loop();
    });
    {
        // Mini-player: relax the min-size, resize + always-on-top, persist the layout.
        let weak = app.as_weak();
        app.on_set_mini_mode(move |mode| {
            if let Some(app) = weak.upgrade() {
                apply_mini_mode(&app, &mode.to_string());
            }
        });
    }
    {
        // Native OS window move for the no-frame window (winit `drag_window`).
        let weak = app.as_weak();
        app.on_start_drag(move || {
            use slint::winit_030::WinitWindowAccessor;
            if let Some(a) = weak.upgrade() {
                a.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_toggle_left_panel(move || {
            if let Some(a) = weak.upgrade() {
                a.set_left_collapsed(!a.get_left_collapsed());
            }
        });
    }
    // Type-to-filter (D7).
    install_filter_helpers(&app);
    {
        let tx = tx.clone();
        app.on_search_enter(move || {
            let _ = tx.send(Cmd::SearchEnter);
        });
    }
    // Keep the cover cache on disk bounded (search adds covers all the time).
    thumbs::prune_disk_cache();
    {
        let tx = tx.clone();
        app.on_list_filter_edited(move |t| {
            let _ = tx.send(Cmd::ListFilter(t.to_string()));
        });
    }
    {
        let tx = tx.clone();
        app.on_toggle_play(move || {
            let _ = tx.send(Cmd::TogglePlay);
        });
    }
    {
        let tx = tx.clone();
        app.on_next_clicked(move || {
            let _ = tx.send(Cmd::Next);
        });
    }
    {
        let tx = tx.clone();
        app.on_prev_clicked(move || {
            let _ = tx.send(Cmd::Prev);
        });
    }
    {
        let tx = tx.clone();
        app.on_open_row(move |i| {
            let _ = tx.send(stamp_shown(0, Cmd::Drill(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        app.on_back_clicked(move || {
            let _ = tx.send(Cmd::Back);
        });
    }
    {
        let tx = tx.clone();
        let w = app.as_weak();
        app.on_play_track(move |i| {
            let row = i.max(0) as usize;
            // Read the uri on the UI thread, from the very model that was clicked.
            let uri = w
                .upgrade()
                .and_then(|a| slint::Model::row_data(&a.get_tracks(), row))
                .map(|t| t.uri.to_string())
                .unwrap_or_default();
            let _ = tx.send(Cmd::PlayTrack { row, uri });
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_select_tab(move |i| {
            if let Some(a) = weak.upgrade() {
                a.set_tab(i);
            }
            if i == 2 {
                let _ = tx.send(Cmd::BpEnsure);
            }
        });
    }
    {
        let tx = tx.clone();
        app.on_query_changed(move |q| {
            let _ = tx.send(Cmd::Search(q.to_string()));
        });
    }
    {
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_set_search_filter(move |f| {
            if let Some(a) = weak.upgrade() {
                a.set_search_filter(f.clone());
            }
            let _ = tx.send(Cmd::SearchFilter(f.to_string()));
        });
    }
    {
        let tx = tx.clone();
        app.on_open_search(move |i| {
            let _ = tx.send(stamp_shown(SEL_SEARCH as usize, Cmd::OpenSearch(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        app.on_toggle_shuffle(move || {
            let _ = tx.send(Cmd::ToggleShuffle);
        });
    }
    {
        let tx = tx.clone();
        app.on_cycle_repeat(move || {
            let _ = tx.send(Cmd::CycleRepeat);
        });
    }
    {
        let tx = tx.clone();
        app.on_add_queue(move || {
            let _ = tx.send(Cmd::AddQueue);
        });
    }
    {
        let tx = tx.clone();
        app.on_start_station(move || {
            let _ = tx.send(Cmd::StartStation);
        });
    }
    {
        let tx = tx.clone();
        app.on_restart_engine(move || {
            let _ = tx.send(Cmd::RestartEngine);
        });
    }
    {
        let tx = tx.clone();
        app.on_settings_select_output(move |i| {
            let _ = tx.send(Cmd::SelectOutput(i.max(0) as usize));
        });
    }
    {
        let tx = tx.clone();
        app.on_toggle_like(move || {
            let _ = tx.send(Cmd::ToggleLike);
        });
    }
    {
        let tx = tx.clone();
        app.on_sort_recent(move || {
            let _ = tx.send(Cmd::SortRecent);
        });
    }
    {
        let tx = tx.clone();
        app.on_sort_alpha(move || {
            let _ = tx.send(Cmd::SortAlpha);
        });
    }
    {
        let tx = tx.clone();
        app.on_seek(move |f| {
            let _ = tx.send(Cmd::Seek(f));
        });
    }
    {
        // Optimistically reflect the drag immediately, then commit to the API.
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_set_volume(move |f| {
            if let Some(a) = weak.upgrade() {
                let v = f.clamp(0.0, 1.0);
                a.set_volume(v);
                // Hold the slider at what the user chose until the service agrees.
                a.set_volume_target(v);
                a.set_volume_pending(true);
            }
            let _ = tx.send(Cmd::SetVolume(f));
        });
    }
    {
        let tx = tx.clone();
        app.on_toggle_sidebar(move |m| {
            let _ = tx.send(Cmd::ToggleSidebar(m));
        });
    }
    {
        let tx = tx.clone();
        app.on_close_sidebar(move || {
            let _ = tx.send(Cmd::CloseSidebar);
        });
    }
    {
        // Right-before-left, one overlay per click (`app.js:1668`). The right sidebar
        // is the worker's state, so that half is a command; the left panel's collapse
        // flag lives in the UI and is set here directly, exactly as the "L" mark does.
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_dismiss_overlays(move || {
            let Some(a) = weak.upgrade() else { return };
            if a.get_sidebar_mode() != 0 {
                let _ = tx.send(Cmd::CloseSidebar);
            } else if !a.get_left_collapsed() {
                a.set_left_collapsed(true);
            }
        });
    }
    {
        let tx = tx.clone();
        app.on_clear_queue(move || {
            let _ = tx.send(Cmd::ClearQueue);
        });
    }
    {
        let tx = tx.clone();
        app.on_clear_downloads(move || {
            let _ = tx.send(Cmd::ClearDownloads);
        });
    }
    {
        let tx = tx.clone();
        app.on_settings_start_downloader_login(move || {
            let _ = tx.send(Cmd::DownloaderLogin);
        });
    }
    {
        let tx = tx.clone();
        app.on_settings_apply_download_path(move |path| {
            let _ = tx.send(Cmd::SetDownloadPath(path.to_string()));
        });
    }
    {
        let tx = tx.clone();
        app.on_remove_queued_track(move |i| {
            let _ = tx.send(stamp_shown(SEL_SIDEBAR as usize, Cmd::RemoveQueuedTrack(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        let w = app.as_weak();
        app.on_sidebar_play(move |i| {
            // DOWNLOADS shares this callback but is its own list.
            let surface = match w.upgrade().map(|a| a.get_sidebar_mode()) {
                Some(3) => LIST_DOWNLOADS,
                _ => SEL_SIDEBAR as usize,
            };
            let _ = tx.send(stamp_shown(surface, Cmd::SidebarPlay(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        app.on_bp_ensure(move || {
            let _ = tx.send(Cmd::BpEnsure);
        });
    }
    {
        // Pure UI toggle of the genre picker (no worker involvement).
        let weak = app.as_weak();
        app.on_bp_toggle_genre(move || {
            if let Some(a) = weak.upgrade() {
                a.set_bp_genre_open(!a.get_bp_genre_open());
            }
        });
    }
    {
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_bp_select_genre(move |i| {
            if let Some(a) = weak.upgrade() {
                a.set_bp_genre_open(false);
            }
            let _ = tx.send(Cmd::BpSelectGenre(i.max(0) as usize));
        });
    }
    {
        let tx = tx.clone();
        app.on_bp_set_mode(move |k| {
            let _ = tx.send(Cmd::BpSetMode(k.to_string()));
        });
    }
    {
        let tx = tx.clone();
        app.on_bp_play(move |i| {
            let _ = tx.send(stamp_shown(SEL_BEATPORT as usize, Cmd::BpPlay(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        app.on_bp_open_external(move || {
            let _ = tx.send(Cmd::BpOpenExternal);
        });
    }
    {
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_open_settings(move || {
            note_settings_open(true);
            if let Some(a) = weak.upgrade() {
                a.set_settings_open(true);
            }
            let _ = tx.send(Cmd::OpenSettings);
        });
    }
    {
        let weak = app.as_weak();
        app.on_close_settings(move || {
            note_settings_open(false);
            if let Some(a) = weak.upgrade() {
                a.set_settings_open(false);
            }
        });
    }
    {
        let tx = tx.clone();
        app.on_settings_refresh_devices(move || {
            let _ = tx.send(Cmd::RefreshDevices);
        });
    }
    {
        let tx = tx.clone();
        app.on_settings_select_device(move |i| {
            let _ = tx.send(Cmd::SelectDevice(i.max(0) as usize));
        });
    }
    {
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_set_poll_rate(move |ms| {
            if let Some(a) = weak.upgrade() {
                a.set_poll_ms(ms);
            }
            let _ = tx.send(Cmd::SetPollRate(ms));
        });
    }
    {
        let tx = tx.clone();
        app.on_load_palette(move || {
            let _ = tx.send(Cmd::PaletteOpen);
        });
    }
    // ── Row context menus. The worker owns the playlist / track vectors, so it
    // builds the item list and keeps the parallel action list (like the palette).
    {
        let tx = tx.clone();
        app.on_open_context(move |kind, index| {
            let cmd = Cmd::OpenContext { kind, index: index.max(0) as usize };
            // Kind 6 is the now-playing menu: no list behind it.
            let cmd = if (0..LIST_SURFACES as i32).contains(&kind) && kind != 6 {
                stamp_shown(kind as usize, cmd)
            } else {
                cmd
            };
            let _ = tx.send(cmd);
        });
    }
    {
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_run_context(move |i| {
            if let Some(a) = weak.upgrade() {
                a.set_ctx_open(false);
            }
            let _ = tx.send(Cmd::RunContext(i.max(0) as usize));
        });
    }
    {
        let tx = tx.clone();
        app.on_play_row(move |i| {
            let _ = tx.send(stamp_shown(0, Cmd::PlayRow(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        app.on_confirm_accept(move || {
            let _ = tx.send(Cmd::ConfirmAccept);
        });
    }
    {
        let tx = tx.clone();
        app.on_signin_authorise(move |cid| {
            let _ = tx.send(Cmd::SignInAuthorise(cid.to_string()));
        });
    }
    {
        let tx = tx.clone();
        let w = app.as_weak();
        app.on_signin_retry(move || {
            let cid = w.upgrade().map(|a| a.get_signin_client_id().to_string()).unwrap_or_default();
            let _ = tx.send(Cmd::SignInRetry(cid));
        });
    }
    {
        let tx = tx.clone();
        app.on_signin_open_dashboard(move || {
            let _ = tx.send(Cmd::SignInOpenDashboard);
        });
    }
    {
        let tx = tx.clone();
        app.on_select_click(move |scope, row, ctrl, shift| {
            let cmd = Cmd::SelectClick { scope, row: row.max(0) as usize, ctrl, shift };
            let _ = tx.send(if (0..LIST_SURFACES as i32).contains(&scope) {
                stamp_shown(scope as usize, cmd)
            } else {
                cmd
            });
        });
    }
    // Native Windows integration, all of which needs the native window — and winit
    // only creates that once the event loop has started, so this waits for it:
    //  * media controls (SMTC): now-playing in the system media flyout / lock screen,
    //    and the hardware media keys (Windows routes them to the active session).
    //    Only if SMTC is unavailable does the old global hotkey grab come back.
    //  * the taskbar thumbnail toolbar (Previous / Play-Pause / Next).
    #[cfg(windows)]
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        let _ = slint::spawn_local(async move {
            use slint::winit_030::WinitWindowAccessor;
            let Some(app) = weak.upgrade() else { return };
            let window = match app.window().winit_window().await {
                Ok(w) => w,
                Err(e) => {
                    media_controls::note(&format!("no native window ({e}); using global media-key hotkeys"));
                    spawn_media_key_listener(tx);
                    return;
                }
            };
            if let Some(hwnd) = media_controls::hwnd_of(&window) {
                // Minimized: stop pushing seek-bar frames, and trim the working set.
                visibility::attach(hwnd);
                if let Err(e) = taskbar::attach(hwnd, tx.clone()) {
                    eprintln!("[lightify] taskbar buttons unavailable: {e}");
                }
            }
            let mc = match media_controls::MediaControls::attach(&window, &app, tx.clone()) {
                Ok(mc) => Some(mc),
                Err(e) => {
                    media_controls::note(&format!("unavailable ({e}); using global media-key hotkeys"));
                    spawn_media_key_listener(tx);
                    None
                }
            };
            let sync = move |a: &MainWindow| {
                if let Some(mc) = mc.as_ref() {
                    mc.sync(a);
                }
                taskbar::set_playing(a.get_playing());
            };
            sync(&app);
            let weak = app.as_weak();
            app.on_media_changed(move || {
                if let Some(a) = weak.upgrade() {
                    sync(&a);
                }
            });
        });
    }
    #[cfg(not(windows))]
    spawn_media_key_listener(tx.clone());
    {
        let tx = tx.clone();
        app.on_load_more_tracks(move || {
            let _ = tx.send(Cmd::LoadMoreTracks);
        });
    }
    {
        let tx = tx.clone();
        app.on_search_back(move || {
            let _ = tx.send(Cmd::SearchBack);
        });
    }
    {
        let tx = tx.clone();
        app.on_play_search_track(move |i| {
            let _ = tx.send(stamp_shown(SEL_SEARCH_DRILL as usize, Cmd::PlaySearchDrillTrack(i.max(0) as usize)));
        });
    }
    {
        let tx = tx.clone();
        app.on_load_more_search_tracks(move || {
            let _ = tx.send(Cmd::LoadMoreSearchTracks);
        });
    }
    {
        let tx = tx.clone();
        app.on_load_more_search_results(move || {
            let _ = tx.send(Cmd::LoadMoreSearchResults);
        });
    }
    {
        let tx = tx.clone();
        app.on_bp_select_all(move || {
            let _ = tx.send(Cmd::BpSelectAll);
        });
    }
    {
        let tx = tx.clone();
        app.on_bp_create_playlist(move || {
            let _ = tx.send(Cmd::BpCreatePlaylist);
        });
    }
    {
        let tx = tx.clone();
        app.on_sidebar_create_playlist(move || {
            let _ = tx.send(Cmd::SidebarCreatePlaylist);
        });
    }
    {
        let tx = tx.clone();
        app.on_clear_selection(move || {
            let _ = tx.send(Cmd::ClearSelection);
        });
    }
    {
        let tx = tx.clone();
        app.on_cmdk_query_changed(move |q| {
            let _ = tx.send(Cmd::PaletteQuery(q.to_string()));
        });
    }
    {
        let tx = tx.clone();
        let weak = app.as_weak();
        app.on_run_palette(move || {
            if let Some(a) = weak.upgrade() {
                let idx = a.get_cmdk_selected().max(0) as usize;
                a.set_palette_open(false);
                let _ = tx.send(Cmd::PaletteRun(idx));
            }
        });
    }

    // Network worker on its own thread + tokio runtime. `self_tx` lets the worker
    // re-inject a command (used by the command palette to run a stored command).
    let weak = app.as_weak();
    let self_tx = tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(worker(weak, rx, self_tx));
    });

    // Notification-area icon. Created on the UI thread (Windows ties a tray icon to
    // the thread that owns it, and that thread must pump messages - which is what
    // `app.run()` below does). Held for the whole run: dropping it removes the icon.
    #[cfg(windows)]
    let _tray = tray::install(app.as_weak());

    let r = app.run();
    // The event loop returns here on quit and the worker thread is never unwound,
    // so this is the only place that can retire the engine — leave it running and
    // a phantom "Lightify" device stays advertised on the account.
    engine::shutdown();
    // Same reasoning for the downloader bridge: it is a child process of ours, and
    // leaving it running would keep the loopback port (and a Spotify Connect
    // advertisement of its own) alive after the window is gone.
    downloader::shutdown();
    r
}

async fn worker(
    weak: slint::Weak<MainWindow>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    self_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
) {
    // No usable session (a fresh install, or a sign-in Spotify has since revoked)
    // used to end the worker right here with "Not connected" and nothing to click.
    // Now the sign-in page takes over until there is one.
    // Stale-while-revalidate: the last library this machine saw goes up before the
    // session check and the network, so the list is there the moment the window is.
    // The fresh fetch below replaces it; the list-version guard turns a click that
    // lands on a row that moved in between into "try again", never the wrong row.
    let cached_library = load_library_cache();
    if let Some(pls) = cached_library.as_ref() {
        push_rows(&weak, build_rows(pls, &sort_playlist_view(pls, "recent", "asc"), "", false));
        thumbs::request(&weak, pls);
    }
    let Some(mut session) = obtain_session(&weak, &mut rx).await else { return };
    set_account(&weak, connected_status(&session));

    // Bring up our OWN Spotify Connect device before anything else. Lightify is a
    // player, not a remote: without this the Web API has no device to target and
    // every transport call dead-ends in "no active device" (which the UI used to
    // render as "open Spotify first" — the exact dependency this app exists to
    // remove). The engine reuses the shipped host's cached streaming credentials,
    // so this is silent.
    start_engine(&weak, &session, &self_tx);

    let mut playlists = match session.playlists().await {
        Ok(p) => {
            remember_library(&p);
            thumbs::request(&weak, &p);
            p
        }
        Err(e) => {
            set_status(&weak, format!("Playlists error — {e}"));
            // Offline or rate-limited: keep showing the cached library rather than
            // blanking the list the user can already see.
            cached_library.unwrap_or_default()
        }
    };
    // Library sort (#library-toolbar). The shipped app keeps a separate mode/direction
    // per surface (playlist list vs drilled track list); the toolbar shows the visible one.
    let mut lib_mode = String::from("recent");
    let mut lib_dir = String::from("asc");
    let mut trk_mode = String::from("recent");
    let mut trk_dir = String::from("asc");
    // Display order (indices into `playlists`) — row clicks map back through it.
    let mut pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
    // Which library row playback comes from, plus a cache of the last pushed
    // (source, playing) so the row model is only rebuilt when the highlight changes.
    let mut launched_src: Option<String> = None;
    let mut presence = Presence::default();
    push_rows(&weak, build_rows(&playlists, &pl_view, "", false));
    // Our own engine's Connect device id, learned from its `Ready` event. librespot
    // mints a fresh random UUID (`SessionConfig::default()`) on every single launch —
    // never persisted — so this is the only reliable way to tell "the device our own
    // IPC pipe controls" apart from any OTHER device that happens to share the
    // "Lightify" display name (a previous run's session Spotify hasn't timed out yet,
    // notably). See `engine_owns_playback`.
    let mut engine_device_id = String::new();
    // The track name our engine last announced (`Event::Track`). During a rate-limit
    // backoff `last` can't be refreshed, so it may still describe the previous song —
    // see `last_is_stale`.
    let mut engine_track_name = String::new();
    // Guards `restore_queue` to run once per launch — the engine can come back up
    // again later (the watchdog restarting it after a lost sink), and re-queuing
    // the same persisted tracks a second time would duplicate them.
    let mut queue_restored = false;

    // Drill-in state (the currently-open track list + its playback context).
    let mut tracks: Vec<Track> = Vec::new();
    let mut trk_view: Vec<usize> = Vec::new();
    let mut context: Option<String> = None;
    let mut drilled = false;
    // The library row the open track list came from ("liked" or a playlist id).
    let mut drill_src: Option<String> = None;
    // Infinite-scroll pager for that list (the host's `state.activeTrackPager`).
    let mut pager = TrackPager::default();
    // The search tab's own drill-in (`#search-drill`): its own tracks, context and
    // pager, so drilling a search hit never disturbs the library list.
    let mut s_tracks: Vec<Track> = Vec::new();
    let mut s_context: Option<String> = None;
    let mut s_pager = TrackPager::default();
    let mut engine_watchdog = EngineWatchdog::new();
    // Follows Windows' own default output device live (Spotify and most normal
    // apps do this; the engine's sink is resolved once at startup and otherwise
    // has no way to notice). Only relevant while the config is "system default"
    // (empty) — a user-pinned device should never be silently overridden. This is
    // a *different* case from `engine_watchdog`'s recovery above: that one fires
    // only after the sink has actually died (device removed); this fires when the
    // OS default is simply reassigned while the current device is still present
    // and the stream is still happily running, which never crashes anything.
    let mut system_default_device = engine::audio_outputs().first().cloned().unwrap_or_default();
    // Station lane: on while a station owns "what plays next", plus the uris we put in
    // the device queue so leftovers can be recognised later.
    let mut station_active = false;
    let mut station_queued: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Playlist-hit pager for the results list (PLAYLISTS filter only, like the host).
    let mut sp_offset: u32 = 0;
    let mut sp_done: bool = true;
    // The rows/actions currently displayed, so a page can be appended without
    // re-running the search. The Search tab opens on recent searches.
    let (recent_rows, recent_actions) = build_recents(&load_recent_searches());
    push_search(&weak, recent_rows.clone());
    let mut search_rows: Vec<SearchRow> = recent_rows;
    let mut search_actions: Vec<SearchAction> = recent_actions;

    // Search state (+ a debounce deadline so we don't hit the API on every keystroke).
    let mut search_results = SearchResults::default();
    let mut search_filter = String::from("all");
    let mut pending_query: Option<String> = None;
    // The query the displayed results came from — the playlist pager keeps asking
    // for more of it (the original guards its pager on `state.searchQuery`).
    let mut last_query = String::new();
    let mut search_deadline: Option<tokio::time::Instant> = None;

    // Sidebar overlay state (which mode is open + its current track list).
    let mut sidebar_mode: i32 = 0;
    let mut sidebar_tracks: Vec<Track> = Vec::new();
    // Bumped on every dispatched `spawn_sidebar_fetch`; a `Cmd::SidebarFetched` whose
    // `generation` doesn't match the current value is stale (the panel was closed,
    // switched, or refreshed again before this fetch returned) and gets dropped
    // rather than clobbering whatever the sidebar has moved on to since.
    let mut sidebar_generation: u64 = 0;

    // Beatport tab state (genres are static; the chart loads on first tab open).
    let bp_genres = lightify_core::beatport_genres();
    let mut bp_idx: usize = 0;
    let mut bp_kind = String::from("tracks");
    let mut bp_loaded = false;
    let mut bp_tracks: Vec<BeatportTrack> = Vec::new();
    // Live Beatport chart autoplay, if a chart row started playback.
    let mut bp_autoplay: Option<BpAutoplay> = None;
    // Assigns each new BpAutoplay's `id` — see its doc comment.
    let mut bp_autoplay_next_id: u64 = 0;
    // The `id` of whichever BpAutoplay is currently authoritative, or 0 for none.
    // Shared (not channel-passed) with every spawned `bp_refill_background` task so
    // it can notice mid-flight that its sequence was abandoned — set synchronously,
    // in the same statement that resets `bp_autoplay`, so there is no window where a
    // background task could read a seq id we have already moved on from. See
    // `bp_refill_background`'s doc comment for why a channel message alone isn't
    // enough here.
    let bp_active_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Uris `bp_refill`/`bp_refill_background` has queued onto the real device queue
    // while a chart plays — exactly `station_queued`'s role, for the same reason: a
    // chart queues rows ahead the same way a station does, and its leftovers need
    // the same cleanup when the user moves on. See `end_autoplay_authority`.
    let mut bp_queued: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Up-next as the user sees it, when a list started playback. See `QueueMirror`.
    let mut queue_mirror: Option<QueueMirror> = None;
    // How long the slider is held at the user's choice before we accept whatever the
    // service reports. Long enough to cover a slow echo, short enough that a volume
    // changed on another device still shows up promptly.
    const VOLUME_SETTLE: Duration = Duration::from_secs(3);
    let mut volume_pending_until: Option<tokio::time::Instant> = None;
    push_bp_genres(&weak, &bp_genres);

    // Settings state (device picker + configurable poll interval).
    // Default 5s (SLOW) to keep steady-state Web-API pressure low — the dev-mode quota
    // is shared across clients; the user can pick 3s/1s in Settings. See [[dev-mode-api-quota-shared]].
    let mut devices: Vec<Device> = Vec::new();
    let mut poll_ms: u64 = 5000;

    // Command-palette state (the command list parallel to the displayed rows).
    let mut palette_actions: Vec<Cmd> = Vec::new();
    // Row-menu state: the actions parallel to the rows currently in `ctx-items`,
    // plus whatever the confirm dialog is armed with (only "Delete playlist" so far).
    let mut ctx_actions: Vec<Cmd> = Vec::new();
    let mut pending_confirm: Option<Cmd> = None;
    // Track-list selection, held as indices into `tracks` (NOT display rows), so a
    // re-sort keeps the same songs selected — the original keys off row identity too.
    let mut selected: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    // Which surface the selection belongs to (0 = nothing selected). Only one
    // surface holds a selection at a time, like the original's scoped registry.
    let mut sel_scope: i32 = SEL_NONE;
    // The row Shift+click extends from (a display index).
    let mut sel_anchor: Option<usize> = None;
    // (scope, list version) the current selection was made against — see the check
    // at the bottom of the loop.
    let mut sel_version: (i32, u64) = (SEL_NONE, 0);

    let mut last: Option<PlaybackState> = None;
    let mut last_art = String::new();
    let mut liked = LikeState::default();
    // When Spotify rate-limits us (429), pause polling until this instant instead of
    // hammering the endpoint every tick (which only prolongs the limit).
    let mut backoff_until: Option<tokio::time::Instant> = None;
    // Track id the queue sidebar was last fetched for — the queue only advances when the
    // track changes, so we skip the extra me/player/queue call on every other tick.
    // When a batch of tracks was just POSTed to `me/player/queue` (queueing a whole
    // selection/playlist), Spotify's own queue ordering is briefly not settled server-side
    // yet — confirmed live: calling `me/player/next` in that window can skip an *extra*
    // track (lands on the queue's 2nd item, not its 1st), even though a single `next()`
    // call was made. Not fixable client-side beyond reducing how often the race is hit:
    // `Cmd::Next` waits out the rest of `QUEUE_SETTLE` below if it fires inside that window.
    let mut last_bulk_queue_write: Option<tokio::time::Instant> = None;
    if let Some(secs) = refresh_playback(&weak, &mut session, &mut last, &mut last_art, &mut liked).await {
        backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
        set_status(&weak, format!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
    } else if last.is_none() {
        // Nothing is playing anywhere on the account: offer the last track, paused at
        // where it stopped (UI-PLAN D3). Only an offer — nothing plays until the user
        // presses Play, and it never appears while another device is playing.
        if let Some(rp) = load_resume() {
            offer_resume(&weak, &mut session, &rp, &mut last_art).await;
        }
    }

    // Every interval here uses `Delay`, not tokio's default `Burst`: most commands
    // are awaited inline, so a slow one (a 50-track queue loop, a big playlist page)
    // used to be followed by a *burst* of catch-up ticks — several back-to-back
    // `me/player` polls and a stack of queue fetches, all at once, exactly when the
    // rolling rate window was already fullest.
    let mut ticker = poll_interval(Duration::from_millis(poll_ms));
    ticker.tick().await; // consume the immediate first tick
    // The playback poll no longer runs on every tick: `playback_poll_every` stretches
    // it when our own engine is already reporting state, or when nothing is playing.
    let mut next_playback_poll = tokio::time::Instant::now();
    // When this app last paused our own engine itself. A pause the engine reports
    // that we did NOT ask for is usually Spotify moving playback to another device
    // (a handoff), which the stretched engine-owned poll would otherwise take up to
    // `ENGINE_OWNED_POLL` to notice. See `engine::Event::Playing`.
    let mut local_pause_at: Option<tokio::time::Instant> = None;
    // A one-off playback poll due at this instant, independent of the tick cadence.
    let mut poll_soon: Option<tokio::time::Instant> = None;
    // Local progress clock — see `ProgressClock`.
    let mut clock = poll_interval(PROGRESS_CLOCK);
    clock.tick().await;
    let mut progress_clock = ProgressClock::default();

    // The QUEUE panel gets its own, faster beat. Riding the playback poll meant a
    // queued track could take a full interval to appear - up to 5s on the default
    // SLOW setting, and longer once the batch-write settle is added on top, which is
    // the "takes about 10 seconds to see the items" report.
    //
    // Deliberately cheap: it only does anything while the panel is actually open on
    // QUEUE, and when a `QueueMirror` owns the panel it costs no network at all. So
    // the extra Web-API traffic is one `me/player/queue` GET every 1.5s, only while
    // the user is looking at a Spotify-owned queue, and none the rest of the time.
    const QUEUE_POLL: Duration = Duration::from_millis(1500);
    let mut queue_ticker = poll_interval(QUEUE_POLL);
    queue_ticker.tick().await;
    // Adaptive queue polling: every fetch that comes back unchanged doubles the wait
    // (1.5 → 3 → 6 → 12 s); a track change or a local queue write snaps it back.
    // The fetch only happens at all when no `QueueMirror` can answer locally, i.e.
    // mostly an *empty* queue — which used to be re-fetched every 1.5 s forever.
    let mut queue_quiet: u32 = 0;
    // Offline handling (UI-PLAN D8): while Spotify can't be reached the playback poll
    // becomes a probe on a doubling delay (5 s → 60 s); the first reply restores the
    // normal cadence and the status line.
    const NET_PROBE_MIN: Duration = Duration::from_secs(5);
    const NET_PROBE_MAX: Duration = Duration::from_secs(60);
    let mut net_probe = NET_PROBE_MIN;
    let mut was_offline = false;
    let mut queue_next_at = tokio::time::Instant::now();
    let mut queue_seen_track = String::new();

    loop {
        // Debounce: fire the pending search once input has been idle for 350ms.
        let debounce = async {
            match search_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        let poll_soon_due = async {
            match poll_soon {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = poll_soon_due => {
                poll_soon = None;
                if backoff_until.is_none() {
                    let now = tokio::time::Instant::now();
                    if let Some(secs) = refresh_playback(&weak, &mut session, &mut last, &mut last_art, &mut liked).await {
                        backoff_until = Some(now + Duration::from_secs(secs));
                        set_status(&weak, format!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
                    }
                    next_playback_poll = now
                        + playback_poll_every(
                            Duration::from_millis(poll_ms),
                            &last,
                            engine_owns_playback(&last, &engine_device_id),
                        );
                }
            }
            _ = queue_ticker.tick(), if sidebar_mode == 1 => {
                let now = tokio::time::Instant::now();
                let cur = current_id(&last);
                if cur != queue_seen_track {
                    queue_seen_track = cur;
                    queue_quiet = 0;
                    queue_next_at = now;
                }
                // Backoff applies here too: a rate limit must not be met with a
                // *faster* poll than the main loop is using. Nor should a background
                // refresh stack a second fetch on one still in flight (on a slow
                // link every reply used to arrive already superseded, leaving the
                // panel stuck on "Loading queue…" while requests piled up).
                if backoff_until.is_none()
                    && !lightify_core::net::is_offline()
                    && now >= queue_next_at
                    && sidebar_fetches_in_flight() == 0
                    && lightify_core::ratelimit::background_ok()
                {
                    tick_queue_sidebar(
                        &weak, &self_tx, &mut sidebar_tracks,
                        &last, &mut queue_mirror, &mut sidebar_generation,
                    );
                }
            }
            _ = clock.tick() => {
                // Engine-owned playback reports its own track changes; for anything
                // else, poll right as the track should have ended instead of waiting
                // out a (possibly stretched) poll interval.
                let owned = engine_owns_playback(&last, &engine_device_id);
                let end_due = progress_clock.tick(&weak, &last, owned);
                // Near the end of a track, warm the next cover (UI-PLAN D9) so the
                // change shows it at once. Only when the queue mirror knows what's next
                // — no Web-API request is made for this.
                if progress_clock.remaining_ms > 0 && progress_clock.remaining_ms <= PREFETCH_ART_BEFORE_MS {
                    if let Some(next) = queue_mirror.as_ref().and_then(|m| m.upcoming.first()) {
                        prefetch_art(&next.album_art, &last_art);
                    }
                }
                if end_due && backoff_until.is_none() {
                    let now = tokio::time::Instant::now();
                    next_playback_poll = now
                        + playback_poll_every(Duration::from_millis(poll_ms), &last, owned);
                    if let Some(secs) = refresh_playback(&weak, &mut session, &mut last, &mut last_art, &mut liked).await {
                        backoff_until = Some(now + Duration::from_secs(secs));
                        set_status(&weak, format!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
                    }
                }
            }
            _ = ticker.tick() => {
                let now = tokio::time::Instant::now();
                // A 429 can land anywhere — a background fetch, a settle poll, a bulk
                // queue loop — and the core's rate gate records it process-wide. Pick
                // it up here so the countdown shows and polling stands down, instead of
                // only noticing when this loop's own next poll happens to hit it.
                if backoff_until.is_none() {
                    if let Some(left) = lightify_core::ratelimit::blocked_for() {
                        backoff_until = Some(now + left);
                    }
                }
                // Give up holding the slider once the echo window has passed, so a
                // level that genuinely never arrives can't freeze it.
                if volume_pending_until.is_some_and(|t| now >= t) {
                    volume_pending_until = None;
                    let _ = weak.upgrade_in_event_loop(|app| app.set_volume_pending(false));
                }
                // Following the OS default output used to live here: every poll tick
                // compared the default device and, on a change, RESTARTED THE ENGINE -
                // up to a full tick to notice plus a new librespot session, a new
                // Connect device and a transfer, which is where 15+ seconds went. The
                // engine's own output thread (`audio_output`) now does this itself in
                // ~100 ms without restarting anything, and reports back through
                // `engine::Event::OutputChanged`.
                // While backing off from a 429, skip all network polling and show a
                // countdown; resume the moment the window elapses.
                if let Some(until) = backoff_until {
                    if now < until {
                        let remaining = (until - now).as_secs() + 1;
                        set_status(&weak, format!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(remaining)));
                    } else {
                        backoff_until = None;
                        set_status(&weak, connected_status(&session));
                    }
                }
                // Downloads come from the local bridge, not Spotify, so they refresh
                // every tick regardless of the Web-API poll schedule or a backoff.
                if sidebar_mode == 3 {
                    // Re-read the bridge's queue each tick (the original polls it every 3s).
                    spawn_downloads_refresh(&weak);
                }
                if backoff_until.is_none() && now >= next_playback_poll {
                    next_playback_poll = now
                        + playback_poll_every(
                            Duration::from_millis(poll_ms),
                            &last,
                            engine_owns_playback(&last, &engine_device_id),
                        );
                    let polled = refresh_playback(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                    if lightify_core::net::is_offline() {
                        // Unreachable: probe again later, backing off, and say so once.
                        next_playback_poll = now + net_probe;
                        set_status(&weak, format!("Offline \u{2014} reconnecting (next try in {}s)", net_probe.as_secs()));
                        net_probe = (net_probe * 2).min(NET_PROBE_MAX);
                        was_offline = true;
                    } else if was_offline {
                        was_offline = false;
                        net_probe = NET_PROBE_MIN;
                        set_status(&weak, connected_status(&session));
                    }
                    match polled {
                        Some(secs) => {
                            backoff_until = Some(now + Duration::from_secs(secs));
                            set_status(&weak, format!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
                        }
                        None => {
                            // Recent re-marks locally (no network); the queue only needs a
                            // re-fetch when the playing track advanced.
                            let cur = current_id(&last);
                            // Keep a Beatport chart queued ahead of what's playing —
                            // `pumpBeatportAutoplay` (`app.js:3800`). The sequence is
                            // dropped the moment the playing track isn't one of ours,
                            // which is the original's own "user started something
                            // else" signal.
                            if let Some(seq) = bp_autoplay.as_mut() {
                                match bp_remaining_ahead(seq, &cur) {
                                    None => {
                                        bp_autoplay = None;
                                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    Some(ahead)
                                        if !seq.completed && !seq.refill_pending && ahead < BP_QUEUE_AHEAD =>
                                    {
                                        // Dispatched to its own background task on its
                                        // own `Session` (see `bp_refill_background`'s doc
                                        // comment) instead of `.await`ed right here — this
                                        // used to block every other command (a sidebar
                                        // click, a search, anything) for as long as a
                                        // match+queue batch took, repeatedly, for as long
                                        // as a chart kept playing.
                                        seq.refill_pending = true;
                                        let seq_id = seq.id;
                                        let source = seq.source.clone();
                                        let next_index = seq.next_index;
                                        let seen = seq.seen.clone();
                                        let tx2 = self_tx.clone();
                                        let active_seq = bp_active_seq.clone();
                                        tokio::spawn(async move {
                                            let result = bp_refill_background(
                                                source, next_index, seen, seq_id, active_seq,
                                            )
                                            .await;
                                            let _ = tx2.send(Cmd::BpRefillDone {
                                                seq_id,
                                                queued: result.0,
                                                seen: result.1,
                                                next_index: result.2,
                                                completed: result.3,
                                                error: result.4,
                                            });
                                        });
                                    }
                                    Some(_) => {}
                                }
                            }
                            // QUEUE (1) has its own `queue_ticker`; DOWNLOADS (3) was
                            // refreshed above. RECENT (2) just re-marks locally.
                            if sidebar_mode == 2 {
                                refresh_sidebar(&weak, &self_tx, sidebar_mode, &mut sidebar_tracks, &last, &mut queue_mirror, &mut sidebar_generation).await;
                            }
                        }
                    }
                }
            }
            _ = debounce => {
                search_deadline = None;
                if let Some(q) = pending_query.take() {
                    // A 429 anywhere (including from search itself) means the whole
                    // session should cool down — searching straight through an active
                    // backoff would just pile on more requests against the same
                    // shared, already-limited quota (see the dev-mode quota note).
                    if let Some(until) = backoff_until.filter(|u| *u > tokio::time::Instant::now()) {
                        let remaining = (until - tokio::time::Instant::now()).as_secs() + 1;
                        set_status(&weak, format!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(remaining)));
                        // Hold the query and run it the moment the limit lifts, rather
                        // than silently dropping what the user typed.
                        pending_query = Some(q);
                        search_deadline = Some(until);
                    } else {
                        let searched = session.search(&q).await;
                        set_search_loading(&weak, false);
                        match searched {
                            Ok(res) => {
                                last_query = q.clone();
                                search_results = res;
                                let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                                search_actions = actions;
                                search_rows = rows.clone();
                                reset_playlist_pager(
                                    &weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done,
                                );
                                // A new search result always replaces whatever was on
                                // screen, including a drilled-into playlist/album/artist
                                // from the previous query (see `exit_search_drill`).
                                exit_search_drill(
                                    &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                                    &mut s_tracks, &mut s_context, &mut s_pager,
                                );
                                drop_selection(&weak, SEL_SEARCH, &mut selected, &mut sel_anchor, &mut sel_scope);
                                push_search(&weak, rows);
                            }
                            Err(e) => {
                                if let Some(secs) = rate_limit_backoff(&e) {
                                    backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
                                    set_status(&weak, format!("Spotify rate limit \u{2014} pausing for {}", fmt_backoff(secs)));
                                } else {
                                    set_status(&weak, format!("Search \u{2014} {e}"));
                                }
                            }
                        }
                    }
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                let cmd = unstamp(cmd);
                match cmd {
                    Cmd::Stamped { .. } => unreachable!("unstamp opens every stamp"),
                    Cmd::StaleClick => {
                        set_status(&weak, "That list just changed \u{2014} try again".to_string());
                    }
                    // Only meaningful on the sign-in page, before this loop starts.
                    Cmd::SignInAuthorise(_) | Cmd::SignInRetry(_) | Cmd::SignInOpenDashboard => {}
                    Cmd::TogglePlay if last.is_none() && resume_offered().is_some() => {
                        // Play on the resume offer: that track, in its context, from
                        // where it stopped, on our own device.
                        if let Some(rp) = take_resume_offer() {
                            set_status(&weak, format!("Resuming {}\u{2026}", rp.name));
                            if let Err(e) = session.play_at(rp.context_uri.as_deref(), &rp.track_uri, rp.position_ms).await {
                                set_status(&weak, format!("Resume \u{2014} {e}"));
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::TogglePlay => {
                        let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                        if engine_owns_playback(&last, &engine_device_id) {
                            // Same line `Next`/`Prev`/volume already draw: our own device
                            // is told directly — instant, and no Web-API quota. The engine
                            // echoes `Event::Playing`, so no settle poll is needed either
                            // (that was two requests per press: the PUT and the re-read).
                            let cmd = if playing { "pause" } else { "play" };
                            if playing {
                                local_pause_at = Some(tokio::time::Instant::now());
                            }
                            engine::send(&serde_json::json!({ "cmd": cmd }).to_string());
                        } else {
                            let r = if playing { session.pause().await } else { session.play().await };
                            if let Err(e) = r { set_status(&weak, format!("Playback — {e}")); }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::Next => {
                        // See QUEUE_SETTLE: give a just-completed batch queue write a
                        // moment to settle server-side before trusting `next()` to
                        // advance by exactly one track.
                        if let Some(t) = last_bulk_queue_write {
                            let elapsed = t.elapsed();
                            if elapsed < QUEUE_SETTLE {
                                tokio::time::sleep(QUEUE_SETTLE - elapsed).await;
                            }
                        }
                        if engine_owns_playback(&last, &engine_device_id) {
                            // No settle poll: the engine's `Event::Track` for the new
                            // track already runs one — doing both read `me/player` twice.
                            engine::send(&serde_json::json!({ "cmd": "next" }).to_string());
                        } else {
                            if let Err(e) = session.next().await {
                                if is_player_restriction(&e) {
                                    // Spotify said "not allowed" but our device can still
                                    // advance itself; don't surface a raw 403 for that.
                                    engine::send(&serde_json::json!({ "cmd": "next" }).to_string());
                                } else {
                                    set_status(&weak, format!("Next \u{2014} {e}"));
                                }
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::Prev => {
                        if engine_owns_playback(&last, &engine_device_id) {
                            // See `Next`: the engine's own track event does the refresh.
                            // (A "previous" that only restarts the current track emits a
                            // position event instead, which the progress clock follows.)
                            engine::send(&serde_json::json!({ "cmd": "prev" }).to_string());
                        } else {
                            if let Err(e) = session.previous().await {
                                if is_player_restriction(&e) {
                                    engine::send(&serde_json::json!({ "cmd": "prev" }).to_string());
                                } else {
                                    set_status(&weak, format!("Prev \u{2014} {e}"));
                                }
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::Drill(i) => {
                        selected.clear();
                        sel_anchor = None;
                        // `i` is the *display* row index, so map it back through the sort view.
                        let src = if i == 0 { None } else { pl_view.get(i - 1).copied() };
                        // ...then drop a filter typed over the library (mapped first: the
                        // index is into the filtered view). It doesn't follow into a playlist.
                        if !filter_of(&PL_FILTER).is_empty() {
                            clear_list_filters(&weak);
                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                            push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                        }
                        set_filter(&TRK_FILTER, "");
                        if i == 0 {
                            // Liked Songs (saved tracks, no context).
                            drilled = true;
                            drill_src = Some(LIKED_SOURCE.to_string());
                            set_drill(&weak, true, "Liked Songs".to_string());
                            set_status(&weak, "Loading Liked Songs\u{2026}".to_string());
                            push_tracks(&weak, &[], &[]);
                            push_sort(&weak, &trk_mode, &trk_dir);
                            tracks.clear();
                            context = None;
                            pager = TrackPager::start(PagerSource::Liked, "Liked Songs");
                            load_next_track_page(
                                &weak, &mut session, &mut pager, &mut tracks, &mut trk_view,
                                &trk_mode, &trk_dir, &presence,
                            )
                            .await;
                        } else if let Some((name, id, uri)) = src
                            .and_then(|k| playlists.get(k))
                            .map(|p| (p.name.clone(), p.id.clone(), p.uri.clone()))
                        {
                            drilled = true;
                            drill_src = Some(id.clone());
                            set_drill(&weak, true, name.clone());
                            set_status(&weak, format!("Loading {name}\u{2026}"));
                            push_tracks(&weak, &[], &[]);
                            push_sort(&weak, &trk_mode, &trk_dir);
                            tracks.clear();
                            context = if uri.is_empty() { None } else { Some(uri) };
                            pager = TrackPager::start(PagerSource::Playlist(id.clone()), &name);
                            load_next_track_page(
                                &weak, &mut session, &mut pager, &mut tracks, &mut trk_view,
                                &trk_mode, &trk_dir, &presence,
                            )
                            .await;
                        }
                    }
                    Cmd::Back => {
                        selected.clear();
                        sel_anchor = None;
                        clear_list_filters(&weak);
                        pager = TrackPager::default();
                        tracks.clear();
                        trk_view.clear();
                        context = None;
                        drilled = false;
                        drill_src = None;
                        set_drill(&weak, false, String::new());
                        push_tracks(&weak, &[], &[]);
                        // The toolbar now reflects the playlist list's own sort state.
                        push_sort(&weak, &lib_mode, &lib_dir);
                    }
                    Cmd::PlayTrack { row, uri } => 'play_track: {
                        let Some(i) = resolve_track_row(&trk_view, &tracks, row, &uri) else {
                            set_status(&weak, "That track is no longer in the list".to_string());
                            break 'play_track;
                        };
                        // With the seq id too: a refill task already in flight checks it
                        // before every queue write, and would otherwise keep queueing the
                        // abandoned chart's rows (`end_autoplay_authority` returns early
                        // when nothing was queued yet, so it can't be relied on here).
                        bp_autoplay = None;
                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        let src = trk_view.get(i).copied();
                        if let Some(t) = src.and_then(|k| tracks.get(k)).cloned() {
                            // No context (Liked Songs): play from here on, in *display* order.
                            let r = if let Some(ctx) = context.clone() {
                                session.play_context(&ctx, &t.uri).await
                            } else {
                                let uris: Vec<String> = trk_view[i..]
                                    .iter()
                                    .filter_map(|&k| tracks.get(k))
                                    .take(50)
                                    .map(|x| x.uri.clone())
                                    .collect();
                                session.play_uris(&uris).await
                            };
                            // Up-next is everything BELOW this row as displayed. A
                            // context play continues in the playlist's own order, which
                            // is not the sorted order on screen, so the panel has to
                            // mirror what the user is looking at.
                            if r.is_ok() {
                                // Without a context only the 50 uris above were sent, so
                                // only those can come next — listing the rest of a long
                                // Liked Songs list here would show tracks that never play.
                                let upcoming: Vec<Track> = trk_view[i + 1..]
                                    .iter()
                                    .filter_map(|&k| tracks.get(k))
                                    .take(if context.is_some() { usize::MAX } else { 49 })
                                    .cloned()
                                    .collect();
                                queue_mirror = Some(QueueMirror::new(t.uri.clone(), upcoming));
                                // A queue fetch dispatched before this play must not land
                                // on top of the mirror it just replaced.
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                            }
                            launched_src = drill_src.clone();
                            if let Err(e) = r { set_status(&weak, format!("Play — {e}")); }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::Search(q) => {
                        let q = q.trim().to_string();
                        if q.is_empty() {
                            pending_query = None;
                            search_deadline = None;
                            search_results = SearchResults::default();
                            search_actions.clear();
                            exit_search_drill(
                                &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                                &mut s_tracks, &mut s_context, &mut s_pager,
                            );
                            drop_selection(&weak, SEL_SEARCH, &mut selected, &mut sel_anchor, &mut sel_scope);
                            // An empty box shows recent searches instead of nothing.
                            last_query.clear();
                            let (rows, actions) = build_recents(&load_recent_searches());
                            search_actions = actions;
                            search_rows = rows.clone();
                            set_search_loading(&weak, false);
                            push_search(&weak, rows);
                        } else {
                            pending_query = Some(q);
                            set_search_loading(&weak, true);
                            search_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_millis(300));
                        }
                    }
                    Cmd::SearchFilter(f) => {
                        search_filter = f;
                        let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                        search_actions = actions;
                        search_rows = rows.clone();
                        reset_playlist_pager(
                            &weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done,
                        );
                        // A filter change re-slices the same fetched results — it must
                        // land on that fresh list, never leave a stale drilled
                        // playlist/album/artist sitting on screen underneath it (see
                        // `exit_search_drill`).
                        exit_search_drill(
                            &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                            &mut s_tracks, &mut s_context, &mut s_pager,
                        );
                        drop_selection(&weak, SEL_SEARCH, &mut selected, &mut sel_anchor, &mut sel_scope);
                        push_search(&weak, rows);
                    }
                    Cmd::OpenSearch(i) => 'open_search: {
                        let action = search_actions.get(i).cloned();
                        // Rows that aren't results: section links and recent searches.
                        match &action {
                            Some(SearchAction::Header { see_all }) => {
                                let f = see_all.clone();
                                let ff = f.clone();
                                let _ = weak.upgrade_in_event_loop(move |app| app.set_search_filter(ff.into()));
                                let _ = self_tx.send(Cmd::SearchFilter(f));
                                break 'open_search;
                            }
                            Some(SearchAction::Recent(q)) => {
                                let _ = self_tx.send(Cmd::PaletteSearch(q.clone()));
                                break 'open_search;
                            }
                            Some(SearchAction::ClearRecents) => {
                                forget_recent_searches();
                                let (rows, actions) = build_recents(&[]);
                                search_actions = actions;
                                search_rows = rows.clone();
                                push_search(&weak, rows);
                                break 'open_search;
                            }
                            // Opening a real result is what makes a query worth remembering.
                            Some(_) if !last_query.is_empty() => remember_search(&last_query),
                            _ => {}
                        }
                        bp_autoplay = None;
                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        if let Some(action) = action {
                            match action {
                                SearchAction::Header { .. } | SearchAction::Recent(_) | SearchAction::ClearRecents => {}
                                SearchAction::Track { uri, .. } => {
                                    end_autoplay_authority(
                                        &weak, &mut session, &mut station_active, &mut station_queued,
                                        &mut bp_autoplay, &mut bp_queued,
                                        &bp_active_seq, &last, &engine_device_id,
                                    )
                                    .await;
                                    // A one-off track has no library source behind it.
                                    launched_src = None;
                                    if let Err(e) = session.play_uris(&[uri]).await {
                                        set_status(&weak, format!("Play — {e}"));
                                    }
                                    settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                }
                                // Both stay in the search tab (`#search-drill`), which
                                // is what the original does — the library list is a
                                // different container and must not be disturbed.
                                SearchAction::Playlist { id, uri, name } => {
                                    s_tracks.clear();
                                    s_context = if uri.is_empty() { None } else { Some(uri) };
                                    set_search_drill(&weak, true, name.clone());
                                    s_pager = TrackPager::start(PagerSource::Playlist(id), &name);
                                    load_next_search_page(
                                        &weak, &mut session, &mut s_pager, &mut s_tracks,
                                    )
                                    .await;
                                }
                                SearchAction::Album { id, name, .. } => {
                                    s_tracks.clear();
                                    s_context = Some(format!("spotify:album:{id}"));
                                    set_search_drill(&weak, true, name);
                                    // Albums are short and the API serves them whole, so
                                    // there is nothing to page here.
                                    s_pager = TrackPager::default();
                                    match session.album_tracks(&id).await {
                                        Ok(t) => {
                                            s_tracks = t;
                                            push_search_tracks(&weak, &s_tracks);
                                            set_status(&weak, connected_status(&session));
                                        }
                                        Err(e) => set_status(&weak, format!("Album \u{2014} {e}")),
                                    }
                                }
                                SearchAction::Artist { id, name } => {
                                    s_tracks.clear();
                                    // No real context behind a synthesized artist
                                    // track list — playing from it falls back to a
                                    // plain uri list, same as Liked Songs.
                                    s_context = None;
                                    set_search_drill(&weak, true, name.clone());
                                    // Top-tracks-or-search-fallback returns everything
                                    // it's going to in one shot; nothing to page.
                                    s_pager = TrackPager::default();
                                    match session.artist_tracks(&id, &name).await {
                                        Ok(t) => {
                                            s_tracks = t;
                                            push_search_tracks(&weak, &s_tracks);
                                            set_status(&weak, connected_status(&session));
                                        }
                                        Err(e) => set_status(&weak, format!("Artist \u{2014} {e}")),
                                    }
                                }
                            }
                        }
                    }
                    // Shuffle / repeat. Both used to send the change and re-read Spotify
                    // 350 ms later — but the Web API still reports the OLD value for a
                    // moment, so the button snapped back, and the next press computed
                    // its next mode from that stale value (off → context → context…).
                    // Now: the button shows the new state at once, a short hold keeps
                    // lagging polls from undoing it (`OPTIONS_HOLD`), and a failure
                    // puts it back. On our own device the engine is told directly
                    // (its own events then confirm it); elsewhere, the Web API.
                    Cmd::ToggleShuffle => {
                        let cur = last.as_ref().map(|p| p.shuffle_state).unwrap_or(false);
                        let want = !cur;
                        show_options(&weak, &mut last, Some(want), None);
                        let result = if engine_owns_playback(&last, &engine_device_id) {
                            engine::send(&serde_json::json!({ "cmd": "shuffle", "enabled": want }).to_string());
                            Ok(())
                        } else {
                            session.set_shuffle(want).await
                        };
                        match result {
                            Ok(()) => {
                                // The up-next order just changed, so the queue panel's
                                // local mirror is wrong now: drop it and let the panel
                                // read Spotify's real (re)shuffled queue.
                                if queue_mirror.is_some() {
                                    queue_mirror = None;
                                    persist_mirror(&queue_mirror);
                                }
                                if sidebar_mode == 1 {
                                    let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                }
                                poll_soon = Some(tokio::time::Instant::now() + Duration::from_millis(2500));
                            }
                            Err(e) => {
                                release_options_hold();
                                show_options(&weak, &mut last, Some(cur), None);
                                release_options_hold();
                                set_status(&weak, format!("Shuffle \u{2014} {e}"));
                            }
                        }
                    }
                    Cmd::CycleRepeat => {
                        let cur = last.as_ref().map(|p| p.repeat_state.clone()).unwrap_or_else(|| "off".into());
                        // off → context → track → off (mirrors the shipped app).
                        let next = next_repeat_mode(&cur);
                        show_options(&weak, &mut last, None, Some(next.to_string()));
                        let result = if engine_owns_playback(&last, &engine_device_id) {
                            // The Web-API repeat flag doesn't reliably reach the local
                            // librespot device — spirc drives track advancement — so our
                            // own device is told directly, as the host does.
                            engine::send(&serde_json::json!({ "cmd": "repeat", "mode": next }).to_string());
                            Ok(())
                        } else {
                            session.set_repeat(next).await
                        };
                        match result {
                            Ok(()) => poll_soon = Some(tokio::time::Instant::now() + Duration::from_millis(2500)),
                            Err(e) => {
                                release_options_hold();
                                show_options(&weak, &mut last, None, Some(cur));
                                release_options_hold();
                                set_status(&weak, format!("Repeat \u{2014} {e}"));
                            }
                        }
                    }
                    Cmd::AddQueue => {
                        if let Some(t) = last.as_ref().and_then(|p| p.track.clone()) {
                            let uri = t.uri.clone();
                            match session.add_to_queue(&uri).await {
                                Ok(()) => {
                                    // While the mirror owns the panel it IS the panel,
                                    // so an addition has to land in it or it would not
                                    // show up at all (the host's context branch does the
                                    // same). Only on success - a mirror that lists a
                                    // track Spotify rejected would be a lie.
                                    if let Some(m) = queue_mirror.as_mut() {
                                        m.enqueue(t);
                                    }
                                    persist_mirror(&queue_mirror);
                                    set_status(&weak, "Added to queue".to_string());
                                }
                                Err(e) => set_status(&weak, format!("Queue — {e}")),
                            }
                            if sidebar_mode == 1 {
                                refresh_sidebar(&weak, &self_tx, sidebar_mode, &mut sidebar_tracks, &last, &mut queue_mirror, &mut sidebar_generation).await;
                            }
                        }
                    }
                    Cmd::SearchEnter => {
                        // Enter in the search box: open the first real result — the Top
                        // result in the All view. (Runs a still-pending query first.)
                        if let Some(q) = pending_query.take() {
                            search_deadline = None;
                            let searched = session.search(&q).await;
                            set_search_loading(&weak, false);
                            if let Ok(res) = searched {
                                last_query = q.clone();
                                search_results = res;
                                let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                                search_actions = actions;
                                search_rows = rows.clone();
                                reset_playlist_pager(&weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done);
                                push_search(&weak, rows);
                            }
                        }
                        if let Some(i) = search_actions.iter().position(|a| {
                            matches!(a, SearchAction::Track { .. } | SearchAction::Playlist { .. } | SearchAction::Album { .. } | SearchAction::Artist { .. })
                        }) {
                            let _ = self_tx.send(Cmd::OpenSearch(i));
                        }
                    }
                    Cmd::ListFilter(text) => {
                        // Whichever left list is showing: the open track list, else
                        // the library. Selections refer to rows, so they drop.
                        selected.clear();
                        sel_anchor = None;
                        let count = if drilled {
                            set_filter(&TRK_FILTER, &text);
                            trk_view = sort_track_view(&tracks, &trk_mode, &trk_dir);
                            push_tracks(&weak, &tracks, &trk_view);
                            let p = presence.clone();
                            let _ = weak.upgrade_in_event_loop(move |app| apply_presence(&app, &p));
                            trk_view.len()
                        } else {
                            set_filter(&PL_FILTER, &text);
                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                            push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                            pl_view.len()
                        };
                        let shown = if text.is_empty() { -1 } else { count as i32 };
                        let _ = weak.upgrade_in_event_loop(move |app| app.set_list_filter_count(shown));
                    }
                    Cmd::SortRecent => {
                        // Applies to whichever library surface is visible (getLibrarySortStateKeys).
                        if drilled {
                            if trk_mode != "recent" {
                                trk_mode = "recent".into();
                                trk_view = sort_track_view(&tracks, &trk_mode, &trk_dir);
                                push_tracks(&weak, &tracks, &trk_view);
                                let p = presence.clone();
                                let _ = weak.upgrade_in_event_loop(move |app| apply_presence(&app, &p));
                            }
                            push_sort(&weak, &trk_mode, &trk_dir);
                        } else {
                            if lib_mode != "recent" {
                                lib_mode = "recent".into();
                                pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                            }
                            push_sort(&weak, &lib_mode, &lib_dir);
                        }
                    }
                    Cmd::SortAlpha => {
                        // Second press on an already-alpha surface flips the direction.
                        if drilled {
                            if trk_mode == "alpha" {
                                trk_dir = if trk_dir == "asc" { "desc" } else { "asc" }.into();
                            } else {
                                trk_mode = "alpha".into();
                                trk_dir = "asc".into();
                            }
                            trk_view = sort_track_view(&tracks, &trk_mode, &trk_dir);
                            push_tracks(&weak, &tracks, &trk_view);
                            let p = presence.clone();
                            let _ = weak.upgrade_in_event_loop(move |app| apply_presence(&app, &p));
                            push_sort(&weak, &trk_mode, &trk_dir);
                        } else {
                            if lib_mode == "alpha" {
                                lib_dir = if lib_dir == "asc" { "desc" } else { "asc" }.into();
                            } else {
                                lib_mode = "alpha".into();
                                lib_dir = "asc".into();
                            }
                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                            push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                            push_sort(&weak, &lib_mode, &lib_dir);
                        }
                    }
                    Cmd::ToggleLike => {
                        let id = last
                            .as_ref()
                            .and_then(|p| p.track.as_ref())
                            .map(|t| t.id.clone())
                            .unwrap_or_default();
                        if id.is_empty() {
                            set_status(&weak, "No track playing".to_string());
                        } else {
                            let target = !liked.on;
                            push_liked(&weak, liked.on, true);
                            match session.set_track_saved(&id, target).await {
                                Ok(()) => {
                                    liked.id = id;
                                    liked.on = target;
                                    push_liked(&weak, target, false);
                                    set_status(
                                        &weak,
                                        if target { "Saved to Liked Songs" } else { "Removed from Liked Songs" }
                                            .to_string(),
                                    );
                                }
                                Err(e) => {
                                    push_liked(&weak, liked.on, false);
                                    set_status(&weak, format!("Like — {e}"));
                                }
                            }
                        }
                    }
                    Cmd::Seek(f) => {
                        let dur = last.as_ref().map(|p| p.duration_ms).unwrap_or(0);
                        if dur > 0 {
                            let ms = (f.clamp(0.0, 1.0) as f64 * dur as f64) as u64;
                            if engine_owns_playback(&last, &engine_device_id) {
                                // Local seek (the host does this for stations): instant, no
                                // quota, and the engine reports the new position back.
                                engine::send(
                                    &serde_json::json!({ "cmd": "seek", "position_ms": ms.min(u32::MAX as u64) as u32 })
                                        .to_string(),
                                );
                                if let Some(pb) = last.as_mut() {
                                    pb.progress_ms = ms;
                                }
                            } else {
                                if let Err(e) = session.seek(ms).await {
                                    set_status(&weak, format!("Seek — {e}"));
                                }
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            }
                        }
                    }
                    Cmd::SetVolume(f) => {
                        let pct = (f.clamp(0.0, 1.0) * 100.0).round() as u32;
                        volume_pending_until = Some(tokio::time::Instant::now() + VOLUME_SETTLE);
                        if engine_owns_playback(&last, &engine_device_id) {
                            // Our own device: set it locally. No round trip, so the
                            // change is audible immediately and the engine echoes the
                            // new level straight back as an event.
                            engine::send(
                                &serde_json::json!({ "cmd": "volume", "percent": pct }).to_string(),
                            );
                        } else if let Err(e) = session.set_volume(pct).await {
                            set_status(&weak, format!("Volume \u{2014} {e}"));
                        }
                        // Deliberately no `settle_refresh` here: re-reading playback
                        // right after the write is what fetched the pre-change level
                        // and snapped the slider back. The regular poll picks it up,
                        // and the latch protects the slider until it agrees.
                        if let Some(pb) = last.as_mut() {
                            pb.volume_percent = pct;
                        }
                    }
                    Cmd::ToggleSidebar(mode) => {
                        let target = next_sidebar_mode(sidebar_mode, mode);
                        sidebar_mode = target;
                        if target == 0 {
                            sidebar_tracks.clear();
                            set_sidebar(&weak, 0, vec![], String::new());
                        } else {
                            match target {
                                // Same non-blocking path the ticker uses: tries the local
                                // mirror first (instant, no network), else spawns the
                                // fetch in the background instead of stalling the whole
                                // command loop on `me/player/queue`.
                                1 => {
                                    refresh_sidebar(
                                        &weak, &self_tx, 1, &mut sidebar_tracks,
                                        &last, &mut queue_mirror, &mut sidebar_generation,
                                    )
                                    .await;
                                }
                                2 => {
                                    sidebar_generation += 1;
                                    set_sidebar(&weak, 2, vec![], "Loading recent\u{2026}".to_string());
                                    spawn_sidebar_fetch(&self_tx, 2, sidebar_generation);
                                }
                                _ => {
                                    // Downloads: rows come from the bridge, not Spotify, so
                                    // this pushes an empty track model and fills the separate
                                    // `sidebar-dl-rows` asynchronously.
                                    sidebar_tracks.clear();
                                    set_sidebar(&weak, 3, vec![], "Loading downloads\u{2026}".to_string());
                                    spawn_downloads_refresh(&weak);
                                }
                            }
                        }
                    }
                    Cmd::CloseSidebar => {
                        sidebar_mode = 0;
                        sidebar_tracks.clear();
                        set_sidebar(&weak, 0, vec![], String::new());
                    }
                    // -- Downloads (the bundled OnTheSpot bridge) --
                    Cmd::Download { url, label } => {
                        spawn_enqueue_download(&weak, &self_tx, url, label);
                    }
                    Cmd::DownloadLibraryRow(row) => {
                        // Row 0 is Liked Songs (a collection URL, not an item id);
                        // every other row is a playlist, indexed through the sort view
                        // exactly like `FollowPlaylist` does.
                        let target = if row == 0 {
                            downloader::spotify_url("liked", "").ok().map(|u| (u, "Liked Songs".to_string()))
                        } else {
                            pl_view
                                .get(row.wrapping_sub(1))
                                .and_then(|&k| playlists.get(k))
                                .and_then(|p| {
                                    downloader::spotify_url("playlist", &p.id)
                                        .ok()
                                        .map(|u| (u, p.name.clone()))
                                })
                        };
                        match target {
                            Some((url, label)) => spawn_enqueue_download(&weak, &self_tx, url, label),
                            None => set_status(&weak, "Nothing to download on that row".to_string()),
                        }
                    }
                    Cmd::DownloadSearchItem(row) => {
                        let target = match search_actions.get(row) {
                            Some(SearchAction::Track { id, .. }) => {
                                let name = search_rows.get(row).map(|r| r.name.to_string()).unwrap_or_default();
                                downloader::spotify_url("track", id).ok().map(|u| (u, name))
                            }
                            Some(SearchAction::Playlist { id, name, .. }) => {
                                downloader::spotify_url("playlist", id).ok().map(|u| (u, name.clone()))
                            }
                            _ => None,
                        };
                        match target {
                            Some((url, label)) => spawn_enqueue_download(&weak, &self_tx, url, label),
                            None => set_status(&weak, "Nothing to download on that row".to_string()),
                        }
                    }
                    Cmd::DownloadBeatportRow(row) => {
                        // A Beatport row is a Beatport track: resolve it to a Spotify
                        // track first, the same scored match `BpPlay`/`BpQueue` use.
                        if let Some(bt) = bp_tracks.get(row).cloned() {
                            set_status(&weak, format!("Matching \u{201c}{}\u{201d} on Spotify\u{2026}", bt.name));
                            match session.beatport_match(&bt.name, &bt.artists).await {
                                Ok(Some(t)) => match downloader::spotify_url("track", &t.id) {
                                    Ok(url) => spawn_enqueue_download(&weak, &self_tx, url, t.name.clone()),
                                    Err(e) => set_status(&weak, e),
                                },
                                Ok(None) => set_status(&weak, format!("No Spotify match to download for {}", bt.name)),
                                Err(e) => set_status(&weak, format!("Beatport \u{2014} {e}")),
                            }
                        }
                    }
                    Cmd::RefreshDownloads => {
                        // `ensureDownloadsSidebar` (app.js:5429): open the panel on
                        // DOWNLOADS if it isn't already, otherwise just re-read.
                        if sidebar_mode != 3 {
                            sidebar_mode = 3;
                            sidebar_tracks.clear();
                            set_sidebar(&weak, 3, vec![], "Loading downloads\u{2026}".to_string());
                        }
                        spawn_downloads_refresh(&weak);
                    }
                    Cmd::ClearDownloads => {
                        spawn_download_action(
                            &weak,
                            true,
                            "Cleared finished downloads".to_string(),
                            downloader::clear_finished(),
                        );
                    }
                    Cmd::DownloadRetry(id) => {
                        if downloader::last_snapshot().iter().any(|i| i.local_id == id) {
                            spawn_download_action(
                                &weak,
                                true,
                                "Download queued for retry".to_string(),
                                async move { downloader::retry(&id).await },
                            );
                        } else {
                            set_status(&weak, "That download is no longer listed".to_string());
                        }
                    }
                    Cmd::DownloadCancel(id) => {
                        if downloader::last_snapshot().iter().any(|i| i.local_id == id) {
                            spawn_download_action(
                                &weak,
                                true,
                                "Download cancelled".to_string(),
                                async move { downloader::cancel(&id).await },
                            );
                        } else {
                            set_status(&weak, "That download is no longer listed".to_string());
                        }
                    }
                    Cmd::DownloadDelete(id) => {
                        if downloader::last_snapshot().iter().any(|i| i.local_id == id) {
                            spawn_download_action(
                                &weak,
                                true,
                                "Deleted downloaded file".to_string(),
                                async move { downloader::delete(&id).await },
                            );
                        } else {
                            set_status(&weak, "That download is no longer listed".to_string());
                        }
                    }
                    Cmd::DownloadOpenFolder(path) => {
                        // An empty path means "no specific file" -- exactly the case
                        // the original uses for "Open download folder".
                        if let Err(e) = downloader::open_folder(&path) {
                            set_status(&weak, e);
                        }
                    }
                    Cmd::DownloaderLogin => {
                        push_downloader_status(&weak, "Waiting\u{2026}".to_string(), false, true);
                        let weak2 = weak.clone();
                        let tx2 = self_tx.clone();
                        tokio::spawn(async move {
                            let login_in_progress = match downloader::start_login().await {
                                Ok(st) => {
                                    push_downloader_status(
                                        &weak2,
                                        st.summary(),
                                        st.spotify_ready,
                                        st.login_in_progress,
                                    );
                                    if !st.spotify_ready {
                                        set_status(
                                            &weak2,
                                            "Downloader sign-in started \u{2014} open Spotify and pick \
                                             OnTheSpot in the device list. If Windows asked about the \
                                             firewall, make sure both Private and Public networks are \
                                             allowed \u{2014} it only checks Private by default."
                                                .to_string(),
                                        );
                                    }
                                    st.login_in_progress
                                }
                                Err(e) => {
                                    push_downloader_status(&weak2, e.clone(), false, false);
                                    set_status(&weak2, format!("Downloader setup failed \u{2014} {e}"));
                                    false
                                }
                            };
                            // Picking "OnTheSpot" in Spotify's device list is a real
                            // Connect device switch — it can knock Lightify's own
                            // engine offline as a side effect (see
                            // `engine::Event::Exited`'s "moved" handling), same as
                            // genuinely switching to any other device would. Unlike
                            // that case, this one is a routine, expected part of using
                            // Lightify's own built-in downloader, not the user
                            // deliberately choosing to listen elsewhere — so reclaim
                            // automatically rather than leaving a status line for the
                            // user to notice and act on themselves. Sent now, not after
                            // the polling loop below — the engine can lose active status
                            // within seconds of the handoff, long before a slow sign-in
                            // (or its 150s timeout) actually resolves.
                            let _ = tx2.send(Cmd::ReclaimPlaybackAfterDownloaderLogin);
                            // The bridge resolves this itself in the background
                            // (connected, "already exists", or — now that the vendored
                            // login wait has a real deadline — a timeout) with nothing
                            // pushing that change back to this app on its own. Without
                            // polling here, the "Select OnTheSpot..." text above stays
                            // frozen on screen forever regardless of what actually
                            // happened, since nothing else re-asks the bridge unless
                            // the user closes and reopens Settings.
                            if login_in_progress {
                                let deadline = tokio::time::Instant::now() + Duration::from_secs(170);
                                while tokio::time::Instant::now() < deadline {
                                    tokio::time::sleep(Duration::from_secs(4)).await;
                                    match downloader::status().await {
                                        Ok(polled) => {
                                            push_downloader_status(
                                                &weak2,
                                                polled.summary(),
                                                polled.spotify_ready,
                                                polled.login_in_progress,
                                            );
                                            if !polled.login_in_progress {
                                                break;
                                            }
                                        }
                                        // Bridge unreachable — nothing more to poll for.
                                        Err(_) => break,
                                    }
                                }
                            }
                        });
                    }
                    Cmd::SetDownloadPath(path) => {
                        let weak2 = weak.clone();
                        tokio::spawn(async move {
                            match downloader::set_download_path(&path).await {
                                Ok(saved) => {
                                    let shown = saved.clone();
                                    let _ = weak2.upgrade_in_event_loop(move |app| {
                                        app.set_settings_download_path(shown.into());
                                    });
                                    set_status(&weak2, "Download path saved".to_string());
                                }
                                Err(e) => set_status(&weak2, format!("Failed to set download path \u{2014} {e}")),
                            }
                        });
                    }
                    Cmd::ClearQueue => {
                        // There is no Web-API endpoint to clear a queue (only to add to
                        // one) — the whole mechanism below is spirc reloading a
                        // single-track context on OUR OWN engine. If Lightify isn't
                        // actually the active device (see `engine_owns_playback`'s doc
                        // comment — a previous-run device still connected, or something
                        // else legitimately took over, e.g. answering the downloader's
                        // own device picker), this silently did nothing to the real
                        // queue while still claiming success — and cleared the LOCAL
                        // display of what was queued too, which is worse: the one place
                        // that's supposed to reflect reality now doesn't either. Bail
                        // out honestly instead of lying about it.
                        if !engine_owns_playback(&last, &engine_device_id) {
                            set_status(
                                &weak,
                                "Lightify isn\u{2019}t the active Spotify player right now \u{2014} \
                                 Settings \u{2192} Restart, then try again."
                                    .to_string(),
                            );
                        } else if last_is_stale(&last, &engine_track_name) {
                            // `clearqueue` reloads `last`'s track at `last`'s position; if
                            // that's the *previous* song, playback would jump back to it.
                            set_status(&weak, STALE_TRACK_NOTE.to_string());
                        } else {
                            // The panel must go empty immediately, not on the next poll —
                            // and a fetch already in flight holds the pre-clear queue,
                            // which would otherwise land (and be persisted) afterwards.
                            queue_mirror = None;
                            sidebar_generation += 1;
                            save_queue("", &[]);
                            // Emptying the queue ends every lane that feeds it. A station
                            // still being resolved must not land afterwards; a station's
                            // autoplay must not keep turning the next song into radio (it
                            // used to stay on, since the lane was dropped without telling
                            // the engine); and a running Beatport chart must not quietly
                            // top the queue back up the moment it was emptied.
                            cancel_pending_station();
                            if station_active {
                                engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": false }).to_string());
                            }
                            bp_autoplay = None;
                            bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                            // Clearing the queue must NOT touch what is playing: the engine
                            // resets the device queue and reloads the current song where it
                            // was (see the engine's `ClearQueue` for why the reset matters).
                            let playing_uri = last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default();
                            set_status(&weak, "Clearing the queue\u{2026}".to_string());
                            clear_device_queue(&mut session, &last).await;
                            // Only forget what we queued once the queue has been read back
                            // and shows it gone. Forgetting first is what let a station's
                            // leftovers sit in the real queue, invisible to every later
                            // cleanup.
                            let mut tracked: std::collections::HashSet<String> = station_queued.clone();
                            tracked.extend(bp_queued.iter().cloned());
                            match queue_after_clear(&mut session, &playing_uri).await {
                                Some(t) => {
                                    let left: std::collections::HashSet<String> =
                                        t.iter().map(|x| x.uri.clone()).filter(|u| tracked.contains(u)).collect();
                                    station_queued.retain(|u| left.contains(u));
                                    bp_queued.retain(|u| left.contains(u));
                                    if left.is_empty() {
                                        station_active = false;
                                        set_status(&weak, "Cleared queue".to_string());
                                    } else {
                                        // Still tracked, so the next play or station retries.
                                        set_status(&weak, format!("Cleared the queue, but {} queued tracks are still there", left.len()));
                                    }
                                    if sidebar_mode == 1 {
                                        let pid = current_id(&last);
                                        sidebar_tracks = t;
                                        set_sidebar(&weak, 1,
                                            build_side_rows(&sidebar_tracks, &pid, false),
                                            "No queued tracks".to_string());
                                    }
                                }
                                None => {
                                    // Never heard back: keep the bookkeeping rather than guess.
                                    set_status(&weak, "Couldn\u{2019}t confirm the queue was cleared \u{2014} try again".to_string());
                                }
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::RefreshQueueSidebar => {
                        // Sent after a local queue write: the queue just changed, so
                        // the adaptive backoff starts over.
                        queue_quiet = 0;
                        queue_next_at = tokio::time::Instant::now();
                        refresh_sidebar(&weak, &self_tx, sidebar_mode, &mut sidebar_tracks, &last, &mut queue_mirror, &mut sidebar_generation).await;
                    }
                    Cmd::SidebarFetched { generation, mode, result } => {
                        // Stale: the panel was closed, switched to a different mode, or
                        // asked to refresh again since this fetch was dispatched —
                        // applying it now would clobber what's actually showing with
                        // something older. See `spawn_sidebar_fetch`'s doc comment.
                        if generation == sidebar_generation && mode == sidebar_mode {
                            let pid = current_id(&last);
                            match result {
                                // A mirror only exists here if some local action (a list
                                // play, a restore) built one after this fetch went out —
                                // fetches are only dispatched while there is none. That
                                // mirror is newer than this answer, so the answer loses.
                                Ok(_) if mode == 1 && queue_mirror.is_some() => {}
                                Ok(t) => {
                                    if mode == 1 {
                                        let same = t.len() == sidebar_tracks.len()
                                            && t.iter().zip(&sidebar_tracks).all(|(a, b)| a.uri == b.uri);
                                        queue_quiet = if same { (queue_quiet + 1).min(3) } else { 0 };
                                        queue_next_at = tokio::time::Instant::now() + QUEUE_POLL * (1u32 << queue_quiet);
                                    }
                                    // Backfill a mirror from Spotify's own queue, exactly
                                    // as the old inline fetch did — not just for plays
                                    // that never built one locally, but so this state
                                    // persists and can fold forward on its own next time.
                                    if mode == 1 {
                                        let playing_uri = last
                                            .as_ref()
                                            .and_then(|p| p.track.as_ref())
                                            .map(|t| t.uri.clone())
                                            .unwrap_or_default();
                                        if !t.is_empty() && !playing_uri.is_empty() {
                                            queue_mirror = Some(QueueMirror::new(playing_uri, t.clone()));
                                            persist_mirror(&queue_mirror);
                                        }
                                    }
                                    sidebar_tracks = t;
                                    let empty = if mode == 1 { "No queued tracks" } else { "No recent tracks yet" };
                                    set_sidebar(&weak, mode, build_side_rows(&sidebar_tracks, &pid, mode == 2), empty.to_string());
                                }
                                // This used to be swallowed outright for the ticker's own
                                // refresh path (`if let Ok(t) = ... {}`, no `else`) — a
                                // failure here just left the panel showing whatever it
                                // had last shown, forever, with nothing telling the user
                                // why it "stopped updating". A `?device_id=`-targeted
                                // call 404s once that remembered device is no longer the
                                // active one (see `engine_owns_playback`'s doc comment),
                                // which is exactly what silently not-updating looked like.
                                Err(e) => {
                                    sidebar_tracks.clear();
                                    let label = if mode == 1 { "Queue" } else { "Recent" };
                                    set_sidebar(&weak, mode, vec![], format!("{label} \u{2014} {e}"));
                                }
                            }
                        }
                    }
                    Cmd::RemoveQueuedTrack(i) => {
                        // No Spotify/librespot primitive removes one specific item from
                        // the middle of a connect-state queue (`clear_next_tracks` keeps
                        // user-queued tracks; there is no "remove at position N"). The
                        // only way to actually drop one is: clear everything, then re-add
                        // every *other* track from what was showing. Real costs, both
                        // accepted deliberately rather than silently: this is O(queue
                        // length) network calls for removing one track, and it re-queues
                        // through the same batch-write path that has its own confirmed
                        // eventual-consistency window (see QUEUE_SETTLE) — so the rebuilt
                        // queue's *order* briefly racing on the very next Next click is a
                        // real, known possibility, not eliminated by this feature.
                        if sidebar_mode == 1 && !engine_owns_playback(&last, &engine_device_id) {
                            // Same reasoning as the Clear button: this whole mechanism is
                            // spirc-only (no Web-API "remove from queue" exists at all), so
                            // if Lightify isn't actually the active device this would
                            // silently do nothing to the real queue while still claiming
                            // the track was removed.
                            set_status(
                                &weak,
                                "Lightify isn\u{2019}t the active Spotify player right now \u{2014} \
                                 Settings \u{2192} Restart, then try again."
                                    .to_string(),
                            );
                        } else if sidebar_mode == 1 && last_is_stale(&last, &engine_track_name) {
                            set_status(&weak, STALE_TRACK_NOTE.to_string());
                        } else if sidebar_mode == 1 {
                            if let Some(removed) = sidebar_tracks.get(i).cloned() {
                                // Capped: the list can be a whole playlist's remainder
                                // (a list-play mirror), and every kept row is one POST,
                                // awaited inline. See `QUEUE_KEEP_MAX`.
                                let kept: Vec<Track> = sidebar_tracks
                                    .iter()
                                    .enumerate()
                                    .filter(|(idx, _)| *idx != i)
                                    .map(|(_, t)| t.clone())
                                    .take(QUEUE_KEEP_MAX)
                                    .collect();
                                let keep: Vec<String> = kept.iter().map(|t| t.uri.clone()).collect();
                                // Same rule as the Clear button: emptying the queue to
                                // rebuild it must not stop what is playing.
                                let cur_uri = last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone());
                                let playing_uri = cur_uri.clone().unwrap_or_default();
                                clear_device_queue(&mut session, &last).await;
                                // The reset deactivates the device for a moment; adding to a
                                // queue that isn't there yet fails, so wait for the song to be
                                // back before re-adding the others.
                                let _ = queue_after_clear(&mut session, &playing_uri).await;
                                let mut requeued = 0usize;
                                let mut failed: Option<String> = None;
                                for uri in &keep {
                                    match session.add_to_queue(uri).await {
                                        Ok(()) => requeued += 1,
                                        Err(e) => { failed = Some(e); break; }
                                    }
                                }
                                if !keep.is_empty() {
                                    last_bulk_queue_write = Some(tokio::time::Instant::now());
                                    spawn_delayed_queue_refresh(&self_tx);
                                }
                                let cur_uri = last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default();
                                // The panel follows the mirror, so the mirror must be what
                                // actually got re-queued — otherwise the next queue tick
                                // redrew the removed track right back (and persisted it).
                                let rebuilt: Vec<Track> = kept.into_iter().take(requeued).collect();
                                // What was re-added is still whatever lane queued it; what was
                                // removed (or failed to come back) is no longer in the queue.
                                // Wiping the bookkeeping here instead would leave the re-added
                                // station tracks in the real queue with nothing tracking them.
                                let back: std::collections::HashSet<&str> = rebuilt.iter().map(|t| t.uri.as_str()).collect();
                                station_queued.retain(|u| back.contains(u.as_str()));
                                bp_queued.retain(|u| back.contains(u.as_str()));
                                let mut m = QueueMirror::new(cur_uri, rebuilt);
                                m.queued = m.upcoming.len();
                                sidebar_tracks = m.upcoming.clone();
                                queue_mirror = if m.upcoming.is_empty() { None } else { Some(m) };
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                                let pid = current_id(&last);
                                set_sidebar(&weak, 1, build_side_rows(&sidebar_tracks, &pid, false), "No queued tracks".to_string());
                                match failed {
                                    Some(e) if requeued < keep.len() => set_status(
                                        &weak,
                                        format!("Removed \u{201c}{}\u{201d}, but only restored {requeued}/{} \u{2014} {e}",
                                            removed.name, keep.len()),
                                    ),
                                    _ => set_status(&weak, format!("Removed \u{201c}{}\u{201d} from the queue", removed.name)),
                                }
                            }
                        }
                    }
                    Cmd::SidebarPlay(i) => {
                        // DOWNLOADS shares this callback but isn't a track list: a click
                        // on a finished row reveals the file, matching the original's
                        // dblclick handler (`app.js:5931`). Anything else is inert.
                        //
                        // Written as if/else rather than an early `continue`: the loop
                        // body ends with `sync_presence`, which a `continue` would skip.
                        if sidebar_mode == 3 {
                            if let Some(item) =
                                downloader::last_snapshot().get(i).filter(|it| it.on_disk())
                            {
                                if let Err(e) = downloader::open_folder(&item.file_path) {
                                    set_status(&weak, e);
                                }
                            }
                        } else {
                        // RECENT starts something new; QUEUE is navigation inside the
                        // queue itself, where a reset would destroy what is being used.
                        if sidebar_mode != 1 {
                            end_autoplay_authority(
                                &weak, &mut session, &mut station_active, &mut station_queued,
                                &mut bp_autoplay, &mut bp_queued,
                                &bp_active_seq, &last, &engine_device_id,
                            )
                            .await;
                        }
                        if let Some(t) = sidebar_tracks.get(i).cloned() {
                            if !t.uri.is_empty() {
                                launched_src = None;
                                // In QUEUE, jump *into* the queue: play the clicked track and
                                // everything after it. Playing it alone replaced the whole
                                // context, so the rest of the list on screen never played.
                                let rest: Vec<Track> = if sidebar_mode == 1 {
                                    sidebar_tracks[i + 1..]
                                        .iter()
                                        .filter(|x| !x.uri.is_empty())
                                        .take(49)
                                        .cloned()
                                        .collect()
                                } else {
                                    Vec::new()
                                };
                                let mut uris = vec![t.uri.clone()];
                                uris.extend(rest.iter().map(|x| x.uri.clone()));
                                let r = session.play_uris(&uris).await;
                                if sidebar_mode == 1 && r.is_ok() {
                                    // The panel now lists exactly what was sent.
                                    queue_mirror = Some(QueueMirror::new(t.uri.clone(), rest));
                                    sidebar_generation += 1;
                                    persist_mirror(&queue_mirror);
                                }
                                if let Err(e) = r {
                                    set_status(&weak, format!("Play — {e}"));
                                }
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                refresh_sidebar(&weak, &self_tx, sidebar_mode, &mut sidebar_tracks, &last, &mut queue_mirror, &mut sidebar_generation).await;
                            }
                        }
                        }
                    }
                    Cmd::BpEnsure => {
                        if !bp_loaded {
                            bp_loaded = true;
                            drop_selection(&weak, SEL_BEATPORT, &mut selected, &mut sel_anchor, &mut sel_scope);
                            bp_fetch(&weak, &session, &bp_genres, bp_idx, &bp_kind, &mut bp_tracks).await;
                        }
                    }
                    Cmd::BpSelectGenre(i) => {
                        if let Some((label, _slug)) = bp_genres.get(i) {
                            bp_idx = i;
                            bp_loaded = true;
                            set_bp_genre_selected(&weak, i, label.clone());
                            drop_selection(&weak, SEL_BEATPORT, &mut selected, &mut sel_anchor, &mut sel_scope);
                            bp_fetch(&weak, &session, &bp_genres, bp_idx, &bp_kind, &mut bp_tracks).await;
                        }
                    }
                    Cmd::BpSetMode(kind) => {
                        bp_kind = kind;
                        bp_loaded = true;
                        set_bp_kind(&weak, bp_kind.clone());
                        drop_selection(&weak, SEL_BEATPORT, &mut selected, &mut sel_anchor, &mut sel_scope);
                        bp_fetch(&weak, &session, &bp_genres, bp_idx, &bp_kind, &mut bp_tracks).await;
                    }
                    Cmd::BpPlay(i) => {
                        // Ports `playBeatportSequenceFromIndex` (`app.js:3868`): play
                        // FROM this row and keep queueing the rest of the chart behind
                        // it, rather than playing the one matched track and stopping.
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        bp_autoplay = None;
                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        if i < bp_tracks.len() {
                            let label = bp_tracks[i].name.clone();
                            set_bp_status(&weak, format!("Matching \u{201c}{label}\u{201d} on Spotify\u{2026}"));

                            // Only the first couple of rows are matched before playback
                            // starts - that wait is the one the user actually feels.
                            bp_autoplay_next_id += 1;
                            let mut seq = BpAutoplay {
                                id: bp_autoplay_next_id,
                                source: bp_tracks.clone(),
                                next_index: i,
                                loaded_ids: Vec::new(),
                                seen: std::collections::HashSet::new(),
                                completed: false,
                                refill_pending: false,
                            };
                            let mut first: Vec<String> = Vec::new();
                            let mut first_name = String::new();
                            let mut first_artists = String::new();
                            // Whether the row the user actually clicked is the one that
                            // ends up playing. A chart's top pick is often a Beatport
                            // exclusive or pre-release with no Spotify match at all — the
                            // loop below silently moves on to the next row when that
                            // happens (by design, so a click always starts *something*),
                            // but that used to read as "it skipped my song": the status
                            // line only ever named whatever track *did* end up matching,
                            // with no mention that it wasn't the one clicked.
                            let mut clicked_matched = false;
                            while first.len() < BP_INITIAL_MATCH && seq.next_index < seq.source.len() {
                                let is_clicked_row = seq.next_index == i;
                                let bt = seq.source[seq.next_index].clone();
                                seq.next_index += 1;
                                match session.beatport_match(&bt.name, &bt.artists).await {
                                    Ok(Some(t)) => {
                                        if !seq.seen.insert(t.uri.clone()) {
                                            continue;
                                        }
                                        if first.is_empty() {
                                            first_name = t.name.clone();
                                            first_artists = t.artists.clone();
                                            clicked_matched = is_clicked_row;
                                        }
                                        if !t.id.is_empty() {
                                            seq.loaded_ids.push(t.id.clone());
                                        }
                                        first.push(t.uri);
                                    }
                                    Ok(None) => continue,
                                    Err(e) => {
                                        set_bp_status(&weak, format!("Match \u{2014} {e}"));
                                        break;
                                    }
                                }
                            }

                            if first.is_empty() {
                                set_bp_status(
                                    &weak,
                                    format!("No Spotify match for \u{201c}{label}\u{201d}"),
                                );
                            } else {
                                launched_src = None;
                                if let Err(e) = session.play_uris(&first).await {
                                    set_bp_status(&weak, format!("Play \u{2014} {e}"));
                                } else {
                                    set_bp_status(
                                        &weak,
                                        format!("Playing {first_name} \u{2014} {first_artists}"),
                                    );
                                    settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                    // One refill now so the queue isn't empty the moment
                                    // they open it; the poll tick tops it up from here.
                                    seq.completed = seq.next_index >= seq.source.len();
                                    if !seq.completed {
                                        bp_refill(&weak, &mut session, &mut seq, &mut bp_queued).await;
                                    }
                                    let left = seq.source.len().saturating_sub(seq.next_index);
                                    // `loaded_ids` also holds the track that is playing.
                                    let ahead = seq.loaded_ids.len().saturating_sub(1);
                                    // The substitution note survives into this final
                                    // message rather than only the transient one above,
                                    // since this is the one the user actually ends up
                                    // reading (the "queued, left to match" update lands
                                    // moments later and would otherwise erase it).
                                    let prefix = if clicked_matched {
                                        String::new()
                                    } else {
                                        format!(
                                            "\u{201c}{label}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{first_name}\u{201d} instead. "
                                        )
                                    };
                                    set_bp_status(
                                        &weak,
                                        if left > 0 {
                                            format!(
                                                "{prefix}Playing \u{2014} {ahead} queued, {left} left to match"
                                            )
                                        } else {
                                            format!("{prefix}Playing \u{2014} {ahead} queued")
                                        },
                                    );
                                    last_bulk_queue_write = Some(tokio::time::Instant::now());
                                    spawn_delayed_queue_refresh(&self_tx);
                                    bp_active_seq
                                        .store(bp_autoplay_next_id, std::sync::atomic::Ordering::SeqCst);
                                    bp_autoplay = Some(seq);
                                }
                            }
                        }
                    }
                    Cmd::BpOpenExternal => {
                        let slug = bp_genres.get(bp_idx).map(|(_, s)| s.clone()).unwrap_or_default();
                        match lightify_core::beatport::chart_url(&slug, &bp_kind) {
                            Ok(url) => {
                                if open_url(&url) {
                                    set_bp_status(&weak, "Opened the chart in your browser".to_string());
                                } else {
                                    set_bp_status(&weak, format!("Open this in your browser: {url}"));
                                }
                            }
                            Err(e) => set_bp_status(&weak, e),
                        }
                    }
                    Cmd::BpRefillDone { seq_id, queued, seen, next_index, completed, error } => {
                        // Tracked here regardless of whether `seq_id` is still current:
                        // `bp_refill_background`'s cancellation check narrows the race to
                        // a sliver (at most the one `add_to_queue` already in flight the
                        // instant the sequence was abandoned), but can't close it to zero
                        // without aborting a request already sent. A uri that lands on the
                        // real queue despite that must still end up in `bp_queued`, or it
                        // becomes an untracked leftover `end_autoplay_authority` can never
                        // find — exactly the bug this whole mechanism exists to prevent.
                        for (uri, _) in &queued {
                            bp_queued.insert(uri.clone());
                        }
                        if let Some(seq) = bp_autoplay.as_mut() {
                            if seq.id == seq_id {
                                seq.refill_pending = false;
                                seq.seen = seen;
                                seq.next_index = next_index;
                                seq.completed = completed;
                                for (_uri, id) in &queued {
                                    if !id.is_empty() {
                                        seq.loaded_ids.push(id.clone());
                                    }
                                }
                                if let Some(e) = error {
                                    set_status(&weak, format!("Beatport queue refill \u{2014} {e}"));
                                }
                            }
                            // else: this result is for a sequence that's since been
                            // abandoned (the user moved on) or replaced (a new chart
                            // click) — its own bookkeeping (seen/next_index/completed/
                            // loaded_ids/status line) no longer applies, but any uris it
                            // did manage to queue are still tracked above.
                        }
                        if !queued.is_empty() {
                            last_bulk_queue_write = Some(tokio::time::Instant::now());
                            spawn_delayed_queue_refresh(&self_tx);
                        }
                    }
                    Cmd::OpenSettings | Cmd::RefreshDevices => {
                        // Ensure the modal is visible (the palette path sends this without
                        // the UI-side open toggle the gear button does).
                        note_settings_open(true);
                        let _ = weak.upgrade_in_event_loop(|app| app.set_settings_open(true));
                        set_account(&weak, connected_status(&session));
                        // Re-read the live process state rather than trusting the last
                        // event: an engine that died silently must not still read "Running".
                        if !engine::running() {
                            push_engine_status(&weak, "Not running".to_string(), false);
                        }
                        push_outputs(&weak);
                        // Downloader: the path comes straight off the shared config
                        // (no bridge needed); the account state does need the bridge,
                        // so it is fetched in the background — the panel must open
                        // instantly even on a cold bridge start.
                        let dl_path = downloader::download_path();
                        let _ = weak.upgrade_in_event_loop(move |app| {
                            app.set_settings_download_path(dl_path.into());
                        });
                        push_downloader_status(&weak, "Checking\u{2026}".to_string(), false, false);
                        spawn_downloader_status(&weak);
                        set_device_status(&weak, "Loading\u{2026}".to_string());
                        push_devices(&weak, vec![]);
                        match session.devices().await {
                            Ok(d) => {
                                if d.is_empty() {
                                    set_device_status(&weak, "No devices yet \u{2014} Lightify\u{2019}s own player is still starting.".to_string());
                                }
                                push_devices(&weak, build_device_rows(&d));
                                devices = d;
                            }
                            Err(e) => {
                                devices.clear();
                                set_device_status(&weak, format!("Devices — {e}"));
                                push_devices(&weak, vec![]);
                            }
                        }
                    }
                    Cmd::SelectDevice(i) => {
                        if let Some(d) = devices.get(i).cloned() {
                            let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                            match session.transfer_playback(&d.id, playing).await {
                                Ok(()) => {
                                    set_status(&weak, format!("Playing on {}", d.name));
                                    note_settings_open(false);
                                    let _ = weak.upgrade_in_event_loop(|app| app.set_settings_open(false));
                                    settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                }
                                Err(e) => set_status(&weak, format!("Transfer — {e}")),
                            }
                        }
                    }
                    Cmd::CollapseLeft => {
                        let _ = weak.upgrade_in_event_loop(|app| app.set_left_collapsed(!app.get_left_collapsed()));
                    }
                    Cmd::PaletteOpen => {
                        let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                        palette_actions = apply_palette_filter(&weak, "", &playlists, &pl_view, playing);
                    }
                    Cmd::PaletteQuery(q) => {
                        let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                        palette_actions = apply_palette_filter(&weak, &q, &playlists, &pl_view, playing);
                    }
                    Cmd::PaletteRun(idx) => {
                        if let Some(cmd) = palette_actions.get(idx).cloned() {
                            let _ = self_tx.send(cmd);
                        }
                    }
                    // ── Row context menus ──────────────────────────────────────
                    Cmd::OpenContext { kind, index } => {
                        // Built here because the worker owns the data the menu
                        // depends on; the display index maps back through the sort
                        // view exactly like a row click does.
                        // Right-clicking INSIDE a multi-row selection swaps in the
                        // group menu; an unselected row keeps the single-row one
                        // (`showGroupMenu` in registerSelectableRow).
                        let in_selection = sel_scope == kind
                            && selected.len() > 1
                            && sel_key(kind, index, &trk_view)
                                .is_some_and(|k| selected.contains(&k));
                        let items = match kind {
                            0 => {
                                if index == 0 {
                                    liked_row_menu()
                                } else {
                                    playlist_row_menu(index)
                                }
                            }
                            1 if in_selection => selection_menu(selected.len()),
                            1 => trk_view
                                .get(index)
                                .and_then(|&k| tracks.get(k))
                                .map(|t| track_row_menu(index, t))
                                .unwrap_or_default(),
                            2 if in_selection => selection_menu(selected.len()),
                            2 => match search_actions.get(index).cloned() {
                                Some(SearchAction::Track { uri, id }) => {
                                    // The original only offers "Like" when the track
                                    // isn't already saved, and checks before showing.
                                    let show_like = if id.is_empty() {
                                        false
                                    } else {
                                        !session.is_track_saved(&id).await.unwrap_or(true)
                                    };
                                    search_track_menu(index, &uri, &id, show_like)
                                }
                                Some(SearchAction::Playlist { .. }) => search_playlist_menu(index),
                                Some(SearchAction::Album { .. }) => search_album_menu(index),
                                Some(SearchAction::Artist { .. }) => search_artist_menu(index),
                                Some(SearchAction::Header { .. } | SearchAction::Recent(_) | SearchAction::ClearRecents) | None => Vec::new(),
                            },
                            3 if in_selection => beatport_selection_menu(selected.len()),
                            3 => beatport_row_menu(index),
                            4 if in_selection => {
                                if sidebar_mode == 1 {
                                    selection_menu(selected.len())
                                } else {
                                    sidebar_recent_selection_menu(selected.len())
                                }
                            }
                            // The original only opens this menu when a track with an
                            // id is loaded; with nothing playing there is no menu.
                            6 => last
                                .as_ref()
                                .and_then(|p| p.track.as_ref())
                                .filter(|t| !t.id.is_empty())
                                .map(now_playing_menu)
                                .unwrap_or_default(),
                            5 if in_selection => selection_menu(selected.len()),
                            5 => s_tracks
                                .get(index)
                                .map(|t| search_drill_menu(index, t))
                                .unwrap_or_default(),
                            4 => sidebar_tracks
                                .get(index)
                                .map(|t| sidebar_row_menu(index, t))
                                .unwrap_or_default(),
                            // The DOWNLOADS sidebar is its own surface: rows are bridge
                            // queue items, not Spotify tracks, so they never take part
                            // in the track multi-select above.
                            7 => downloader::last_snapshot()
                                .get(index)
                                .map(download_row_menu)
                                .unwrap_or_default(),
                            _ => Vec::new(),
                        };
                        // Menu actions run later (after `RunContext` re-queues them), so
                        // each carries the list version the menu was built from.
                        let items: Vec<CtxItem> = if (0..LIST_SURFACES as i32).contains(&kind) && kind != 6 {
                            items
                                .into_iter()
                                .map(|mut it| {
                                    it.cmd = stamp_current(kind as usize, it.cmd);
                                    it
                                })
                                .collect()
                        } else {
                            items
                        };
                        ctx_actions = push_context(&weak, items);
                    }
                    Cmd::RunContext(i) => {
                        if let Some(cmd) = ctx_actions.get(i).cloned() {
                            let _ = self_tx.send(cmd);
                        }
                    }

                    // ── Search result menus ────────────────────────────────────
                    Cmd::QueueSearchTrack(i) => {
                        if let Some(SearchAction::Track { uri, .. }) = search_actions.get(i).cloned() {
                            match session.add_to_queue(&uri).await {
                                Ok(()) => {
                                    mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[&search_results.tracks]);
                                    if sidebar_mode == 1 {
                                        let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                    }
                                    set_status(&weak, "Added to queue".to_string());
                                }
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::LikeSearchTrack(i) => {
                        if let Some(SearchAction::Track { id, .. }) = search_actions.get(i) {
                            match session.set_track_saved(id, true).await {
                                Ok(()) => set_status(&weak, "Saved to Liked Songs".to_string()),
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::ShareSearchTrack(i) => {
                        if let Some(SearchAction::Track { id, .. }) = search_actions.get(i) {
                            share_spotify_link(&weak, "track", id);
                        }
                    }
                    Cmd::PlaySearchContext(i) => {
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        let uri = match search_actions.get(i) {
                            Some(SearchAction::Playlist { uri, .. }) => uri.clone(),
                            Some(SearchAction::Album { uri, .. }) => uri.clone(),
                            _ => String::new(),
                        };
                        if uri.is_empty() {
                            set_status(&weak, "Nothing to play here".to_string());
                        } else {
                            launched_src = None;
                            if let Err(e) = session.play_context_start(&uri).await {
                                set_status(&weak, format!("Play \u{2014} {e}"));
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::SaveSearchItem(i) => {
                        let r = match search_actions.get(i) {
                            Some(SearchAction::Playlist { id, .. }) => {
                                Some((session.follow_playlist(id).await, "Playlist saved"))
                            }
                            Some(SearchAction::Album { id, .. }) => {
                                Some((session.save_album(id).await, "Album saved"))
                            }
                            _ => None,
                        };
                        if let Some((res, ok)) = r {
                            match res {
                                Ok(()) => set_status(&weak, ok.to_string()),
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    // ── Beatport menus ─────────────────────────────────────────
                    Cmd::BpQueue(i) => {
                        if let Some(bt) = bp_tracks.get(i).cloned() {
                            match session.beatport_match(&bt.name, &bt.artists).await {
                                Ok(Some(t)) => match session.add_to_queue(&t.uri).await {
                                    Ok(()) => {
                                        set_status(&weak, format!("Added to queue: {}", t.name));
                                        let uri = t.uri.clone();
                                        mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[std::slice::from_ref(&t)]);
                                        if sidebar_mode == 1 {
                                            let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                        }
                                    }
                                    Err(e) => set_status(&weak, e),
                                },
                                Ok(None) => set_status(&weak, format!("No Spotify match for {}", bt.name)),
                                Err(e) => set_status(&weak, format!("Beatport \u{2014} {e}")),
                            }
                        }
                    }
                    Cmd::BpSelectAll => {
                        // Toggle: a full selection clears, anything else selects all.
                        let all = bp_tracks.len();
                        if sel_scope != SEL_BEATPORT || selected.len() < all {
                            if sel_scope != SEL_BEATPORT {
                                let old = sel_scope;
                                selected.clear();
                                if old != SEL_NONE {
                                    push_selection(&weak, old, &selected, &trk_view);
                                }
                                sel_scope = SEL_BEATPORT;
                            }
                            selected = (0..all).collect();
                        } else {
                            selected.clear();
                            sel_scope = SEL_NONE;
                        }
                        sel_anchor = None;
                        push_selection(&weak, SEL_BEATPORT, &selected, &trk_view);
                    }
                    Cmd::BpPlaySelection | Cmd::BpQueueSelection | Cmd::BpCreatePlaylist
                        if sel_scope != SEL_BEATPORT =>
                    {
                        // The toolbar button is enabled by the global selection count,
                        // so it can fire while the selection belongs to another list —
                        // whose indices would then be read as Beatport row numbers.
                        set_status(&weak, "No Beatport tracks selected".to_string());
                    }
                    Cmd::BpPlaySelection | Cmd::BpQueueSelection | Cmd::BpCreatePlaylist => {
                        // A selection is its own fixed list, not the running chart —
                        // but *saving* one as a playlist leaves playback alone.
                        if !matches!(cmd, Cmd::BpCreatePlaylist) {
                            bp_autoplay = None;
                            bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        }
                        // Beatport rows are Beatport tracks: each has to be resolved to
                        // a Spotify track first (the host's scored match).
                        let picked: Vec<BeatportTrack> = bp_tracks
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| selected.contains(i))
                            .map(|(_, t)| t.clone())
                            .collect();
                        if picked.is_empty() {
                            set_status(&weak, "No Beatport tracks selected".to_string());
                        } else {
                            set_status(&weak, format!("Matching {} Beatport tracks\u{2026}", picked.len()));
                            let mut uris = Vec::new();
                            let mut bp_matched: Vec<Track> = Vec::new();
                            let mut missed = 0usize;
                            for bt in &picked {
                                match session.beatport_match(&bt.name, &bt.artists).await {
                                    Ok(Some(t)) => {
                                        uris.push(t.uri.clone());
                                        bp_matched.push(t);
                                    }
                                    _ => missed += 1,
                                }
                            }
                            if uris.is_empty() {
                                set_status(&weak, "No Spotify matches for the selection".to_string());
                            } else {
                                let tail = if missed > 0 { format!(" ({missed} unmatched)") } else { String::new() };
                                match cmd {
                                    Cmd::BpPlaySelection => {
                                        end_autoplay_authority(
                                            &weak, &mut session, &mut station_active, &mut station_queued,
                                            &mut bp_autoplay, &mut bp_queued,
                                            &bp_active_seq, &last, &engine_device_id,
                                        )
                                        .await;
                                        launched_src = None;
                                        let n = uris.len();
                                        if let Err(e) = session.play_uris(&uris).await {
                                            set_status(&weak, format!("Play \u{2014} {e}"));
                                        } else {
                                            set_status(&weak, format!("Playing {n} selected{tail}"));
                                        }
                                        settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                    }
                                    Cmd::BpQueueSelection => {
                                        let mut queued = 0usize;
                                        let mut failed: Option<String> = None;
                                        for uri in &uris {
                                            match session.add_to_queue(uri).await {
                                                Ok(()) => queued += 1,
                                                Err(e) => { failed = Some(e); break; }
                                            }
                                        }
                                        if queued > 0 {
                                            mirror_queued(&mut queue_mirror, &mut sidebar_generation, &uris[..queued], &[&bp_matched]);
                                            last_bulk_queue_write = Some(tokio::time::Instant::now());
                                            spawn_delayed_queue_refresh(&self_tx);
                                        }
                                        match failed {
                                            Some(e) if queued == 0 => set_status(&weak, e),
                                            Some(e) => set_status(&weak, format!("Added {queued} to queue, then stopped \u{2014} {e}")),
                                            None => set_status(&weak, format!("Added {queued} to queue{tail}")),
                                        }
                                    }
                                    _ => {
                                        let name = timestamped_playlist_name("BP Selection");
                                        match session.create_playlist(&name, &uris).await {
                                            Ok(pl) => {
                                                set_status(&weak, format!("Created: {}{tail}", pl.name));
                                                if let Ok(p) = session.playlists().await {
                                                    playlists = p;
                                                    remember_library(&playlists);
                                                    thumbs::request(&weak, &playlists);
                                                    pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                                    push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                                                }
                                            }
                                            Err(e) => set_status(&weak, format!("Create playlist \u{2014} {e}")),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // ── Sidebar menus ──────────────────────────────────────────
                    Cmd::SidebarQueueRow(i) => {
                        if let Some(uri) = sidebar_tracks.get(i).map(|t| t.uri.clone()) {
                            match session.add_to_queue(&uri).await {
                                Ok(()) => {
                                    let pool = sidebar_tracks.clone();
                                    mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[&pool]);
                                    if sidebar_mode == 1 {
                                        let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                    }
                                    set_status(&weak, "Added to queue".to_string());
                                }
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::ShareSidebarTrack(i) => {
                        if let Some(id) = sidebar_tracks.get(i).map(|t| t.id.clone()) {
                            share_spotify_link(&weak, "track", &id);
                        }
                    }
                    Cmd::SidebarCreatePlaylist => {
                        // The original builds this from the WHOLE queue, not a selection.
                        let mut seen = std::collections::HashSet::new();
                        let uris: Vec<String> = sidebar_tracks
                            .iter()
                            .filter(|t| !t.uri.is_empty())
                            .filter(|t| seen.insert(t.uri.clone()))
                            .map(|t| t.uri.clone())
                            .collect();
                        if uris.is_empty() {
                            set_status(&weak, "Queue is empty".to_string());
                        } else {
                            let name = timestamped_playlist_name("Queue");
                            match session.create_playlist(&name, &uris).await {
                                Ok(pl) => {
                                    set_status(&weak, format!("Created: {}", pl.name));
                                    if let Ok(p) = session.playlists().await {
                                        playlists = p;
                                        remember_library(&playlists);
                                        thumbs::request(&weak, &playlists);
                                        pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                        push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                                    }
                                }
                                Err(e) => set_status(&weak, format!("Failed: {e}")),
                            }
                        }
                    }
                    Cmd::SidebarPlayFirstSelected => {
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        let first = sidebar_tracks
                            .iter()
                            .enumerate()
                            .find(|(i, _)| selected.contains(i))
                            .map(|(_, t)| t.uri.clone());
                        match first {
                            Some(uri) if !uri.is_empty() => {
                                launched_src = None;
                                if let Err(e) = session.play_uris(&[uri]).await {
                                    set_status(&weak, format!("Play \u{2014} {e}"));
                                }
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            }
                            _ => set_status(&weak, "Nothing selected".to_string()),
                        }
                    }
                    Cmd::SearchBack => {
                        exit_search_drill(
                            &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                            &mut s_tracks, &mut s_context, &mut s_pager,
                        );
                        set_status(&weak, connected_status(&session));
                    }
                    Cmd::LoadMoreSearchResults => {
                        // Every single-type filter pages (the original paged PLAYLISTS
                        // only): 10 more of that type per scroll to the bottom. No
                        // `loading` flag is needed: the worker handles one command at a
                        // time, and the UI closes its own gate the moment it fires.
                        let kind = search_kind(&search_filter);
                        if sp_done || kind.is_empty() {
                            let _ = weak.upgrade_in_event_loop(|app| app.set_search_more(false));
                        } else {
                            let q = last_query.clone();
                            match session.search_page(&q, kind, sp_offset).await {
                                Ok((more, total)) => {
                                    let before = search_rows.len();
                                    search_results.tracks.extend(more.tracks);
                                    search_results.artists.extend(more.artists);
                                    search_results.albums.extend(more.albums);
                                    search_results.playlists.extend(more.playlists);
                                    search_results.track_thumbs.extend(more.track_thumbs);
                                    sp_offset += Session::SEARCH_PAGE;
                                    // Spotify's playlist search drops `null` entries
                                    // into the items array, so the number that PARSES
                                    // is routinely below the limit — page off the
                                    // reported total, not off a short parsed page.
                                    let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                                    sp_done = if total > 0 { sp_offset >= total } else { rows.len() == before };
                                    search_actions = actions;
                                    search_rows = rows.clone();
                                    push_search(&weak, rows);
                                }
                                Err(e) => {
                                    sp_done = true;
                                    set_status(&weak, format!("Search \u{2014} {e}"));
                                }
                            }
                            let more = !sp_done;
                            let _ = weak.upgrade_in_event_loop(move |app| app.set_search_more(more));
                        }
                    }
                    Cmd::LoadMoreSearchTracks => {
                        load_next_search_page(&weak, &mut session, &mut s_pager, &mut s_tracks).await;
                    }
                    Cmd::PlaySearchDrillTrack(i) => {
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        if let Some(t) = s_tracks.get(i).cloned() {
                            let r = if let Some(ctx) = s_context.clone() {
                                session.play_context(&ctx, &t.uri).await
                            } else {
                                let uris: Vec<String> =
                                    s_tracks[i..].iter().take(50).map(|x| x.uri.clone()).collect();
                                session.play_uris(&uris).await
                            };
                            // A search hit isn't a library row, so nothing to highlight.
                            launched_src = None;
                            if let Err(e) = r {
                                set_status(&weak, format!("Play \u{2014} {e}"));
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::QueueSearchDrillTrack(i) => {
                        if let Some(uri) = s_tracks.get(i).map(|t| t.uri.clone()) {
                            match session.add_to_queue(&uri).await {
                                Ok(()) => {
                                    mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[&s_tracks]);
                                    if sidebar_mode == 1 {
                                        let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                    }
                                    set_status(&weak, "Added to queue".to_string());
                                }
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::ShareSearchDrillTrack(i) => {
                        if let Some(id) = s_tracks.get(i).map(|t| t.id.clone()) {
                            share_spotify_link(&weak, "track", &id);
                        }
                    }
                    Cmd::LoadMoreTracks => {
                        load_next_track_page(
                            &weak, &mut session, &mut pager, &mut tracks, &mut trk_view,
                            &trk_mode, &trk_dir, &presence,
                        )
                        .await;
                    }
                    Cmd::PlayCurrentTrack => {
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        if let Some(uri) =
                            last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone())
                        {
                            // The original plays it as a bare uri, with no context.
                            launched_src = None;
                            if let Err(e) = session.play_uris(&[uri]).await {
                                set_status(&weak, format!("Play \u{2014} {e}"));
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::ShareCurrentTrack => {
                        let id = last
                            .as_ref()
                            .and_then(|p| p.track.as_ref())
                            .map(|t| t.id.clone())
                            .unwrap_or_default();
                        share_spotify_link(&weak, "track", &id);
                    }
                    Cmd::StartStation => {
                        // Covers starting a station directly off whatever is already
                        // playing (a Beatport chart, or another station) without an
                        // intervening play command of its own — the common "started from
                        // a liked song" case already goes through `Cmd::PlayTrack`'s own
                        // cleanup, but nothing enforced that this path can't be reached
                        // straight from an active chart/station too.
                        // The song keeps playing and the new station queues behind it, so
                        // an earlier station's leftovers are cleared without stopping it.
                        end_autoplay_authority_keep_playing(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued, &bp_active_seq,
                            &last, &engine_device_id,
                        )
                        .await;
                        let uri = last
                            .as_ref()
                            .and_then(|p| p.track.as_ref())
                            .map(|t| t.uri.clone())
                            .unwrap_or_default();
                        start_station(&weak, &uri).await;
                        settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                    }
                    Cmd::StationFromUri(uri) => {
                        end_autoplay_authority_keep_playing(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued, &bp_active_seq,
                            &last, &engine_device_id,
                        )
                        .await;
                        start_station(&weak, &uri).await;
                        settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                    }
                    Cmd::Note(msg) => set_status(&weak, msg),
                    // ── Multi-select ───────────────────────────────────────────
                    Cmd::SelectClick { scope, row, ctrl, shift } => {
                        // A click in a different list drops the previous selection and
                        // clears its marks, so only one surface is ever selected.
                        if scope != sel_scope {
                            let old = sel_scope;
                            selected.clear();
                            sel_anchor = None;
                            if old != SEL_NONE {
                                push_selection(&weak, old, &selected, &trk_view);
                            }
                            sel_scope = scope;
                        }
                        if let Some(key) = sel_key(scope, row, &trk_view) {
                            if shift {
                                // Extend from the last clicked row, like any list UI.
                                let from = sel_anchor.unwrap_or(row);
                                let (lo, hi) = if row < from { (row, from) } else { (from, row) };
                                selected.clear();
                                for r in lo..=hi {
                                    if let Some(k) = sel_key(scope, r, &trk_view) {
                                        selected.insert(k);
                                    }
                                }
                            } else if ctrl {
                                if !selected.remove(&key) {
                                    selected.insert(key);
                                }
                                sel_anchor = Some(row);
                            }
                            push_selection(&weak, scope, &selected, &trk_view);
                        }
                    }
                    Cmd::ClearSelection => {
                        sel_anchor = None;
                        if !selected.is_empty() {
                            let scope = sel_scope;
                            selected.clear();
                            sel_scope = SEL_NONE;
                            push_selection(&weak, scope, &selected, &trk_view);
                        }
                    }
                    Cmd::PlaySelection => {
                        bp_autoplay = None;
                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        // The host filters unplayable rows and de-dupes before playing.
                        let uris = scoped_selection_uris(
                            sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, true,
                        );
                        if uris.is_empty() {
                            set_status(&weak, "Selection has no playable tracks".to_string());
                        } else {
                            let n = uris.len();
                            if let Err(e) = session.play_uris(&uris).await {
                                set_status(&weak, format!("Play \u{2014} {e}"));
                            } else {
                                set_status(&weak, format!("Playing {n} selected"));
                            }
                            launched_src = None;
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::QueueSelection => {
                        let uris = scoped_selection_uris(
                            sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, true,
                        );
                        if uris.is_empty() {
                            set_status(&weak, "Selection has no playable tracks".to_string());
                        } else {
                            let mut queued = 0usize;
                            let mut failed: Option<String> = None;
                            for uri in &uris {
                                match session.add_to_queue(uri).await {
                                    Ok(()) => queued += 1,
                                    Err(e) => {
                                        failed = Some(e);
                                        break;
                                    }
                                }
                            }
                            if queued > 0 {
                                let pool = sidebar_tracks.clone();
                                mirror_queued(
                                    &mut queue_mirror, &mut sidebar_generation, &uris[..queued],
                                    &[&tracks, &s_tracks, &pool, &search_results.tracks],
                                );
                                last_bulk_queue_write = Some(tokio::time::Instant::now());
                                spawn_delayed_queue_refresh(&self_tx);
                            }
                            match failed {
                                // Report what actually landed: one POST per track, so a
                                // mid-run rate limit must not read as a clean success.
                                Some(e) if queued == 0 => set_status(&weak, e),
                                Some(e) => set_status(&weak, format!("Added {queued} to queue, then stopped \u{2014} {e}")),
                                None => set_status(&weak, format!("Added {queued} to queue")),
                            }
                        }
                    }
                    Cmd::CreatePlaylistFromSelection => {
                        // Unlike Play/Queue this keeps unplayable rows: the host only
                        // drops them for playback, not for a saved playlist.
                        let uris = scoped_selection_uris(
                            sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, false,
                        );
                        if uris.is_empty() {
                            set_status(&weak, "Selection has no playlist tracks".to_string());
                        } else {
                            let name = timestamped_playlist_name("Selection");
                            match session.create_playlist(&name, &uris).await {
                                Ok(pl) => {
                                    set_status(&weak, format!("Created: {}", pl.name));
                                    // The host reloads the library so the new row shows.
                                    if let Ok(p) = session.playlists().await {
                                        playlists = p;
                                        remember_library(&playlists);
                                        thumbs::request(&weak, &playlists);
                                        pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                        push_rows(
                                            &weak,
                                            build_rows(&playlists, &pl_view, &presence.source, presence.playing),
                                        );
                                    }
                                }
                                Err(e) => set_status(&weak, format!("Create playlist \u{2014} {e}")),
                            }
                        }
                    }

                    Cmd::PlayRow(i) => {
                        bp_autoplay = None;
                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                        end_autoplay_authority(
                            &weak, &mut session, &mut station_active, &mut station_queued,
                            &mut bp_autoplay, &mut bp_queued,
                            &bp_active_seq, &last, &engine_device_id,
                        )
                        .await;
                        // Liked Songs has no "play the whole thing" in the original
                        // (its row carries no dblclick and its menu has no Play), so
                        // row 0 deliberately does nothing.
                        if let Some((id, uri)) = pl_view
                            .get(i.wrapping_sub(1))
                            .and_then(|&k| playlists.get(k))
                            .map(|p| (p.id.clone(), p.uri.clone()))
                        {
                            if uri.is_empty() {
                                set_status(&weak, "Play \u{2014} playlist has no URI".to_string());
                            } else {
                                launched_src = Some(id);
                                if let Err(e) = session.play_context_start(&uri).await {
                                    set_status(&weak, format!("Play \u{2014} {e}"));
                                }
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            }
                        }
                    }
                    Cmd::FollowPlaylist(i) => {
                        if let Some(id) = pl_view
                            .get(i.wrapping_sub(1))
                            .and_then(|&k| playlists.get(k))
                            .map(|p| p.id.clone())
                        {
                            match session.follow_playlist(&id).await {
                                Ok(()) => set_status(&weak, "Playlist saved to library".to_string()),
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::ConfirmDeletePlaylist(i) => {
                        if let Some(name) = pl_view
                            .get(i.wrapping_sub(1))
                            .and_then(|&k| playlists.get(k))
                            .map(|p| p.name.clone())
                        {
                            let id = pl_view
                                .get(i.wrapping_sub(1))
                                .and_then(|&k| playlists.get(k))
                                .map(|p| p.id.clone())
                                .unwrap_or_default();
                            pending_confirm = Some(Cmd::DeletePlaylist(id, name.clone()));
                            let msg = format!("Delete \u{201c}{name}\u{201d} from your Spotify library?");
                            let _ = weak.upgrade_in_event_loop(move |app| {
                                app.set_confirm_title("DELETE PLAYLIST".into());
                                app.set_confirm_message(msg.into());
                                app.set_confirm_open(true);
                            });
                        }
                    }
                    Cmd::ConfirmAccept => {
                        if let Some(cmd) = pending_confirm.take() {
                            let _ = self_tx.send(cmd);
                        }
                    }
                    Cmd::DeletePlaylist(id, name) => {
                        if !id.is_empty() {
                            match session.unfollow_playlist(&id).await {
                                Ok(()) => {
                                    // The original re-runs loadPlaylists() so the row
                                    // actually disappears.
                                    match session.playlists().await {
                                        Ok(p) => {
                                            playlists = p;
                                            remember_library(&playlists);
                                            thumbs::request(&weak, &playlists);
                                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                            push_rows(
                                                &weak,
                                                build_rows(&playlists, &pl_view, &presence.source, presence.playing),
                                            );
                                        }
                                        Err(e) => set_status(&weak, format!("Playlists \u{2014} {e}")),
                                    }
                                    set_status(&weak, format!("Deleted playlist: {name}"));
                                }
                                Err(e) => set_status(&weak, format!("Failed to delete playlist: {e}")),
                            }
                        }
                    }
                    Cmd::QueueTrack { row, uri } => {
                        let i = resolve_track_row(&trk_view, &tracks, row, &uri);
                        if let Some(t) = i.and_then(|i| trk_view.get(i)).and_then(|&k| tracks.get(k)).cloned() {
                            match session.add_to_queue(&t.uri).await {
                                Ok(()) => {
                                    if let Some(m) = queue_mirror.as_mut() {
                                        m.enqueue(t);
                                    }
                                    persist_mirror(&queue_mirror);
                                    set_status(&weak, "Added to queue".to_string());
                                }
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::ShareTrack(i) => {
                        if let Some(id) =
                            trk_view.get(i).and_then(|&k| tracks.get(k)).map(|t| t.id.clone())
                        {
                            share_spotify_link(&weak, "track", &id);
                        }
                    }
                    Cmd::SetMiniMode(mode) => {
                        // Marshal to the UI thread — window resize + always-on-top run there.
                        let _ = weak.upgrade_in_event_loop(move |app| apply_mini_mode(&app, &mode));
                    }
                    Cmd::PaletteSearch(q) => {
                        set_tab(&weak, 1);
                        let qq = q.clone();
                        let _ = weak.upgrade_in_event_loop(move |app| app.set_search_text(qq.into()));
                        if let Some(until) = backoff_until {
                            let remaining = (until - tokio::time::Instant::now()).as_secs() + 1;
                            set_status(&weak, format!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(remaining)));
                        } else {
                            let searched = session.search(&q).await;
                            set_search_loading(&weak, false);
                            match searched {
                                Ok(res) => {
                                    last_query = q.clone();
                                    search_results = res;
                                    let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                                    search_actions = actions;
                                    search_rows = rows.clone();
                                    reset_playlist_pager(
                                        &weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done,
                                    );
                                    exit_search_drill(
                                        &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                                        &mut s_tracks, &mut s_context, &mut s_pager,
                                    );
                                    drop_selection(&weak, SEL_SEARCH, &mut selected, &mut sel_anchor, &mut sel_scope);
                                    push_search(&weak, rows);
                                }
                                Err(e) => {
                                    if let Some(secs) = rate_limit_backoff(&e) {
                                        backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
                                        set_status(&weak, format!("Spotify rate limit \u{2014} pausing for {}", fmt_backoff(secs)));
                                    } else {
                                        set_status(&weak, format!("Search \u{2014} {e}"));
                                    }
                                }
                            }
                        }
                    }
                    Cmd::Engine(ev) => match ev {
                        engine::Event::OutputChanged { device } => {
                            // Only announce actual moves, not the first open at startup -
                            // "Audio output -> Speakers" on every launch is noise.
                            if !system_default_device.is_empty() && system_default_device != device {
                                set_status(&weak, format!("Audio output \u{2192} {device}"));
                            }
                            system_default_device = device;
                            push_outputs(&weak);
                        }
                        engine::Event::Ready { device_id } => {
                            engine_watchdog.note_healthy();
                            push_engine_status(&weak, "Running \u{00B7} Lightify".to_string(), true);
                            // Record it regardless of whether the adoption call below
                            // succeeds — the id is valid the moment spirc reports it,
                            // and `engine_owns_playback` needs it to recognise OUR
                            // device even if the Web-API transfer had trouble.
                            engine_device_id = device_id.clone();
                            // Adopt our own device so every Web-API transport call
                            // lands here rather than 404-ing for want of a device.
                            match adopt_engine_device(&mut session, &device_id).await {
                                Ok(name) => {
                                    set_status(&weak, format!("Playing through {name}"));
                                    if let Some(secs) =
                                        refresh_playback(&weak, &mut session, &mut last, &mut last_art, &mut liked).await
                                    {
                                        backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
                                    } else if !queue_restored {
                                        queue_restored = true;
                                        restore_queue(&weak, &mut session, &last, &mut queue_mirror).await;
                                    }
                                }
                                Err(e) => set_status(&weak, format!("Playback device \u{2014} {e}")),
                            }
                            if devices_open(&weak) {
                                if let Ok(d) = session.devices().await {
                                    push_devices(&weak, build_device_rows(&d));
                                    devices = d;
                                }
                            }
                        }
                        // The engine knows about play/pause and seeks the instant they
                        // happen; trusting it beats waiting up to a poll interval.
                        engine::Event::Playing { playing } => {
                            let was_playing = last.as_ref().is_some_and(|p| p.is_playing);
                            let ours = local_pause_at.take().is_some_and(|t| t.elapsed() < Duration::from_secs(3));
                            if was_playing && !playing && !ours {
                                // Not a pause we sent: find out soon where playback went
                                // (a short delay lets Spotify's device state settle).
                                poll_soon = Some(tokio::time::Instant::now() + Duration::from_millis(1500));
                            }
                            if let Some(pb) = last.as_mut() {
                                pb.is_playing = playing;
                            }
                            let _ = weak.upgrade_in_event_loop(move |app| app.set_playing(playing));
                        }
                        engine::Event::Position { ms } => {
                            if let Some(pb) = last.as_mut() {
                                pb.progress_ms = ms;
                                let dur = pb.duration_ms;
                                if dur > 0 {
                                    let frac = (ms as f32 / dur as f32).clamp(0.0, 1.0);
                                    let elapsed = fmt_time(ms);
                                    let _ = weak.upgrade_in_event_loop(move |app| {
                                        if !app.get_scrubbing_progress() {
                                            app.set_progress(frac);
                                            app.set_elapsed(elapsed.into());
                                        }
                                    });
                                }
                            }
                        }
                        // Authoritative for our own device: take it as is.
                        engine::Event::Options { shuffle, repeat } => {
                            release_options_hold();
                            if let Some(pb) = last.as_mut() {
                                if let Some(s) = shuffle {
                                    pb.shuffle_state = s;
                                }
                                if let Some(r) = repeat.as_ref() {
                                    pb.repeat_state = r.clone();
                                }
                            }
                            let _ = weak.upgrade_in_event_loop(move |app| {
                                if let Some(s) = shuffle {
                                    app.set_shuffle_on(s);
                                }
                                if let Some(r) = repeat {
                                    app.set_repeat_mode(r.into());
                                }
                            });
                        }
                        engine::Event::Volume { percent } => {
                            let v = (percent as f32 / 100.0).clamp(0.0, 1.0);
                            let _ = weak.upgrade_in_event_loop(move |app| {
                                apply_reported_volume(&app, v);
                            });
                        }
                        // The engine names the new track the moment it loads. Show that
                        // straight away, then pull the full state (cover art, duration,
                        // liked) from the Web API — a poll interval later is too slow to
                        // feel connected to the music.
                        engine::Event::Track { name, artists } => {
                            // librespot drops repeat-one when a *different* track starts
                            // (a skip), without reporting it; with repeat-one riding on
                            // repeat-all that leaves repeat-all (see the engine's Repeat).
                            // Mirror it now rather than after the next poll. The same
                            // track starting again is repeat-one doing its job.
                            let skipped_off_repeat_one = !engine_track_name.is_empty()
                                && !name.is_empty()
                                && name != engine_track_name
                                && last.as_ref().is_some_and(|p| p.repeat_state == "track");
                            if skipped_off_repeat_one {
                                release_options_hold();
                                show_options(&weak, &mut last, None, Some("context".to_string()));
                                release_options_hold();
                            }
                            engine_track_name = name.clone();
                            if !name.is_empty() {
                                let (n, a) = (name.clone(), artists.clone());
                                let _ = weak.upgrade_in_event_loop(move |app| {
                                    app.set_track_name(n.into());
                                    app.set_track_artist(a.into());
                                });
                            }
                            if backoff_until.is_none() {
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            }
                        }
                        engine::Event::NeedAuth { msg } => {
                            let msg = if msg.trim().is_empty() {
                                "Finish the Spotify streaming sign-in in your browser".to_string()
                            } else {
                                msg
                            };
                            set_status(&weak, msg);
                        }
                        engine::Event::Failed { msg } => {
                            push_engine_status(&weak, format!("Error \u{2014} {msg}"), false);
                            set_status(&weak, format!("Playback engine \u{2014} {msg}"));
                        }
                        // An answer to a station the user has since abandoned (see
                        // `STATION_EPOCH`): queueing it now would put the OLD station's
                        // tracks in front of whatever they moved on to.
                        engine::Event::StationTracks { seq, .. } if !station_epoch_is_current(seq) => {
                            set_status(&weak, "Ignored a station that was no longer wanted".to_string());
                        }
                        engine::Event::StationTracks { uris, .. } => {
                            // Drop the seed itself and anything already queued-worthy
                            // duplicate, then either queue behind the current song or —
                            // if nothing is playing — start the station outright.
                            let current = last
                                .as_ref()
                                .and_then(|p| p.track.as_ref())
                                .map(|t| t.uri.clone())
                                .unwrap_or_default();
                            let mut seen = std::collections::HashSet::new();
                            let picked: Vec<String> = uris
                                .into_iter()
                                .filter(|u| *u != current)
                                .filter(|u| seen.insert(u.clone()))
                                .take(STATION_QUEUE_MAX)
                                .collect();
                            if picked.is_empty() {
                                set_status(&weak, "Station had no tracks to queue".to_string());
                            } else if last.as_ref().is_some_and(|p| p.is_playing) {
                                // The station now owns the queue lane.
                                station_active = true;
                                engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
                                // Added to, never replaced: anything an earlier station
                                // left that couldn't be cleared yet must stay tracked so a
                                // later cleanup still finds it.
                                station_queued.extend(picked.iter().cloned());
                                let mut queued = 0usize;
                                let mut failed: Option<String> = None;
                                for uri in &picked {
                                    match session.add_to_queue(uri).await {
                                        Ok(()) => queued += 1,
                                        Err(e) => { failed = Some(e); break; }
                                    }
                                }
                                if queued > 0 {
                                    last_bulk_queue_write = Some(tokio::time::Instant::now());
                                    spawn_delayed_queue_refresh(&self_tx);
                                    // Capture + persist the resulting queue regardless of
                                    // whether the panel is open — this is the "start a
                                    // station" bug report's own scenario, and it shouldn't
                                    // depend on the QUEUE sidebar happening to be visible.
                                    if let Ok(t) = session.queue().await {
                                        if !t.is_empty() {
                                            queue_mirror = Some(QueueMirror::new(current.clone(), t));
                                            persist_mirror(&queue_mirror);
                                        }
                                    }
                                }
                                match failed {
                                    Some(e) if queued == 0 => {
                                        set_status(&weak, format!("Station \u{2014} {e}"))
                                    }
                                    Some(e) => set_status(
                                        &weak,
                                        format!("Queued {queued} station tracks, then stopped \u{2014} {e}"),
                                    ),
                                    None => set_status(
                                        &weak,
                                        format!("Station queued \u{2014} {queued} tracks after this one"),
                                    ),
                                }
                                // If the queue sidebar is open, show the new tracks.
                                if sidebar_mode == 1 {
                                    refresh_sidebar(
                                        &weak, &self_tx, sidebar_mode, &mut sidebar_tracks,
                                        &last, &mut queue_mirror, &mut sidebar_generation,
                                    )
                                    .await;
                                }
                            } else {
                                // Nothing playing: this is the one case where a station
                                // should take over, because there is nothing to disturb.
                                station_active = true;
                                engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
                                station_queued.extend(picked.iter().cloned());
                                launched_src = None;
                                if let Err(e) = session.play_uris(&picked).await {
                                    set_status(&weak, format!("Station \u{2014} {e}"));
                                } else {
                                    set_status(
                                        &weak,
                                        format!("Station started \u{2014} {} tracks", picked.len()),
                                    );
                                    // No Track metadata for `picked[1..]` yet (station
                                    // resolve only returns uris); `refresh_sidebar`'s
                                    // live-queue fallback backfills a real mirror the
                                    // next time the QUEUE panel is opened. Always runs,
                                    // even for a single-track "queue" (`picked[1..]` is
                                    // then empty), so a stale persisted queue from
                                    // *before* this station doesn't linger.
                                    queue_mirror = None;
                                    save_queue(&picked[0], &picked[1..]);
                                }
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            }
                        }
                        engine::Event::Exited => {
                            push_engine_status(&weak, "Not running".to_string(), false);
                            // Ask who owns playback now — that is what separates "the
                            // user moved to another device" from "our audio sink died".
                            let moved = match session.devices().await {
                                Ok(devs) => another_device_active(&devs, ENGINE_DEVICE_NAME),
                                // No answer: assume nothing took over, since the sink
                                // failure is the case worth recovering from.
                                Err(_) => false,
                            };
                            if moved {
                                set_status(
                                    &weak,
                                    "Playback moved to another Spotify device \u{2014} Lightify\u{2019}s player stopped. Settings \u{2192} Restart to play here again."
                                        .to_string(),
                                );
                            } else if engine_watchdog.should_restart(std::time::Instant::now()) {
                                set_status(
                                    &weak,
                                    "Playback engine stopped (audio device lost?) \u{2014} restarting\u{2026}"
                                        .to_string(),
                                );
                                push_engine_status(&weak, "Restarting\u{2026}".to_string(), false);
                                // A beat, so a hard-failing engine cannot spin, and the
                                // OS has a moment to settle its new default device.
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                start_engine(&weak, &session, &self_tx);
                            } else {
                                set_status(
                                    &weak,
                                    "Playback engine keeps stopping \u{2014} check the audio output in Settings, then Restart"
                                        .to_string(),
                                );
                            }
                        }
                    },
                    Cmd::SelectOutput(i) => {
                        // Row 0 is "System default" (stored as an empty string, the same
                        // convention the shipped host's config uses).
                        let chosen = if i == 0 {
                            String::new()
                        } else {
                            engine::audio_outputs().get(i - 1).cloned().unwrap_or_default()
                        };
                        let dir = lightify_core::config::data_dir();
                        match lightify_core::config::update_config(&dir, |cfg| cfg.audio_output = chosen.clone()) {
                            Ok(()) => {
                                let label = if chosen.is_empty() { "the system default".to_string() } else { chosen.clone() };
                                set_status(&weak, format!("Audio output \u{2192} {label}"));
                                // Live, no restart: the engine's output thread moves to it
                                // on its next pass. An engine that is not running picks
                                // the saved choice up when it next starts.
                                if engine::running() {
                                    engine::send(
                                        &serde_json::json!({ "cmd": "output", "device": chosen }).to_string(),
                                    );
                                } else {
                                    start_engine(&weak, &session, &self_tx);
                                }
                                push_outputs(&weak);
                            }
                            Err(e) => set_status(&weak, format!("Settings \u{2014} {e}")),
                        }
                    }
                    Cmd::RestartEngine => {
                        push_engine_status(&weak, "Restarting\u{2026}".to_string(), false);
                        engine::shutdown();
                        start_engine(&weak, &session, &self_tx);
                    }
                    Cmd::ReclaimPlaybackAfterDownloaderLogin => {
                        // Only act if the engine has actually stopped running — that's
                        // the specific, narrow signal that the downloader's own device
                        // handoff knocked it offline (see `engine::Event::Exited`'s
                        // "moved" handling). If it's still running, it's either still
                        // the active device (nothing to do) or the user is genuinely,
                        // deliberately listening somewhere else right now (respect
                        // that — do NOT fight to reclaim it, same as `Event::Exited`
                        // already chooses not to).
                        if !engine::running() {
                            // A beat for the handoff to finish settling server-side
                            // before trying to re-adopt.
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            set_status(&weak, "Reclaiming playback after downloader sign-in\u{2026}".to_string());
                            push_engine_status(&weak, "Restarting\u{2026}".to_string(), false);
                            start_engine(&weak, &session, &self_tx);
                        }
                    }
                    Cmd::SetPollRate(ms) => {
                        let ms = ms.clamp(500, 60_000) as u64;
                        if ms != poll_ms {
                            poll_ms = ms;
                            ticker = poll_interval(Duration::from_millis(poll_ms));
                            ticker.tick().await; // consume the immediate first tick
                            next_playback_poll = tokio::time::Instant::now();
                        }
                    }
                }
            }
        }
        // Keep the "playing here" marks current. Cheap: rows are only touched when the
        // active source, the loaded track, or the play/pause state actually changed.
        sync_presence(&weak, &last, &launched_src, &mut presence);
        // A multi-selection is row indices, so once its list's rows change underneath
        // it (the queue panel advancing, a new search page, a chart swap) it points at
        // other rows. Drop it rather than let "Play N selected" act on those. The track
        // list is exempt: its selection is keyed to tracks and survives a re-sort.
        if sel_scope != SEL_NONE && sel_scope != SEL_TRACKS {
            let v = list_version(sel_scope as usize);
            if sel_version.0 != sel_scope {
                sel_version = (sel_scope, v);
            } else if sel_version.1 != v {
                let scope = sel_scope;
                drop_selection(&weak, scope, &mut selected, &mut sel_anchor, &mut sel_scope);
                sel_version = (SEL_NONE, 0);
            }
        } else {
            sel_version = (SEL_NONE, 0);
        }
    }
}

/// Re-mark the "playing here" rows when (and only when) the highlight actually
/// changes. Mirrors `refreshLibraryPlaybackPresence` + `refreshTrackPlaybackPresence`:
/// the rows are edited in place, so neither list loses its scroll position.
fn sync_presence(
    weak: &slint::Weak<MainWindow>,
    last: &Option<PlaybackState>,
    launched: &Option<String>,
    cache: &mut Presence,
) {
    let (id, playing) = active_source(last, launched);
    let uri = last
        .as_ref()
        .and_then(|p| p.track.as_ref())
        .map(|t| t.uri.clone())
        .unwrap_or_default();
    let next = Presence { source: id, track_uri: uri, playing };
    if next == *cache {
        return;
    }
    *cache = next.clone();
    let _ = weak.upgrade_in_event_loop(move |app| apply_presence(&app, &next));
}

/// What the library highlight is currently marking.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Presence {
    source: String,    // "liked" or a playlist id ("" = nothing)
    track_uri: String, // the loaded track's uri ("" = nothing)
    playing: bool,     // false = the source/track is loaded but paused
}

/// Apply `p` to the two live row models (UI thread). Split out from `sync_presence`
/// so `--selftest-presence` can drive it without an event loop.
fn apply_presence(app: &MainWindow, p: &Presence) {
    let rows = app.get_playlists();
    if let Some(vm) = rows.as_any().downcast_ref::<slint::VecModel<Row>>() {
        for i in 0..vm.row_count() {
            let Some(mut r) = vm.row_data(i) else { continue };
            let active = !p.source.is_empty() && r.id == p.source.as_str();
            if r.active != active || r.playing != (active && p.playing) {
                r.active = active;
                r.playing = active && p.playing;
                vm.set_row_data(i, r);
            }
        }
    }
    let tracks = app.get_tracks();
    if let Some(vm) = tracks.as_any().downcast_ref::<slint::VecModel<Trk>>() {
        for i in 0..vm.row_count() {
            let Some(mut t) = vm.row_data(i) else { continue };
            let active = !p.track_uri.is_empty() && t.uri == p.track_uri.as_str();
            if t.active != active || t.playing != (active && p.playing) {
                t.active = active;
                t.playing = active && p.playing;
                vm.set_row_data(i, t);
            }
        }
    }
}

/// The library sort a headless render should use, from an optional CLI token:
/// `alpha` (A->Z), `alpha-desc` (Z->A); anything else keeps "Recent".
fn parse_sort_arg(arg: Option<&String>) -> (String, String) {
    match arg.map(|s| s.as_str()) {
        Some("alpha") => ("alpha".into(), "asc".into()),
        Some("alpha-desc") | Some("desc") => ("alpha".into(), "desc".into()),
        _ => default_sort(),
    }
}

/// The toolbar's startup state: Recent, ascending (matches the .slint defaults).
fn default_sort() -> (String, String) {
    ("recent".into(), "asc".into())
}

/// Engine row for a render that hasn't started one (most `--shot*` modes).
fn default_engine() -> (String, bool) {
    match engine::find_binary() {
        Some(_) => ("Not started (headless render)".into(), false),
        None => ("Not found".into(), false),
    }
}

/// Headless proof that Lightify can play on its own: start the bundled engine, wait
/// for it to register as a Spotify Connect device, and confirm the Web API can see
/// and target it. If this passes, the Spotify desktop app is not needed for playback.
/// Prove a station resolves to real tracks **without playing anything** — the whole
/// point of the queue-don't-interrupt change. Starts the engine, asks it to resolve
/// the seed's radio, and prints what came back.
/// The queue-authority regression, run against the real account.
///
/// Reproduces exactly what went wrong: play a track, start a station (which queues
/// tracks on the device), then start a playlist — and check the playlist's own
/// up-next is what plays, not the station's leftovers. Reasoning alone is not enough
/// here; the whole class of bug is about residue surviving a lane change.
///
/// Leaves playback paused.
async fn selftest_queue_authority() {
    println!("queue authority");
    println!("{:-<72}", "");
    let mut fails: Vec<String> = Vec::new();
    let check = |cond: bool, what: &str, fails: &mut Vec<String>| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };

    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  session: {e}\nFAIL");
            return;
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        println!("  session: {e}\nFAIL");
        return;
    }
    let Some(bin) = engine::find_binary() else {
        println!("  engine binary missing\nFAIL");
        return;
    };

    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).ok().flatten();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        println!("  engine spawn: {e}\nFAIL");
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut device = String::new();
    while std::time::Instant::now() < deadline && device.is_empty() {
        if let Ok(engine::Event::Ready { device_id }) = rx.recv_timeout(Duration::from_millis(500)) {
            device = device_id;
        }
    }
    if device.is_empty() {
        println!("  engine never registered\nFAIL");
        return;
    }
    if let Err(e) = adopt_engine_device(&mut session, &device).await {
        println!("  adopt: {e}\nFAIL");
        return;
    }

    // A seed to play, and a playlist to switch to afterwards.
    let seed = match session.saved_tracks_page(0).await {
        Ok(page) => match page.tracks.into_iter().find(|t| !t.uri.is_empty()) {
            Some(t) => t,
            None => {
                println!("  no liked tracks to seed with\nFAIL");
                return;
            }
        },
        Err(e) => {
            println!("  liked songs: {e}\nFAIL");
            return;
        }
    };
    let playlist = match session.playlists().await {
        Ok(p) => p.into_iter().find(|p| p.tracks > 3 && !p.uri.is_empty()),
        Err(_) => None,
    };
    let Some(playlist) = playlist else {
        println!("  no usable playlist\nFAIL");
        return;
    };
    println!("  seed: {} | switching to: {}", seed.name, playlist.name);

    // Spotify can take a moment to mark a freshly adopted device active, so give the
    // first play a few tries rather than reading a startup race as a failure.
    let mut played = false;
    for attempt in 0..4 {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        match session.play_uris(&[seed.uri.clone()]).await {
            Ok(()) => { played = true; break; }
            Err(e) => {
                if attempt == 3 { println!("  play seed: {e}\nFAIL"); return; }
                let _ = session.transfer_playback(&device, false).await;
            }
        }
    }
    check(played, "the engine's device accepts playback", &mut fails);
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Start a station the way the app does: resolve, then queue behind the seed.
    engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
    engine::send(
        &serde_json::json!({ "cmd": "stationtracks", "context_uri": seed.uri }).to_string(),
    );
    let mut station: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline && station.is_empty() {
        if let Ok(engine::Event::StationTracks { uris, .. }) = rx.recv_timeout(Duration::from_millis(500)) {
            station = uris;
        }
    }
    check(!station.is_empty(), "station resolves to tracks", &mut fails);
    let picked: Vec<String> =
        station.iter().filter(|u| **u != seed.uri).take(5).cloned().collect();
    for uri in &picked {
        let _ = session.add_to_queue(uri).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    let queued_now = session.queue().await.unwrap_or_default();
    let station_in_queue = queued_now.iter().filter(|t| picked.contains(&t.uri)).count();
    check(station_in_queue > 0, "station tracks really are queued on the device", &mut fails);

    // ── the actual regression: start a playlist while that station queue is live ──
    let mut active = true;
    let mut queued_set: std::collections::HashSet<String> = picked.iter().cloned().collect();
    let mut bp_autoplay_unused: Option<BpAutoplay> = None;
    let mut bp_queued_unused: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bp_active_seq_unused = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let weak = slint::Weak::<MainWindow>::default();
    // `device` really is the active device at this point (playback was just driven
    // onto it above), so a fresh read gives `end_autoplay_authority` the same
    // ownership signal the live app would have — exercising the real reset path
    // rather than trivially short-circuiting on the ownership guard.
    let last_pb = session.playback().await.ok().flatten();
    end_autoplay_authority(
        &weak, &mut session, &mut active, &mut queued_set,
        &mut bp_autoplay_unused, &mut bp_queued_unused,
        &bp_active_seq_unused, &last_pb, &device,
    )
    .await;
    check(!active, "starting a new session drops the station lane", &mut fails);

    if let Err(e) = session.play_context_start(&playlist.uri).await {
        println!("  play playlist after reset: {e}");
        fails.push("playlist play after reset".into());
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    let after = session.queue().await.unwrap_or_default();
    let leftovers = after.iter().filter(|t| picked.contains(&t.uri)).count();
    check(leftovers == 0, "no station leftovers survive into the new playlist", &mut fails);
    check(!after.is_empty(), "the playlist supplies its own up-next", &mut fails);
    if !after.is_empty() {
        println!("      up next: {}", after.iter().take(3).map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));
    }
    let pb = session.playback().await.ok().flatten();
    let ctx_ok = pb
        .as_ref()
        .and_then(|p| p.context_uri.clone())
        .is_some_and(|c| c == playlist.uri);
    check(ctx_ok, "...and playback really is in the playlist's context", &mut fails);

    let _ = session.pause().await;
    if fails.is_empty() {
        println!("PASS: a new session takes the queue back from a station");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// The ordering rule behind the Beatport start (`BP_INITIAL_MATCH`): Spotify plays queued
/// tracks BEFORE what remains of the play context. So "play the first track, queue the
/// rest" keeps chart order, while "play two, queue the rest" puts the second last.
/// Checked against the real account, because it is Spotify's behaviour, not ours.
/// Same conditions as `--selftest-station-clear`: Lightify closed, a few quiet seconds.
async fn selftest_queue_order() {
    println!("queue order: play one + queue the rest vs play two + queue the rest");
    println!("{:-<72}", "");
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => return println!("  session: {e}\nFAIL"),
    };
    if let Err(e) = session.ensure_fresh().await {
        return println!("  session: {e}\nFAIL");
    }
    let Some(bin) = engine::find_binary() else { return println!("  engine missing\nFAIL") };
    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).ok().flatten();
    if engine::start(&bin, session.access_token(), output.as_deref(), move |ev| { let _ = tx.send(ev); }).is_err() {
        return println!("  engine spawn failed\nFAIL");
    }
    let mut device = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while std::time::Instant::now() < deadline && device.is_empty() {
        if let Ok(engine::Event::Ready { device_id }) = rx.recv_timeout(Duration::from_millis(500)) {
            device = device_id;
        }
    }
    if device.is_empty() || adopt_engine_device(&mut session, &device).await.is_err() {
        return println!("  no device\nFAIL");
    }
    let volume_before = session.playback().await.ok().flatten().map(|p| p.volume_percent);
    let _ = session.set_volume(6).await;
    let liked: Vec<String> = session
        .saved_tracks_page(0)
        .await
        .map(|p| p.tracks.into_iter().map(|t| t.uri).filter(|u| !u.is_empty()).collect())
        .unwrap_or_default();
    let mut fails: Vec<String> = Vec::new();
    if liked.len() < 6 {
        println!("  need 6 liked songs\nFAIL");
        return;
    }
    let mut check = |cond: bool, what: &str| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };
    let (a, b, c, d, e) = (&liked[0], &liked[1], &liked[2], &liked[3], &liked[4]);

    // The fix: one track played, the rest queued in order.
    let mut started = false;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        if session.play_uris(&[a.clone()]).await.is_ok() { started = true; break; }
    }
    check(started, "playback starts");
    for u in [b, c, d, e] {
        let _ = session.add_to_queue(u).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let q = queue_uris(&mut session).await;
    let head: Vec<&String> = q.iter().take(4).collect();
    check(head == vec![b, c, d, e], "play 1 + queue the rest: the queue keeps the order it was given");

    // The old start: two played as a context, the rest queued.
    engine::send(&clear_queue_command(&None, false));
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = session.play_uris(&[a.clone(), b.clone()]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    for u in [c, d, e] {
        let _ = session.add_to_queue(u).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let q = queue_uris(&mut session).await;
    let head: Vec<&String> = q.iter().take(3).collect();
    check(
        head == vec![c, d, e],
        "play 2 + queue the rest: the queued tracks jump ahead of the context's 2nd track (why the start plays only one)",
    );
    let _ = session.pause().await;
    if let Some(v) = volume_before {
        let _ = session.set_volume(v).await;
    }
    if fails.is_empty() {
        println!("PASS: Spotify plays queued tracks before the rest of the play context");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// Every uri in the device's up-next, in order.
async fn queue_uris(session: &mut Session) -> Vec<String> {
    session.queue().await.unwrap_or_default().into_iter().map(|t| t.uri).collect()
}

/// Ask the engine for a seed's radio and wait for it.
fn resolve_station(rx: &std::sync::mpsc::Receiver<engine::Event>, seed: &str) -> Vec<String> {
    engine::send(&serde_json::json!({ "cmd": "stationtracks", "context_uri": seed }).to_string());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(engine::Event::StationTracks { uris, .. }) = rx.recv_timeout(Duration::from_millis(500)) {
            return uris;
        }
    }
    Vec::new()
}

/// The reported bug, run against the real account: start a station (its tracks are
/// queued), play a new song, press Clear, start another station — and the queue and
/// "next" must belong to the NEW station only. Also the queue's Remove-track rebuild.
///
/// Plays a few seconds of a liked song on this PC at low volume (restored after) and
/// leaves playback paused. Needs Lightify itself closed: it registers its own device.
async fn selftest_station_clear() {
    println!("station -> new song -> clear -> new station");
    println!("{:-<72}", "");
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => return println!("  session: {e}\nFAIL"),
    };
    if let Err(e) = session.ensure_fresh().await {
        return println!("  session: {e}\nFAIL");
    }
    let Some(bin) = engine::find_binary() else {
        return println!("  engine binary missing\nFAIL");
    };
    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).ok().flatten();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        return println!("  engine spawn: {e}\nFAIL");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut device = String::new();
    while std::time::Instant::now() < deadline && device.is_empty() {
        if let Ok(engine::Event::Ready { device_id }) = rx.recv_timeout(Duration::from_millis(500)) {
            device = device_id;
        }
    }
    if device.is_empty() {
        return println!("  engine never registered\nFAIL");
    }
    if let Err(e) = adopt_engine_device(&mut session, &device).await {
        return println!("  adopt: {e}\nFAIL");
    }
    // Quiet while testing, put back afterwards.
    let volume_before = session.playback().await.ok().flatten().map(|p| p.volume_percent);
    let _ = session.set_volume(6).await;
    let fails = station_clear_scenario(&mut session, &rx).await;
    let _ = session.pause().await;
    if let Some(v) = volume_before {
        let _ = session.set_volume(v).await;
    }
    if fails.is_empty() {
        println!("PASS: Clear really clears, and a new station owns the queue");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

async fn station_clear_scenario(
    session: &mut Session,
    rx: &std::sync::mpsc::Receiver<engine::Event>,
) -> Vec<String> {
    let mut fails: Vec<String> = Vec::new();
    let mut check = |cond: bool, what: &str| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };
    let liked = match session.saved_tracks_page(0).await {
        Ok(p) => p.tracks.into_iter().filter(|t| !t.uri.is_empty()).take(2).collect::<Vec<_>>(),
        Err(e) => {
            println!("  liked songs: {e}");
            return vec!["liked songs".into()];
        }
    };
    if liked.len() < 2 {
        println!("  need two liked songs");
        return vec!["liked songs".into()];
    }
    let (s0, s1) = (liked[0].clone(), liked[1].clone());
    println!("  song 1: {} | song 2: {}", s0.name, s1.name);

    let mut played = false;
    for attempt in 0..4 {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        match session.play_uris(&[s0.uri.clone()]).await {
            Ok(()) => { played = true; break; }
            Err(e) => {
                if attempt == 3 { println!("  play: {e}"); }
            }
        }
    }
    check(played, "the engine's device accepts playback");
    if !played {
        return fails;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Station A, queued the way the app does it ──
    engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
    let a_all = resolve_station(rx, &s0.uri);
    let station_b_all = resolve_station(rx, &s1.uri);
    let a: Vec<String> = a_all.iter().filter(|u| **u != s0.uri).take(6).cloned().collect();
    // Tracks only station A has, so an overlap between the two radios can't blur the check.
    let a_only: Vec<String> = a.iter().filter(|u| !station_b_all.contains(u)).cloned().collect();
    check(a_only.len() >= 2, "station A has tracks of its own to look for");
    for uri in &a {
        let _ = session.add_to_queue(uri).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let q = queue_uris(session).await;
    check(a_only.iter().any(|u| q.contains(u)), "station A's tracks are queued");

    // ── Find a new song: a plain play, as a click on a search hit does ──
    let _ = session.play_uris(&[s1.uri.clone()]).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let q = queue_uris(session).await;
    println!(
        "      after playing the new song, {} of A's own tracks are still queued",
        a_only.iter().filter(|u| q.contains(u)).count()
    );

    // ── Clear the queue: exactly what the button asks of the engine ──
    let before = session.playback().await.ok().flatten();
    let pos_before = before.as_ref().map(|p| p.progress_ms).unwrap_or(0);
    let cur = before.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default();
    check(cur == s1.uri, "the new song is what is playing before Clear");
    // Ground truth is the engine's own events: it pauses for the reset, then plays the
    // same song again from where it was. (Spotify's Web API view lags, so it is only
    // reported below, not relied on.)
    while rx.try_recv().is_ok() {}
    clear_device_queue(session, &before).await;
    let (mut saw_pause, mut playing_again, mut first_pos) = (false, false, None::<u64>);
    let resume_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < resume_deadline && first_pos.is_none() {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(engine::Event::Playing { playing: false }) => saw_pause = true,
            Ok(engine::Event::Playing { playing: true }) if saw_pause => playing_again = true,
            Ok(engine::Event::Position { ms }) if playing_again => first_pos = Some(ms),
            _ => {}
        }
    }
    // Give the device time to be seen by Spotify again, then read both views.
    let mut after: Option<PlaybackState> = None;
    let seen_from = std::time::Instant::now();
    let mut lag_ms = 0u128;
    for _ in 0..24 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        after = session.playback().await.ok().flatten();
        lag_ms = seen_from.elapsed().as_millis();
        if after.as_ref().is_some_and(|p| p.is_playing && p.track.as_ref().is_some_and(|t| t.uri == s1.uri)) {
            break;
        }
    }
    println!(
        "      engine: paused={saw_pause} playing again={playing_again} resumed at {:?}ms (was {pos_before}ms) | web api saw it after {lag_ms}ms: {}",
        first_pos,
        match after.as_ref() {
            Some(p) => format!(
                "playing={} track={} at {}ms",
                p.is_playing,
                p.track.as_ref().map(|t| t.name.as_str()).unwrap_or("-"),
                p.progress_ms
            ),
            None => "no player state".to_string(),
        }
    );
    let q = queue_uris(session).await;
    let left_a = a_only.iter().filter(|u| q.contains(u)).count();
    check(left_a == 0, &format!("Clear removes the previous station's tracks ({left_a} left)"));
    check(
        playing_again && first_pos.is_some_and(|ms| ms + 1500 >= pos_before),
        "...and the song that was playing resumes where it was, not restarted",
    );
    check(
        after.as_ref().is_some_and(|p| p.is_playing && p.track.as_ref().is_some_and(|t| t.uri == s1.uri)),
        "...and Spotify sees it playing again",
    );

    // ── Start the new station ──
    engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
    let b: Vec<String> = station_b_all.iter().filter(|u| **u != s1.uri).take(6).cloned().collect();
    for uri in &b {
        let _ = session.add_to_queue(uri).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let q = queue_uris(session).await;
    check(b.iter().any(|u| q.contains(u)), "the new station's tracks are queued");
    check(!a_only.iter().any(|u| q.contains(u)), "...and none of the old station's");
    check(q.first().is_some_and(|u| b.contains(u)), "what plays next is the new station's first track");

    // ── Remove one queued track: clear, then re-add the others (the app's rebuild) ──
    let show: Vec<String> = q.iter().take(4).cloned().collect();
    if show.len() >= 3 {
        let removed = show[1].clone();
        let kept: Vec<String> = show.iter().filter(|u| **u != removed).cloned().collect();
        let now = session.playback().await.ok().flatten();
        clear_device_queue(session, &now).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        for uri in &kept {
            let _ = session.add_to_queue(uri).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let q = queue_uris(session).await;
        let head: Vec<String> = q.iter().take(kept.len()).cloned().collect();
        check(head == kept, "removing a queued track leaves exactly the others, in order");
        check(!q.contains(&removed), "...and the removed one is gone");
        let dupes = q.len() - q.iter().collect::<std::collections::HashSet<_>>().len();
        check(dupes == 0, &format!("...with no duplicates ({dupes})"));
    }

    // ── Clear while PAUSED: must stay silent, stay paused, and Spotify must agree ──
    let _ = session.pause().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let marker: Vec<String> = station_b_all.iter().filter(|u| **u != s1.uri).skip(6).take(2).cloned().collect();
    for uri in &marker {
        let _ = session.add_to_queue(uri).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let paused_state = session.playback().await.ok().flatten();
    check(paused_state.as_ref().is_some_and(|p| !p.is_playing), "paused before the second Clear");
    while rx.try_recv().is_ok() {}
    clear_device_queue(session, &paused_state).await;
    let mut made_sound = false;
    let quiet_until = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < quiet_until {
        if let Ok(engine::Event::Playing { playing: true }) = rx.recv_timeout(Duration::from_millis(250)) {
            made_sound = true;
        }
    }
    check(!made_sound, "clearing while paused makes no sound");
    let after_paused = session.playback().await.ok().flatten();
    check(
        after_paused.as_ref().is_some_and(|p| !p.is_playing && p.track.as_ref().is_some_and(|t| t.uri == s1.uri)),
        "...the song is still loaded, and Spotify agrees it is paused",
    );
    let q = queue_uris(session).await;
    check(!marker.iter().any(|u| q.contains(u)), "...and the queue was emptied");
    fails
}

/// Experiment: after the engine empties its queue, how long until the Web API's view of
/// the device (`me/player`) agrees with what the engine is really doing — and which
/// nudge gets it there? Prints a timeline; changes nothing lasting.
async fn probe_clear_sync(strategy: &str, paused: bool) {
    println!("clear-sync probe: strategy={strategy} paused={paused}");
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => return println!("  session: {e}"),
    };
    if let Err(e) = session.ensure_fresh().await {
        return println!("  session: {e}");
    }
    let Some(bin) = engine::find_binary() else { return println!("  engine missing") };
    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).ok().flatten();
    if engine::start(&bin, session.access_token(), output.as_deref(), move |ev| { let _ = tx.send(ev); }).is_err() {
        return println!("  engine spawn failed");
    }
    let mut device = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while std::time::Instant::now() < deadline && device.is_empty() {
        if let Ok(engine::Event::Ready { device_id }) = rx.recv_timeout(Duration::from_millis(500)) {
            device = device_id;
        }
    }
    if device.is_empty() || adopt_engine_device(&mut session, &device).await.is_err() {
        return println!("  no device");
    }
    let volume_before = session.playback().await.ok().flatten().map(|p| p.volume_percent);
    let _ = session.set_volume(6).await;
    let liked: Vec<Track> = session.saved_tracks_page(0).await.map(|p| p.tracks).unwrap_or_default();
    if liked.len() < 5 {
        return println!("  need 5 liked songs");
    }
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        if session.play_uris(&[liked[0].uri.clone()]).await.is_ok() { break; }
    }
    for t in &liked[2..5] {
        let _ = session.add_to_queue(&t.uri).await;
    }
    let _ = session.play_uris(&[liked[1].uri.clone()]).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    if paused {
        let _ = session.pause().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let before = session.playback().await.ok().flatten();
    let (cur, pos) = before
        .as_ref()
        .map(|p| (p.track.as_ref().map(|t| t.uri.clone()).unwrap_or_default(), p.progress_ms))
        .unwrap_or_default();
    println!("  before: playing={:?} pos={pos}ms", before.as_ref().map(|p| p.is_playing));
    while rx.try_recv().is_ok() {}
    let t0 = std::time::Instant::now();
    match strategy {
        "play" => {
            engine::send(&serde_json::json!({ "cmd": "clearqueue" }).to_string());
            tokio::time::sleep(Duration::from_millis(600)).await;
            for _ in 0..4 {
                match session.play_at(None, &cur, pos).await {
                    Ok(()) => break,
                    Err(e) => { println!("    play_at: {e}"); tokio::time::sleep(Duration::from_millis(500)).await; }
                }
            }
            if paused {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = session.pause().await;
            }
        }
        _ => {
            engine::send(&serde_json::json!({
                "cmd": "clearqueue", "keep_uri": cur, "position_ms": pos, "playing": !paused,
            }).to_string());
            tokio::time::sleep(Duration::from_millis(1500)).await;
            match session.seek(pos).await {
                Ok(()) => println!("    seek ok"),
                Err(e) => println!("    seek: {e}"),
            }
        }
    }
    // Timeline: what the Web API says vs what the engine last told us.
    let mut eng_playing = None;
    let mut eng_pos = None;
    for i in 0..16 {
        tokio::time::sleep(Duration::from_millis(750)).await;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                engine::Event::Playing { playing } => eng_playing = Some(playing),
                engine::Event::Position { ms } => eng_pos = Some(ms),
                _ => {}
            }
        }
        let api = session.playback().await.ok().flatten();
        let q = session.queue().await.unwrap_or_default();
        let leftovers = q.iter().filter(|t| liked[2..5].iter().any(|l| l.uri == t.uri)).count();
        println!(
            "  t+{:>5}ms  engine: playing={:?} pos={:?} | web api: playing={:?} pos={:?} track_ok={} | queued leftovers={leftovers}",
            t0.elapsed().as_millis(),
            eng_playing,
            eng_pos,
            api.as_ref().map(|p| p.is_playing),
            api.as_ref().map(|p| p.progress_ms),
            api.as_ref().and_then(|p| p.track.as_ref()).is_some_and(|t| t.uri == cur),
        );
        let _ = i;
    }
    let _ = session.pause().await;
    if let Some(v) = volume_before {
        let _ = session.set_volume(v).await;
    }
}

async fn probe_station(seed: &str) {
    println!("station resolve probe");
    println!("{:-<72}", "");
    let seed = if seed.starts_with("spotify:track:") {
        seed.to_string()
    } else {
        format!("spotify:track:{seed}")
    };
    println!("  seed: {seed}");

    let Some(bin) = engine::find_binary() else {
        println!("  binary: NOT FOUND\nFAIL");
        return;
    };
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  session: LOAD ERROR {e}\nFAIL");
            return;
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        println!("  session: AUTH ERROR {e}\nFAIL");
        return;
    }
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).ok().flatten();

    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        println!("  spawn: ERROR {e}\nFAIL");
        return;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut ready = false;
    let mut asked = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(engine::Event::Ready { .. }) => {
                ready = true;
                println!("  engine ready; resolving the station\u{2026}");
                engine::send(
                    &serde_json::json!({ "cmd": "stationtracks", "context_uri": seed }).to_string(),
                );
                asked = true;
            }
            Ok(engine::Event::StationTracks { uris, .. }) => {
                println!("  resolved {} tracks (nothing was loaded or played):", uris.len());
                for u in uris.iter().take(5) {
                    println!("    - {u}");
                }
                if uris.len() > 5 {
                    println!("    \u{2026} and {} more", uris.len() - 5);
                }
                println!("{}", if uris.len() > 1 { "PASS" } else { "FAIL (a station needs more than one track)" });
                return;
            }
            Ok(engine::Event::Failed { msg }) => {
                println!("  engine error: {msg}\nFAIL");
                return;
            }
            Ok(engine::Event::Exited) => {
                println!("  engine exited (see lightify-shell-audio.log)\nFAIL");
                return;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    println!(
        "  timed out (ready={ready}, asked={asked})\nFAIL"
    );
}

async fn probe_engine() {
    println!("playback engine probe");
    println!("{:-<72}", "");

    let bin = match engine::find_binary() {
        Some(b) => b,
        None => {
            println!("  binary: NOT FOUND");
            println!("  build it: cd lightify-audio && cargo build --release");
            println!("FAIL");
            return;
        }
    };
    println!("  binary:      {}", bin.display());
    println!("  cache:       {}", engine::cache_dir().display());
    println!(
        "  cached login: {}",
        if engine::has_cached_login() { "yes (silent start)" } else { "no (browser sign-in once)" }
    );

    // Audio output resolution. A stale name here is fatal to the engine (librespot's
    // rodio backend unwraps the device lookup), so print exactly what was configured,
    // what actually exists, and what will be used.
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    println!("  configured output: {configured:?}");
    for (i, name) in engine::audio_outputs().iter().enumerate() {
        println!("    {} {}", if i == 0 { "*" } else { " " }, name);
    }
    let output = match engine::resolve_output(&configured) {
        Ok(None) => {
            println!("  using output: system default");
            None
        }
        Ok(Some(name)) => {
            println!("  using output: {name}");
            Some(name)
        }
        Err(missing) => {
            println!("  using output: system default (configured {missing:?} is NOT available)");
            None
        }
    };

    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  session: LOAD ERROR {e}");
            println!("FAIL");
            return;
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        println!("  session: AUTH ERROR {e}");
        println!("FAIL");
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        println!("  spawn: ERROR {e}");
        println!("FAIL");
        return;
    }
    println!("  spawned, waiting for the Connect device to register\u{2026}");

    // The one-time keymaster sign-in (if it has never run) opens a browser, so allow
    // a generous window; a cached login registers in a couple of seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut device_id = String::new();
    while std::time::Instant::now() < deadline && device_id.is_empty() {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(engine::Event::Ready { device_id: id }) => {
                println!("  event ready:  device_id={id}");
                device_id = id;
            }
            Ok(engine::Event::NeedAuth { msg }) => println!("  event needauth: {msg}"),
            Ok(engine::Event::Failed { msg }) => {
                println!("  event error:  {msg}");
                println!("FAIL");
                return;
            }
            Ok(engine::Event::Exited) => {
                println!("  engine exited before registering (see lightify-shell-audio.log)");
                println!("FAIL");
                return;
            }
            Ok(other) => println!("  event:        {other:?}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    if device_id.is_empty() {
        println!("  timed out waiting for the device to register");
        println!("FAIL");
        return;
    }

    // Now the part that actually matters: can the Web API see and target it?
    // Registration propagates a beat after `ready`, so poll rather than snapshot.
    println!("  waiting for Spotify to list the device\u{2026}");
    let ours = find_engine_device(&mut session, &device_id, 15).await;
    let devices = session.devices().await.unwrap_or_default();
    println!("  devices seen by the Web API: {}", devices.len());
    for d in &devices {
        println!(
            "    - {:<28} id={} active={}",
            d.name,
            &d.id[..d.id.len().min(12)],
            d.is_active
        );
    }
    match ours {
        Some(d) => {
            print!("  transferring playback to \"{}\"\u{2026} ", d.name);
            match session.transfer_playback(&d.id, false).await {
                Ok(()) => println!("ok"),
                Err(e) => {
                    println!("FAILED: {e}");
                    println!("FAIL");
                    return;
                }
            }
            println!("{:-<72}", "");
            println!("PASS \u{2014} Lightify is its own playback device; Spotify's app is not required");
        }
        None => {
            println!("  the engine registered but the Web API doesn't list it yet");
            println!("FAIL");
        }
    }
}

/// End-to-end playback check: start the engine, adopt its device, actually PLAY a
/// track, and prove audio reached the sink.
///
/// `--probe-engine` only proves the device registers and can be transferred to —
/// which stayed green while real playback was broken, because the break was in how
/// transport calls addressed the device (no `?device_id=`, plus a redundant transfer
/// that 500s). This probe is the one that would have caught that: it asserts the
/// Web API reports `is_playing`, that the position actually advances, and that the
/// engine's own log shows `sink.write` packets.
/// Live test of switching audio output while playing, against real WASAPI devices.
///
/// The requirement is "a device change lands in under a second and never takes the
/// engine down". This measures both, in two parts:
///
/// A. **The sink on its own** - `SwitchableSink` fed silence by a thread that behaves
///    like librespot's writer, switched between real devices. Silent, touches no
///    system setting, needs no Spotify. Measures switch latency, and the longest any
///    `write` blocked - the old rodio sink could block forever.
/// B. **The whole engine, playing a real track**, switched between devices through
///    the engine's own `output` command. Asserts the switch lands under a second,
///    the engine never exits, and playback position keeps advancing afterwards
///    (i.e. the player thread did not hang).
///
/// It does not change the Windows default device - that is a system setting - but
/// that path runs the same switching code; only how the target is chosen differs,
/// and A checks that choosing the OS default resolves to Windows' actual default.
async fn probe_output() {
    use librespot_playback::audio_backend::Sink;
    use librespot_playback::convert::Converter;
    use librespot_playback::decoder::AudioPacket;

    println!("output switching probe");
    println!("{:-<72}", "");
    let outputs = engine::audio_outputs();
    let Some(default) = outputs.first().cloned() else {
        println!("  no audio outputs at all");
        println!("FAIL");
        return;
    };
    // Prefer a virtual device for the "other" output so nothing audible changes.
    let other = outputs
        .iter()
        .skip(1)
        .find(|n| n.to_ascii_lowercase().contains("cable"))
        .or_else(|| outputs.get(1))
        .cloned();
    let Some(other) = other else {
        println!("  only one output device ({default}); nothing to switch to");
        println!("FAIL");
        return;
    };
    println!("  default: {default}");
    println!("  other:   {other}");
    const LIMIT: Duration = Duration::from_millis(1000);
    let mut fails: Vec<String> = Vec::new();

    // ── A. the sink alone ──────────────────────────────────────────────────────
    println!("\nA. SwitchableSink, fed silence");
    let (tx, rx) = std::sync::mpsc::channel::<(std::time::Instant, String)>();
    let log: std::sync::Arc<dyn Fn(String) + Send + Sync> = std::sync::Arc::new(|_m| {});
    let on_change: std::sync::Arc<dyn Fn(String) + Send + Sync> = {
        let tx = std::sync::Mutex::new(tx);
        std::sync::Arc::new(move |d: String| {
            let _ = tx.lock().unwrap().send((std::time::Instant::now(), d));
        })
    };
    let sink = std::sync::Arc::new(std::sync::Mutex::new(audio_output::SwitchableSink::new(
        None,
        log,
        on_change,
    )));
    let stop_writer = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let sink = std::sync::Arc::clone(&sink);
        let stop = std::sync::Arc::clone(&stop_writer);
        std::thread::spawn(move || {
            // Same shape as librespot's feed: ~2048-sample packets, as fast as the sink
            // accepts them. Returns (packets written, longest single write).
            let mut conv = Converter::new(None);
            let mut worst = Duration::ZERO;
            let mut n = 0u64;
            let _ = sink.lock().unwrap().start();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let t = std::time::Instant::now();
                let _ = sink.lock().unwrap().write(AudioPacket::Samples(vec![0.0; 2048]), &mut conv);
                worst = worst.max(t.elapsed());
                n += 1;
            }
            (n, worst)
        })
    };

    let wait_for = |want: &str, since: std::time::Instant| -> Option<Duration> {
        let deadline = since + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok((at, dev)) = rx.recv_timeout(Duration::from_millis(50)) {
                if dev == want {
                    return Some(at.saturating_duration_since(since));
                }
            }
        }
        None
    };

    let t0 = std::time::Instant::now();
    match wait_for(&default, t0) {
        Some(d) => println!("  opened on the OS default in {} ms", d.as_millis()),
        None => {
            println!("  FAIL never opened on the OS default ({default})");
            fails.push("A: initial open".into());
        }
    }
    for (label, target, expect) in [
        ("switch to other", Some(other.clone()), other.clone()),
        ("back to OS default", None, default.clone()),
        // A pinned device that does not exist must fall back, not stop the music.
        ("pinned but missing", Some("No Such Device (Lightify probe)".to_string()), default.clone()),
        ("switch to other again", Some(other.clone()), other.clone()),
    ] {
        let t = std::time::Instant::now();
        audio_output::set_target(target);
        match wait_for(&expect, t) {
            Some(d) if d < LIMIT => println!("  ok   {label}: {} ms  -> {expect}", d.as_millis()),
            Some(d) => {
                println!("  FAIL {label}: {} ms (over {} ms)", d.as_millis(), LIMIT.as_millis());
                fails.push(format!("A: {label} too slow"));
            }
            None => {
                println!("  FAIL {label}: never landed on {expect}");
                fails.push(format!("A: {label} never landed"));
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    // Steady state while pinned and healthy must not enumerate devices at all - the
    // first version did, every 100 ms, which is ~150 ms of work each time.
    let scans_before = audio_output::scan_count();
    std::thread::sleep(Duration::from_secs(2));
    let idle_scans = audio_output::scan_count() - scans_before;
    if idle_scans == 0 {
        println!("  ok   2 s pinned and idle: 0 device enumerations");
    } else {
        println!("  FAIL 2 s pinned and idle: {idle_scans} device enumerations (want 0)");
        fails.push("A: steady-state enumeration".into());
    }
    stop_writer.store(true, std::sync::atomic::Ordering::SeqCst);
    let (written, worst) = writer.join().unwrap_or((0, Duration::MAX));
    println!("  writer: {written} packets, longest single write {} ms", worst.as_millis());
    if written < 50 {
        println!("  FAIL the writer barely moved - something is blocking it");
        fails.push("A: writer stalled".into());
    }
    if worst > Duration::from_millis(1500) {
        println!("  FAIL a write blocked for {} ms", worst.as_millis());
        fails.push("A: write blocked".into());
    }
    drop(sink);

    // ── B. the whole engine, playing ───────────────────────────────────────────
    println!("\nB. engine playing a real track");
    let Some(bin) = engine::find_binary() else {
        println!("  binary not found");
        println!("FAIL");
        return;
    };
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  session: {e}");
            println!("FAIL");
            return;
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        println!("  auth: {e}");
        println!("FAIL");
        return;
    }
    let (etx, erx) = std::sync::mpsc::channel::<engine::Event>();
    if let Err(e) = engine::start(&bin, session.access_token(), None, move |ev| {
        let _ = etx.send(ev);
    }) {
        println!("  spawn: {e}");
        println!("FAIL");
        return;
    }
    let mut device_id = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while std::time::Instant::now() < deadline && device_id.is_empty() {
        if let Ok(engine::Event::Ready { device_id: id }) = erx.recv_timeout(Duration::from_millis(200)) {
            device_id = id;
        }
    }
    if device_id.is_empty() {
        println!("  engine never registered");
        println!("FAIL");
        return;
    }
    if let Err(e) = adopt_engine_device(&mut session, &device_id).await {
        println!("  adopt: {e}");
        println!("FAIL");
        return;
    }
    let Some(track) = session.saved_tracks(1).await.ok().and_then(|t| t.into_iter().next()) else {
        println!("  no track to play");
        println!("FAIL");
        return;
    };
    if let Err(e) = session.play_uris(&[track.uri.clone()]).await {
        println!("  play: {e}");
        println!("FAIL");
        return;
    }
    println!("  playing: {} \u{2014} {}", track.name, track.artists);

    // Drain events for a while; report the latest position and whether it exited.
    let drain = |ms: u64| -> (Option<u64>, bool, Vec<String>) {
        let end = std::time::Instant::now() + Duration::from_millis(ms);
        let (mut pos, mut exited, mut outs) = (None, false, Vec::new());
        while std::time::Instant::now() < end {
            match erx.recv_timeout(Duration::from_millis(20)) {
                Ok(engine::Event::Position { ms }) => pos = Some(ms),
                Ok(engine::Event::Exited) => exited = true,
                Ok(engine::Event::OutputChanged { device }) => outs.push(device),
                _ => {}
            }
        }
        (pos, exited, outs)
    };
    let (pos0, _, _) = drain(3000);

    for (label, target, expect) in [
        ("switch to other while playing", other.clone(), other.clone()),
        ("back to the OS default", String::new(), default.clone()),
    ] {
        let t = std::time::Instant::now();
        engine::send(&serde_json::json!({ "cmd": "output", "device": target }).to_string());
        let mut landed = None;
        let mut exited = false;
        let mut last_pos = None;
        while t.elapsed() < Duration::from_secs(5) && landed.is_none() {
            match erx.recv_timeout(Duration::from_millis(20)) {
                Ok(engine::Event::OutputChanged { device }) if device == expect => landed = Some(t.elapsed()),
                Ok(engine::Event::Exited) => exited = true,
                Ok(engine::Event::Position { ms }) => last_pos = Some(ms),
                _ => {}
            }
        }
        // Playback must still be moving after the switch, not just "switched".
        let (pos_after, exited_after, _) = drain(2500);
        let advanced = match (last_pos.or(pos0), pos_after) {
            (Some(a), Some(b)) => b > a,
            (None, Some(_)) => true,
            _ => false,
        };
        match landed {
            Some(d) if d < LIMIT => println!("  ok   {label}: {} ms", d.as_millis()),
            Some(d) => {
                println!("  FAIL {label}: {} ms", d.as_millis());
                fails.push(format!("B: {label} too slow"));
            }
            None => {
                println!("  FAIL {label}: never landed on {expect}");
                fails.push(format!("B: {label} never landed"));
            }
        }
        if exited || exited_after {
            println!("  FAIL the engine exited during the switch");
            fails.push(format!("B: {label} engine exited"));
        }
        println!(
            "       position after: {} ({})",
            pos_after.map(|p| format!("{p} ms")).unwrap_or_else(|| "none".into()),
            if advanced { "still advancing" } else { "STALLED" }
        );
        if !advanced {
            fails.push(format!("B: {label} playback stalled"));
        }
        if !engine::running() {
            println!("  FAIL engine is no longer running");
            fails.push("B: engine stopped".into());
            break;
        }
    }
    let _ = session.pause().await;

    println!("{:-<72}", "");
    if fails.is_empty() {
        println!("PASS \u{2014} every switch under {} ms, engine never restarted, playback kept going", LIMIT.as_millis());
    } else {
        println!("FAIL \u{2014} {}", fails.join("; "));
    }
}

async fn probe_play() {
    println!("playback probe (end-to-end)");
    println!("{:-<72}", "");

    let Some(bin) = engine::find_binary() else {
        println!("  binary: NOT FOUND");
        println!("FAIL");
        return;
    };
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  session: LOAD ERROR {e}");
            println!("FAIL");
            return;
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        println!("  session: AUTH ERROR {e}");
        println!("FAIL");
        return;
    }

    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).unwrap_or(None);
    println!("  output: {}", output.as_deref().unwrap_or("system default"));

    // The engine recreates this log per run, so the whole file is this run's output.
    // (An earlier version diffed against the pre-start byte length, which silently
    // skipped real lines whenever the previous run's log had been longer.)
    let log_path = lightify_core::config::data_dir().join("lightify-shell-audio.log");

    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        println!("  spawn: ERROR {e}");
        println!("FAIL");
        return;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut device_id = String::new();
    while std::time::Instant::now() < deadline && device_id.is_empty() {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(engine::Event::Ready { device_id: id }) => device_id = id,
            Ok(engine::Event::Failed { msg }) => {
                println!("  engine error: {msg}");
                println!("FAIL");
                return;
            }
            Ok(engine::Event::Exited) => {
                println!("  engine exited before registering");
                println!("FAIL");
                return;
            }
            _ => {}
        }
    }
    if device_id.is_empty() {
        println!("  timed out waiting for the device");
        println!("FAIL");
        return;
    }
    println!("  engine ready: device_id={device_id}");

    match adopt_engine_device(&mut session, &device_id).await {
        Ok(name) => println!("  adopted: {name} (targeting {:?})", session.active_device()),
        Err(e) => {
            println!("  adopt FAILED: {e}");
            println!("FAIL");
            return;
        }
    }

    // A real track from the user's own library, so this exercises the same path the
    // UI uses rather than a hard-coded id that may not be playable on this account.
    let track = match session.saved_tracks(1).await {
        Ok(tracks) => tracks.into_iter().next(),
        Err(e) => {
            println!("  could not read Liked Songs: {e}");
            None
        }
    };
    let Some(track) = track else {
        println!("  no track available to test with");
        println!("FAIL");
        return;
    };
    println!("  playing: {} \u{2014} {}", track.name, track.artists);

    if let Err(e) = session.play_uris(&[track.uri.clone()]).await {
        println!("  play FAILED: {e}");
        println!("FAIL");
        return;
    }

    // Let it run, then check the account actually reports progress.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let state = session.playback().await;
    let mut ok = true;
    match &state {
        Ok(Some(pb)) => {
            println!(
                "  API says: playing={} pos={}ms device={}",
                pb.is_playing, pb.progress_ms, pb.device_name
            );
            if !pb.is_playing {
                println!("  FAIL Spotify does not report playback");
                ok = false;
            }
            if pb.progress_ms == 0 {
                println!("  FAIL position never advanced");
                ok = false;
            }
        }
        Ok(None) => {
            println!("  FAIL nothing is active on any device");
            ok = false;
        }
        Err(e) => {
            println!("  FAIL playback read error: {e}");
            ok = false;
        }
    }

    // The decisive one: did decoded audio reach the output sink?
    let log_after = std::fs::read_to_string(&log_path).unwrap_or_default();
    let wrote_audio = log_after.contains("sink.write");
    println!("  engine log: sink.write packets = {}", if wrote_audio { "yes" } else { "NO" });
    if !wrote_audio {
        println!("  FAIL no audio packets reached the output device");
        ok = false;
    }

    let _ = session.pause().await;
    println!("{:-<72}", "");
    println!("{}", if ok { "PASS \u{2014} audio really played through Lightify's own engine" } else { "FAIL" });
}

/// Does playing a *list* of URIs leave the rest of them in the up-next queue?
///
/// This is what "Play N selected" on a Beatport chart (and every other multi-track
/// play) depends on: `PUT me/player/play` with `uris` plays the first and queues the
/// remainder, which is what the QUEUE sidebar then shows. Worth its own probe because
/// it is the one behaviour that silently degrades if the play request is malformed or
/// aimed at the wrong device.
async fn probe_queue() {
    println!("queue-from-uris probe");
    println!("{:-<72}", "");

    let Some(bin) = engine::find_binary() else {
        println!("  binary: NOT FOUND");
        println!("FAIL");
        return;
    };
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  session: LOAD ERROR {e}");
            println!("FAIL");
            return;
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        println!("  session: AUTH ERROR {e}");
        println!("FAIL");
        return;
    }

    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).unwrap_or(None);

    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        println!("  spawn: ERROR {e}");
        println!("FAIL");
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut device_id = String::new();
    while std::time::Instant::now() < deadline && device_id.is_empty() {
        if let Ok(engine::Event::Ready { device_id: id }) = rx.recv_timeout(Duration::from_millis(500)) {
            device_id = id;
        }
    }
    if device_id.is_empty() {
        println!("  timed out waiting for the device");
        println!("FAIL");
        return;
    }
    if let Err(e) = adopt_engine_device(&mut session, &device_id).await {
        println!("  adopt FAILED: {e}");
        println!("FAIL");
        return;
    }

    let tracks = session.saved_tracks(5).await.unwrap_or_default();
    if tracks.len() < 3 {
        println!("  need at least 3 Liked Songs to test with");
        println!("FAIL");
        return;
    }
    let uris: Vec<String> = tracks.iter().take(5).map(|t| t.uri.clone()).collect();
    println!("  playing {} uris:", uris.len());
    for t in tracks.iter().take(5) {
        println!("    - {}", t.name);
    }
    if let Err(e) = session.play_uris(&uris).await {
        println!("  play FAILED: {e}");
        println!("FAIL");
        return;
    }

    tokio::time::sleep(Duration::from_secs(4)).await;
    let queued = session.queue().await.unwrap_or_default();
    println!("  me/player/queue returned {} upcoming tracks", queued.len());
    for t in queued.iter().take(6) {
        println!("    - {}", t.name);
    }

    // The Beatport chart refill tops the queue up with `add_to_queue` rather than
    // replaying a list, so prove that path too: it is a different endpoint.
    let mut appended = false;
    if let Some(extra) = tracks.get(4).or_else(|| tracks.last()) {
        match session.add_to_queue(&extra.uri).await {
            Ok(()) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let after = session.queue().await.unwrap_or_default();
                appended = after.len() > queued.len()
                    || after.iter().any(|t| t.uri == extra.uri);
                println!(
                    "  add_to_queue(\"{}\") -> queue {} -> {} ({})",
                    extra.name,
                    queued.len(),
                    after.len(),
                    if appended { "appended" } else { "NOT appended" }
                );
            }
            Err(e) => println!("  add_to_queue FAILED: {e}"),
        }
    }
    let _ = session.pause().await;

    println!("{:-<72}", "");
    if queued.len() >= uris.len() - 1 && appended {
        println!("PASS \u{2014} the rest of the list is queued and add_to_queue appends");
    } else if queued.len() >= uris.len() - 1 {
        println!("FAIL \u{2014} list queued, but add_to_queue did not append");
    } else {
        println!("FAIL \u{2014} expected at least {} queued, got {}", uris.len() - 1, queued.len());
    }
}

/// Headless check of the playback-presence marking: build the two row models,
/// run `apply_presence` over them, and assert the right rows light up. No network,
/// no window — this is what proves the in-place update (which keeps list scroll)
/// really reaches the models.
/// Drive the row menus with *real* pointer events on a headless software window.
/// This is the part a rendered PNG can't prove: the rows live inside a ListView,
/// whose Flickable filters pointer events (it delays left presses to decide whether
/// the user is flicking, and forwards non-left buttons), so "right-click opens the
/// menu" and "double-click plays the playlist" are claims about event plumbing.
/// Two halves of the infinite-scroll port that a PNG can't show:
///   1. LIVE — page Liked Songs against the real account and check the counts,
///      the reported total, and that `done` only trips at the end.
///   2. UI — scroll the real ListView with pointer-wheel events and check that
///      crossing the 320px threshold asks for the next page exactly once.
async fn selftest_paging_live(fails: &mut Vec<String>) {
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("  FAIL could not load the session: {e}");
            fails.push("session".into());
            return;
        }
    };
    let mut pager = TrackPager::start(PagerSource::Liked, "Liked Songs");
    let mut tracks: Vec<Track> = Vec::new();
    let mut view: Vec<usize> = Vec::new();
    let weak = slint::Weak::<MainWindow>::default();
    let presence = Presence::default();

    for page in 1..=3u32 {
        load_next_track_page(
            &weak, &mut session, &mut pager, &mut tracks, &mut view, "recent", "asc", &presence,
        )
        .await;
        let want = (Session::LIKED_PAGE * page) as usize;
        let ok = tracks.len() == want;
        println!(
            "  {} page {page}: {} tracks (want {want}), total={}, done={}",
            if ok { "ok  " } else { "FAIL" },
            tracks.len(),
            pager.total,
            pager.done
        );
        if !ok {
            fails.push(format!("page {page} count"));
        }
    }
    // The whole point of the change: the old build stopped at LIKED_CAP=200.
    let beyond = tracks.len() > 0 && pager.total > 200;
    println!(
        "  {} Spotify reports {} liked tracks \u{2014} more than the old 200 cap",
        if beyond { "ok  " } else { "FAIL" },
        pager.total
    );
    if !beyond {
        fails.push("total > old cap".into());
    }
    if pager.done {
        println!("  FAIL pager finished early at {} tracks", tracks.len());
        fails.push("done too early".into());
    } else {
        println!("  ok   pager still has more to load");
    }
    // A playlist this account does not own: `/items` 403s under the dev-mode token,
    // so it can only come from the public embed page. Before that fallback existed
    // these opened to an empty list.
    let me = session.display_name().to_string();
    let foreign = match session.playlists().await {
        Ok(pls) => pls.into_iter().find(|p| !p.owner.is_empty() && p.owner != me),
        Err(e) => {
            println!("  FAIL could not list playlists: {e}");
            fails.push("playlists".into());
            None
        }
    };
    match foreign {
        Some(pl) => {
            let mut p2 = TrackPager::start(PagerSource::Playlist(pl.id.clone()), &pl.name);
            let mut t2: Vec<Track> = Vec::new();
            let mut v2: Vec<usize> = Vec::new();
            load_next_track_page(
                &weak, &mut session, &mut p2, &mut t2, &mut v2, "recent", "asc", &presence,
            )
            .await;
            let got = !t2.is_empty();
            println!(
                "  {} non-owned \u{201c}{}\u{201d} ({} tracks listed): loaded {} rows via {}",
                if got { "ok  " } else { "FAIL" },
                pl.name,
                pl.tracks,
                t2.len(),
                if p2.partial { "the embed fallback" } else { "the Web API" }
            );
            if !got {
                fails.push("non-owned playlist".into());
            }
            if got && p2.partial && (t2.len() as u32) < pl.tracks {
                println!("  ok   ...and it is flagged partial, so the status says so");
            } else if got && !p2.partial {
                println!("  ok   ...served by the Web API after all (no fallback needed)");
            }
        }
        None => println!("  note no non-owned playlist in this library to exercise the fallback"),
    }

    // Search results page too, but only under the PLAYLISTS filter — the original
    // guards its pager the same way.
    match (
        session.search_playlists_page("chill", 0).await,
        session.search_playlists_page("chill", Session::SEARCH_PAGE).await,
    ) {
        (Ok((p1, total)), Ok((p2, _))) => {
            // Spotify puts `null`s in playlist search items, so a "page of 10" often
            // parses to far fewer — the count is not the thing to assert on.
            let sized = !p1.is_empty() && total > Session::SEARCH_PAGE;
            println!(
                "  {} playlist search pages: {} then {} parsed hits of {total} reported",
                if sized { "ok  " } else { "FAIL" },
                p1.len(),
                p2.len()
            );
            if !sized {
                fails.push("search page sizes".into());
            }
            let first: std::collections::HashSet<&str> =
                p1.iter().map(|p| p.id.as_str()).collect();
            let fresh = p2.iter().all(|p| !first.contains(p.id.as_str()));
            println!(
                "  {} ...and the second page is different playlists, not a repeat",
                if fresh { "ok  " } else { "FAIL" }
            );
            if !fresh {
                fails.push("search page overlap".into());
            }
        }
        (a, b) => {
            let e = a.err().or(b.err()).unwrap_or_default();
            println!("  FAIL playlist search paging: {e}");
            fails.push("search paging".into());
        }
    }

    // The sort view has to cover every loaded row, or row clicks would mis-map.
    let view_ok = view.len() == tracks.len();
    println!("  {} sort view covers all {} loaded rows", if view_ok { "ok  " } else { "FAIL" }, tracks.len());
    if !view_ok {
        fails.push("sort view length".into());
    }
}

fn selftest_paging() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use slint::platform::{Platform, WindowAdapter, WindowEvent};
    use slint::LogicalPosition;
    use std::cell::Cell;

    let mut fails: Vec<String> = Vec::new();

    println!("live paging (real account):");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(selftest_paging_live(&mut fails));

    println!("scroll trigger:");
    const W: u32 = 980;
    const H: u32 = 660;
    struct ShotPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for ShotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    if slint::platform::set_platform(Box::new(ShotPlatform { window: window.clone() })).is_err() {
        println!("  FAIL could not install the headless platform");
        fails.push("platform".into());
    }
    let app = match MainWindow::new() {
        Ok(a) => a,
        Err(e) => {
            println!("  WINDOW ERROR: {e}");
            return;
        }
    };
    // A list long enough to scroll, drilled in, with another page available.
    let mk = |i: usize| Trk {
        uri: format!("spotify:track:t{i}").into(),
        name: format!("Track {i}").into(),
        artist: "Someone".into(),
        playable: true,
        active: false,
        playing: false,
        selected: false,
    };
    let rows: Vec<Trk> = (0..80).map(mk).collect();
    app.set_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    app.set_drilled(true);
    app.set_drill_title("Liked Songs".into());
    app.set_tracks_more(true);

    let asks = Rc::new(Cell::new(0u32));
    {
        let asks = asks.clone();
        app.on_load_more_tracks(move || asks.set(asks.get() + 1));
    }

    window.set_size(slint::PhysicalSize::new(W, H));
    let _ = app.show();
    let mut buffer = vec![Rgb565Pixel(0); (W * H) as usize];
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });

    let win = window.window();
    let at = LogicalPosition::new(120.0, 300.0);
    let check = |cond: bool, what: &str, fails: &mut Vec<String>| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };
    check(asks.get() == 0, "sitting at the top asks for nothing", &mut fails);

    // A small scroll stays well clear of the 320px threshold on an 80-row list.
    win.dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: -120.0 });
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    check(asks.get() == 0, "a short scroll still asks for nothing", &mut fails);

    // Now scroll to the end.
    for _ in 0..40 {
        win.dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: -400.0 });
    }
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    check(asks.get() == 1, "scrolling to the end asks for exactly one more page", &mut fails);
    check(!app.get_tracks_more(), "...and closes the gate until that page lands", &mut fails);

    // With the gate shut, more scrolling must not re-ask.
    for _ in 0..10 {
        win.dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: -400.0 });
    }
    check(asks.get() == 1, "...and further scrolling does not re-ask", &mut fails);

    // A page that lands without pushing the bottom away (too few rows to fill the
    // viewport) must pull the next one immediately — the original re-fires from its
    // requestAnimationFrame. Reopening the gate is what a landed page looks like.
    app.set_tracks_more(true);
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    check(
        asks.get() == 2,
        "a page that doesn't fill the viewport pulls the next one right away",
        &mut fails,
    );
    check(!app.get_tracks_more(), "...and the gate closes again", &mut fails);

    // The results list has its own, tighter threshold (220px) and its own gate.
    println!("search-results scroll trigger:");
    let asks_sr = Rc::new(Cell::new(0u32));
    {
        let asks_sr = asks_sr.clone();
        app.on_load_more_search_results(move || asks_sr.set(asks_sr.get() + 1));
    }
    let hits: Vec<SearchRow> = (0..40)
        .map(|i| SearchRow {
            name: format!("Playlist {i}").into(),
            sub: "Playlist \u{2022} 20 tracks".into(),
            kind: "playlist".into(),
            selected: false,
            art: Default::default(),
            meta: Default::default(),
        })
        .collect();
    app.set_search_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(hits))));
    app.set_drilled(false);
    app.set_search_drilled(false);
    app.set_tab(1);
    app.set_search_more(true);
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    check(asks_sr.get() == 0, "sitting at the top asks for nothing", &mut fails);
    for _ in 0..40 {
        win.dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: -400.0 });
    }
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    check(asks_sr.get() == 1, "scrolling the results to the end asks for one more page", &mut fails);
    check(!app.get_search_more(), "...and closes the results gate", &mut fails);

    if fails.is_empty() {
        println!("PASS: paging loads real pages and the scroll threshold fires once");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// Click-to-dismiss on the now-playing pane, driven with real pointer events.
///
/// Ports the `#now-playing` click handler (`app.js:1659`): clicking the centre pane
/// closes the RIGHT sidebar first, and only a second click collapses the LEFT panel.
/// A rendered PNG cannot show any of this — it is entirely about which TouchArea
/// receives a click — and the pane's background TouchArea sits *under* every control,
/// so the risk worth testing is that it starts swallowing clicks meant for transport
/// buttons. Hence the "play button still plays" case at the end.
fn selftest_dismiss() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
    use slint::LogicalPosition;
    use std::cell::RefCell;

    const W: u32 = 980;
    const H: u32 = 660;
    // Empty pane background: left of the centred artwork, above the controls.
    const BG: (f32, f32) = (350.0, 120.0);
    // Centre of the artwork (it has its own TouchArea, which must forward).
    const ART: (f32, f32) = (622.0, 227.0);
    // Centre of the play/pause button (measured from a render at this size).
    const PLAY: (f32, f32) = (622.0, 462.0);

    struct ShotPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for ShotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    if slint::platform::set_platform(Box::new(ShotPlatform { window: window.clone() })).is_err() {
        println!("FAIL: could not install the headless platform");
        return;
    }
    let app = match MainWindow::new() {
        Ok(a) => a,
        Err(e) => {
            println!("WINDOW ERROR: {e}");
            return;
        }
    };
    window.set_size(slint::PhysicalSize::new(W, H));

    let closes = Rc::new(RefCell::new(0usize));
    let plays = Rc::new(RefCell::new(0usize));
    {
        let closes = Rc::clone(&closes);
        app.on_close_sidebar(move || *closes.borrow_mut() += 1);
    }
    {
        let plays = Rc::clone(&plays);
        app.on_toggle_play(move || *plays.borrow_mut() += 1);
    }
    // The real wiring lives in main() (the UI can't set the worker-owned
    // `sidebar-mode`), so mirror it here exactly.
    {
        let weak = app.as_weak();
        let closes = Rc::clone(&closes);
        app.on_dismiss_overlays(move || {
            let Some(a) = weak.upgrade() else { return };
            if a.get_sidebar_mode() != 0 {
                *closes.borrow_mut() += 1;
            } else if !a.get_left_collapsed() {
                a.set_left_collapsed(true);
            }
        });
    }

    let mut buffer = vec![Rgb565Pixel(0); (W * H) as usize];
    let mut fails: Vec<String> = Vec::new();
    let check = |cond: bool, what: &str, fails: &mut Vec<String>| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };

    let win = window.window();
    let mut click = |at: (f32, f32)| {
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buffer, W as usize);
        });
        let pos = LogicalPosition::new(at.0, at.1);
        win.dispatch_event(WindowEvent::PointerMoved { position: pos });
        win.dispatch_event(WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left });
        win.dispatch_event(WindowEvent::PointerReleased { position: pos, button: PointerEventButton::Left });
    };

    // Both overlays open.
    app.set_sidebar_mode(1);
    app.set_left_collapsed(false);

    // 1. First click closes the RIGHT sidebar, and leaves the left panel alone.
    click(BG);
    check(*closes.borrow() == 1, "click on the pane asks to close the sidebar", &mut fails);
    check(!app.get_left_collapsed(), "...and does NOT collapse the left panel yet", &mut fails);

    // 2. The worker answers by clearing sidebar-mode; the next click takes the left.
    app.set_sidebar_mode(0);
    click(BG);
    check(app.get_left_collapsed(), "the second click collapses the left panel", &mut fails);
    check(*closes.borrow() == 1, "...without asking to close the sidebar again", &mut fails);

    // 3. Nothing left to dismiss — a third click is inert.
    click(BG);
    check(
        *closes.borrow() == 1 && app.get_left_collapsed(),
        "a third click does nothing",
        &mut fails,
    );

    // 4. The artwork has its own TouchArea; the original dismisses on it too.
    app.set_sidebar_mode(1);
    app.set_left_collapsed(false);
    click(ART);
    check(*closes.borrow() == 2, "clicking the artwork dismisses as well", &mut fails);

    // 5. The guard that matters: controls must still win over the background.
    app.set_sidebar_mode(1);
    click(PLAY);
    check(*plays.borrow() == 1, "the play button still toggles playback", &mut fails);
    check(
        *closes.borrow() == 2,
        "...and clicking it does NOT dismiss the sidebar",
        &mut fails,
    );

    println!("{:-<72}", "");
    if fails.is_empty() {
        println!("PASS \u{2014} right-before-left dismiss, and controls keep their clicks");
    } else {
        println!("FAIL \u{2014} {} check(s): {}", fails.len(), fails.join("; "));
    }
}

fn selftest_menus() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
    use slint::LogicalPosition;
    use std::cell::RefCell;

    const W: u32 = 980;
    const H: u32 = 660;

    struct ShotPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for ShotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    if slint::platform::set_platform(Box::new(ShotPlatform { window: window.clone() })).is_err() {
        println!("FAIL: could not install the headless platform");
        return;
    }
    let app = match MainWindow::new() {
        Ok(a) => a,
        Err(e) => {
            println!("WINDOW ERROR: {e}");
            return;
        }
    };

    let playlists = vec![
        lightify_core::Playlist { id: "pl1".into(), name: "Deep Focus".into(), tracks: 3, owner: "Jon".into(), uri: "spotify:playlist:pl1".into(), image: String::new() },
        lightify_core::Playlist { id: "pl2".into(), name: "Techno Bunker".into(), tracks: 2, owner: "Jon".into(), uri: "spotify:playlist:pl2".into(), image: String::new() },
    ];
    let track = Track {
        id: "t1".into(),
        name: "One".into(),
        artists: "Someone".into(),
        artist_ids: vec!["a1".into()],
        album: String::new(),
        album_id: String::new(),
        duration_ms: 0,
        uri: "spotify:track:t1".into(),
        album_art: String::new(),
        added_at: None,
        is_playable: true,
    };
    let order: Vec<usize> = (0..playlists.len()).collect();
    app.set_playlists(slint::ModelRc::from(Rc::new(slint::VecModel::from(build_rows(
        &playlists, &order, "", false,
    )))));
    // 30 rows so the list is genuinely scrollable — otherwise the Flickable has
    // nothing to steal and the long-press test would prove nothing.
    let trks: Vec<Track> = (0..30)
        .map(|i| Track { id: format!("t{i}"), name: format!("Track {i}"), uri: format!("spotify:track:t{i}"), ..track.clone() })
        .collect();
    app.set_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        trks.iter()
            .map(|t| Trk {
                uri: t.uri.clone().into(),
                name: t.name.clone().into(),
                artist: t.artists.clone().into(),
                playable: true,
                active: false,
                playing: false,
                selected: false,
            })
            .collect::<Vec<_>>(),
    ))));
    let view: Vec<usize> = (0..trks.len()).collect();
    // Mirror the worker's selection set so the gesture can be checked end to end.
    let selected: Rc<RefCell<std::collections::BTreeSet<usize>>> = Rc::new(RefCell::new(Default::default()));


    // Mirror the worker: record what the row asked for, and build the same menu.
    let opened: Rc<RefCell<Vec<(i32, i32)>>> = Rc::new(RefCell::new(Vec::new()));
    let played: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let opened = opened.clone();
        let weak = app.as_weak();
        let track = track.clone();
        let sel_for_menu = selected.clone();
        app.on_open_context(move |kind, index| {
            opened.borrow_mut().push((kind, index));
            let items = if kind == 0 {
                if index == 0 { liked_row_menu() } else { playlist_row_menu(index as usize) }
            } else {
                let sel = sel_for_menu.borrow();
                if sel.len() > 1 && sel.contains(&(index as usize)) {
                    selection_menu(sel.len())
                } else {
                    track_row_menu(index as usize, &track)
                }
            };
            if let Some(a) = weak.upgrade() {
                a.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(ctx_rows(&items)))));
                a.set_ctx_open(true);
            }
        });
    }
    {
        let played = played.clone();
        app.on_play_row(move |i| played.borrow_mut().push(i));
    }
    let activated: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let activated = activated.clone();
        app.on_play_track(move |i| activated.borrow_mut().push(i));
    }
    {
        // Same wiring the interactive path installs.
        let selected = selected.clone();
        let anchor: Rc<std::cell::Cell<Option<usize>>> = Rc::new(std::cell::Cell::new(None));
        let weak = app.as_weak();
        let view = view.clone();
        app.on_select_click(move |_scope, row, ctrl, shift| {
            let row = row.max(0) as usize;
            let Some(&src) = view.get(row) else { return };
            let mut sel = selected.borrow_mut();
            if shift {
                let from = anchor.get().unwrap_or(row);
                let (lo, hi) = if row < from { (row, from) } else { (from, row) };
                sel.clear();
                for r in lo..=hi {
                    if let Some(&k) = view.get(r) {
                        sel.insert(k);
                    }
                }
            } else if ctrl {
                if !sel.remove(&src) {
                    sel.insert(src);
                }
                anchor.set(Some(row));
            }
            if let Some(a) = weak.upgrade() {
                apply_selection(&a, &selection_flags(&sel, &view));
            }
        });
    }
    {
        let selected = selected.clone();
        let weak = app.as_weak();
        let view = view.clone();
        app.on_clear_selection(move || {
            let mut sel = selected.borrow_mut();
            sel.clear();
            if let Some(a) = weak.upgrade() {
                apply_selection(&a, &selection_flags(&sel, &view));
            }
        });
    }

    window.set_size(slint::PhysicalSize::new(W, H));
    // Timer elements (the long-press) only tick on a shown window — with
    // MinimalSoftwareWindow this costs nothing, there is no OS window.
    if let Err(e) = app.show() {
        println!("WINDOW ERROR: {e}");
        return;
    }
    let mut buffer = vec![Rgb565Pixel(0); (W * H) as usize];

    // Row centres at 980x660, measured off the --shot-ctx renders: the library list
    // starts under the tab bar + sort toolbar, ListRow is 46px, TrackRow is 40px.
    const LIKED_Y: f32 = 118.0;
    const PLAYLIST1_Y: f32 = 164.0;
    const TRACK0_Y: f32 = 148.0;
    const ROW_X: f32 = 120.0;

    let mut fails: Vec<String> = Vec::new();
    let check = |cond: bool, what: &str, fails: &mut Vec<String>| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };
    let labels_of = |app: &MainWindow| -> Vec<String> {
        app.get_ctx_items().iter().map(|m| m.label.to_string()).collect()
    };

    // 1. right-click the Liked Songs row
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    let at = |y: f32| LogicalPosition::new(ROW_X, y);
    let win = window.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: at(LIKED_Y) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(LIKED_Y), button: PointerEventButton::Right });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(LIKED_Y), button: PointerEventButton::Right });
    check(
        opened.borrow().last() == Some(&(0, 0)),
        "right-click on Liked Songs opens the library menu for row 0",
        &mut fails,
    );
    check(app.get_ctx_open(), "...and the menu is open", &mut fails);
    check(
        labels_of(&app) == ["Open", "Download liked songs"],
        "...with the Liked Songs items (Open / Download liked songs)",
        &mut fails,
    );

    // 2. right-click a playlist row
    app.set_ctx_open(false);
    win.dispatch_event(WindowEvent::PointerMoved { position: at(PLAYLIST1_Y) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(PLAYLIST1_Y), button: PointerEventButton::Right });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(PLAYLIST1_Y), button: PointerEventButton::Right });
    check(
        opened.borrow().last() == Some(&(0, 1)),
        "right-click on the first playlist opens the menu for row 1",
        &mut fails,
    );
    check(
        labels_of(&app) == ["Play", "Save to library", "Download", "Delete"],
        "...with the playlist items (Play / Save to library / Download / Delete)",
        &mut fails,
    );
    let danger: Vec<bool> = app.get_ctx_items().iter().map(|m| m.danger).collect();
    check(danger == [false, false, false, true], "...and only Delete is danger-styled", &mut fails);
    // Download stopped being a dimmed placeholder when the OnTheSpot bridge landed
    // (2026-09-11), so nothing in a playlist menu is dimmed any more.
    let muted: Vec<bool> = app.get_ctx_items().iter().map(|m| m.muted).collect();
    check(muted == [false, false, false, false], "...and nothing is dimmed", &mut fails);

    // 3. double-click a playlist row -> play the whole thing
    app.set_ctx_open(false);
    for _ in 0..2 {
        win.dispatch_event(WindowEvent::PointerPressed { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
        win.dispatch_event(WindowEvent::PointerReleased { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    }
    check(
        played.borrow().as_slice() == [1],
        "double-click on the first playlist plays row 1 (exactly once)",
        &mut fails,
    );

    // 4. right-click a drilled track row
    app.set_ctx_open(false);
    app.set_drilled(true);
    app.set_drill_title("Liked Songs".into());
    window.request_redraw();
    window.draw_if_needed(|r| { r.render(&mut buffer, W as usize); });
    win.dispatch_event(WindowEvent::PointerMoved { position: at(TRACK0_Y) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(TRACK0_Y), button: PointerEventButton::Right });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(TRACK0_Y), button: PointerEventButton::Right });
    check(
        opened.borrow().last() == Some(&(1, 0)),
        "right-click on a drilled track opens the track menu for row 0",
        &mut fails,
    );
    check(
        labels_of(&app)
            == ["Play", "Add to queue", "Share", "Start station", "Follow artist", "Download"],
        "...with buildSpotifyTrackMenu's rows, in order",
        &mut fails,
    );



    let row_y = |i: f32| TRACK0_Y + 40.0 * i;

    // 6. Modifier DELIVERY, proven on its own with the shell's already-shipped
    //    Ctrl+K palette shortcut — it reads `modifiers.control` from exactly the
    //    same place the selection click does.
    let ctrl: slint::SharedString = slint::platform::Key::Control.into();
    win.dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
    win.dispatch_event(WindowEvent::KeyPressed { text: "k".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "k".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: ctrl });
    check(
        app.get_palette_open(),
        "Ctrl+<key> modifiers reach the UI (Ctrl+K opens the palette)",
        &mut fails,
    );
    app.set_palette_open(false);

    // 7. Selection MODEL, driven through the same callback the row fires. Injecting
    //    key events around a synthetic pointer click suppresses the click in this
    //    harness (it swallows the activation too), so the modifier-plus-click
    //    combination is the one step left for the real window — see PARITY.md.
    app.invoke_clear_selection();
    app.invoke_select_click(SEL_TRACKS, 1, true, false);
    check(
        selected.borrow().iter().copied().collect::<Vec<_>>() == vec![1],
        "Ctrl+click selects one track row",
        &mut fails,
    );
    app.invoke_select_click(SEL_TRACKS, 3, true, false);
    check(
        selected.borrow().iter().copied().collect::<Vec<_>>() == vec![1, 3],
        "...a second Ctrl+click adds another",
        &mut fails,
    );
    app.invoke_select_click(SEL_TRACKS, 3, true, false);
    check(
        selected.borrow().iter().copied().collect::<Vec<_>>() == vec![1],
        "...and Ctrl+clicking it again removes it",
        &mut fails,
    );

    // 8. Shift+click extends from the last clicked row.
    app.invoke_clear_selection();
    app.invoke_select_click(SEL_TRACKS, 1, true, false);
    app.invoke_select_click(SEL_TRACKS, 4, false, true);
    let sel: Vec<usize> = selected.borrow().iter().copied().collect();
    check(sel == vec![1, 2, 3, 4], "Shift+click extends over the rows in between", &mut fails);
    check(app.get_sel_count() as usize == sel.len(), "...and sel-count matches", &mut fails);
    let marked: Vec<usize> = app
        .get_tracks()
        .iter()
        .enumerate()
        .filter(|(_, t)| t.selected)
        .map(|(i, _)| i)
        .collect();
    check(marked == sel, "...and exactly those rows are marked in the model", &mut fails);
    let n = sel.len();

    // 9. right-click INSIDE the selection → the group menu (a real pointer event)
    app.set_ctx_open(false);
    let inside = row_y(2.0);
    win.dispatch_event(WindowEvent::PointerMoved { position: at(inside) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(inside), button: PointerEventButton::Right });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(inside), button: PointerEventButton::Right });
    check(
        labels_of(&app)
            == [
                format!("Play {n} selected"),
                format!("Add {n} to queue"),
                "Create playlist from selection".to_string(),
            ],
        "right-click inside the selection shows buildSpotifySelectionMenu",
        &mut fails,
    );

    // 10. ...and OUTSIDE it the single-row menu is unchanged
    app.set_ctx_open(false);
    let outside = row_y(8.0);
    win.dispatch_event(WindowEvent::PointerMoved { position: at(outside) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(outside), button: PointerEventButton::Right });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(outside), button: PointerEventButton::Right });
    check(
        labels_of(&app).first().map(|s| s.as_str()) == Some("Play"),
        "right-click on an unselected row keeps the single-track menu",
        &mut fails,
    );

    // 11. a plain click still plays the row it hit (a real pointer event)
    app.set_ctx_open(false);
    win.dispatch_event(WindowEvent::PointerMoved { position: at(row_y(6.0)) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(row_y(6.0)), button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(row_y(6.0)), button: PointerEventButton::Left });
    check(
        activated.borrow().as_slice() == [6],
        "an unmodified click still plays the track",
        &mut fails,
    );
    app.invoke_clear_selection();
    check(selected.borrow().is_empty(), "clear-selection empties the selection", &mut fails);

    if fails.is_empty() {
        println!("PASS: row menus, double-click and multi-select all reach the rows through the ListView");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// The pure helpers the type-to-filter key handler calls (D7).
fn install_filter_helpers(app: &MainWindow) {
    app.on_is_printable(|t| {
        let mut chars = t.chars();
        // One character, not a control code, and not one of the private-use codes
        // Slint uses for special keys (arrows, F-keys, …).
        matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&c))
    });
    app.on_chop_last(|t| {
        let mut s = t.to_string();
        s.pop();
        s.into()
    });
}

/// `--selftest-filter [out.png]`: type-to-filter key handling (D7) with real key
/// events — letters build the filter, Backspace trims, Esc clears, special keys and
/// Ctrl-chords don't type, other tabs and overlays are left alone — then renders
/// the pill for a visual check.
fn selftest_filter(png: Option<&str>) {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter, WindowEvent};
    struct P(Rc<MinimalSoftwareWindow>);
    impl Platform for P {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    if slint::platform::set_platform(Box::new(P(window.clone()))).is_err() {
        println!("FAIL: could not install the headless platform");
        return;
    }
    let app = MainWindow::new().expect("build UI");
    install_filter_helpers(&app);
    app.set_ui_font(ui_font().into());
    window.set_size(slint::PhysicalSize::new(980, 660));
    let _ = app.show();
    let edits: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    {
        let edits = edits.clone();
        app.on_list_filter_edited(move |t| edits.borrow_mut().push(t.to_string()));
    }
    let key = |t: &str| {
        let text: slint::SharedString = t.into();
        app.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        app.window().dispatch_event(WindowEvent::KeyReleased { text });
    };
    let mut fails = Vec::new();
    let mut check = |ok: bool, what: &str| {
        println!("  {}  {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            fails.push(what.to_string());
        }
    };
    let filt = |app: &MainWindow| app.get_list_filter().to_string();

    for c in ["t", "e", "c", "h"] {
        key(c);
    }
    check(filt(&app) == "tech", "letters build the filter");
    check(edits.borrow().last().map(String::as_str) == Some("tech"), "each edit reaches the host");
    key(&slint::SharedString::from(slint::platform::Key::Backspace));
    check(filt(&app) == "tec", "Backspace trims one character");
    key(&slint::SharedString::from(slint::platform::Key::UpArrow));
    check(filt(&app) == "tec", "special keys don't type");
    key(" ");
    key("n");
    check(filt(&app) == "tec n", "space types once a filter is going");
    key(&slint::SharedString::from(slint::platform::Key::Escape));
    check(filt(&app).is_empty(), "Esc clears");
    key(" ");
    check(filt(&app).is_empty(), "a leading space doesn't start a filter");
    app.set_tab(1);
    key("x");
    check(filt(&app).is_empty(), "other tabs are left alone");
    app.set_tab(0);
    app.set_settings_open(true);
    key("x");
    check(filt(&app).is_empty(), "overlays are left alone");
    app.set_settings_open(false);

    for c in ["d", "e", "e", "p"] {
        key(c);
    }
    app.set_list_filter_count(2);
    if let Some(path) = png {
        slint::platform::update_timers_and_animations();
        let (w, h) = (980u32, 660u32);
        // One buffer for every render below: the window repaints only what changed.
        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buffer, w as usize);
        });
        let rgb: Vec<u8> = buffer.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
        let file = std::fs::File::create(path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
        println!("  wrote {path}");
        // The Search tab's empty states, which live searches rarely produce.
        for (name, text) in [("no-results", "zzqxjvwk"), ("empty-box", ""), ("recents", "")] {
            app.set_tab(1);
            app.set_search_text(text.into());
            app.set_search_loading(false);
            let rows = if name == "recents" {
                build_recents(&["toby keith".into(), "midnight city".into(), "lofi".into()]).0
            } else {
                Vec::new()
            };
            app.set_search_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|r| {
                r.render(&mut buffer, w as usize);
            });
            let rgb: Vec<u8> = buffer.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
            let out = path.replace(".png", &format!("-{name}.png"));
            let file = std::fs::File::create(&out).expect("create png");
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&rgb).unwrap();
            println!("  wrote {out}");
        }
    }
    if fails.is_empty() {
        println!("PASS: type-to-filter keys");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// `--selftest-fade`: the cover cross-fade (UI-PLAN A6) — first cover appears
/// without a fade; a replacement starts transparent over the old one, rises to
/// opaque, and the old cover is released; with reduced motion it's instant.
fn selftest_fade() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};
    struct P(Rc<MinimalSoftwareWindow>);
    impl Platform for P {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    if slint::platform::set_platform(Box::new(P(window.clone()))).is_err() {
        println!("FAIL: could not install the headless platform");
        return;
    }
    let app = MainWindow::new().expect("build UI");
    window.set_size(slint::PhysicalSize::new(980, 660));
    let img = |v: u8| {
        let px = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&[v, v, v, 255].repeat(4), 2, 2);
        slint::Image::from_rgba8(px)
    };
    // Mirrors `set_art` on the UI thread.
    let set_art = |i: slint::Image| {
        app.set_prev_art(app.get_album_art());
        app.set_album_art(i);
    };
    let step = |ms: u64| {
        std::thread::sleep(Duration::from_millis(ms));
        slint::platform::update_timers_and_animations();
        let _ = app.get_art_fade(); // evaluate bindings / change handlers
        slint::platform::update_timers_and_animations();
    };
    let mut fails = Vec::new();
    let mut check = |ok: bool, what: &str| {
        println!("  {}  {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            fails.push(what.to_string());
        }
    };

    set_art(img(10));
    step(5);
    check(app.get_art_fade() >= 0.99, "first cover: no fade");
    check(app.get_prev_art().size().width == 0, "first cover: nothing kept underneath");

    set_art(img(200));
    step(1);
    check(app.get_art_fade() <= 0.01, "replacement starts transparent");
    check(app.get_prev_art().size().width > 0, "old cover kept underneath");
    step(30); // the 16 ms kick-off timer fires; the eased rise starts here
    step(90);
    let mid = app.get_art_fade();
    check(mid > 0.01 && mid < 0.99, &format!("mid-fade opacity is between (got {mid:.2})"));
    step(400);
    check(app.get_art_fade() >= 0.99, "fade ends opaque");
    check(app.get_prev_art().size().width == 0, "old cover released after the fade");

    app.global::<Motion>().set_reduced(true);
    set_art(img(90));
    step(5);
    check(app.get_art_fade() >= 0.99, "reduced motion: instant");
    check(app.get_prev_art().size().width == 0, "reduced motion: nothing kept underneath");

    if let Some(path) = std::env::args().skip_while(|a| a != "--selftest-fade").nth(1) {
        use slint::platform::software_renderer::PremultipliedRgbaColor;
        app.set_repeat_mode("track".into());
        app.set_shuffle_on(true);
        slint::platform::update_timers_and_animations();
        let (w, h) = (980u32, 660u32);
        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buffer, w as usize);
        });
        let rgb: Vec<u8> = buffer.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
        let file = std::fs::File::create(&path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
        println!("  wrote {path} (repeat-one + shuffle on)");
    }
    if fails.is_empty() {
        println!("PASS: cover cross-fade");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

fn selftest_presence() {
    let app = match MainWindow::new() {
        Ok(a) => a,
        Err(e) => {
            println!("WINDOW ERROR: {e}");
            return;
        }
    };
    let playlists = vec![
        lightify_core::Playlist { id: "pl1".into(), name: "Deep Focus".into(), tracks: 3, owner: "Jon".into(), uri: "spotify:playlist:pl1".into(), image: String::new() },
        lightify_core::Playlist { id: "pl2".into(), name: "Techno Bunker".into(), tracks: 2, owner: "Jon".into(), uri: "spotify:playlist:pl2".into(), image: String::new() },
    ];
    let mk = |uri: &str, name: &str| Track {
        id: name.into(),
        name: name.into(),
        artists: "Someone".into(),
        artist_ids: vec![],
        album: String::new(),
        album_id: String::new(),
        duration_ms: 0,
        uri: uri.into(),
        album_art: String::new(),
        added_at: None,
        is_playable: true,
    };
    let tracks = vec![mk("spotify:track:t1", "One"), mk("spotify:track:t2", "Two")];

    let order: Vec<usize> = (0..playlists.len()).collect();
    app.set_playlists(slint::ModelRc::from(Rc::new(slint::VecModel::from(build_rows(
        &playlists, &order, "", false,
    )))));
    app.set_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        tracks
            .iter()
            .map(|t| Trk {
                uri: t.uri.clone().into(),
                name: t.name.clone().into(),
                artist: t.artists.clone().into(),
                playable: true,
                active: false,
                playing: false,
                selected: false,
            })
            .collect::<Vec<_>>(),
    ))));

    // (label, presence, expected row index that is active, expected `playing`)
    let cases: Vec<(&str, Presence, Option<usize>, Option<usize>, bool)> = vec![
        ("playing pl2 / track t2", Presence { source: "pl2".into(), track_uri: "spotify:track:t2".into(), playing: true }, Some(2), Some(1), true),
        ("paused pl2 / track t2", Presence { source: "pl2".into(), track_uri: "spotify:track:t2".into(), playing: false }, Some(2), Some(1), false),
        ("playing Liked / track t1", Presence { source: LIKED_SOURCE.into(), track_uri: "spotify:track:t1".into(), playing: true }, Some(0), Some(0), true),
        ("nothing playing", Presence::default(), None, None, false),
    ];

    println!("playback-presence self-test");
    println!("{:-<72}", "");
    let mut ok = true;
    for (label, p, want_row, want_trk, want_playing) in cases {
        apply_presence(&app, &p);
        let rows = app.get_playlists();
        let trks = app.get_tracks();
        let got_row = (0..rows.row_count()).find(|&i| rows.row_data(i).map(|r| r.active).unwrap_or(false));
        let got_trk = (0..trks.row_count()).find(|&i| trks.row_data(i).map(|t| t.active).unwrap_or(false));
        let row_playing = got_row
            .and_then(|i| rows.row_data(i))
            .map(|r| r.playing)
            .unwrap_or(false);
        let active_count = (0..rows.row_count()).filter(|&i| rows.row_data(i).map(|r| r.active).unwrap_or(false)).count();
        let pass = got_row == want_row
            && got_trk == want_trk
            && active_count == want_row.map(|_| 1).unwrap_or(0)
            && row_playing == (want_row.is_some() && want_playing);
        ok &= pass;
        println!(
            "  {:<26} row={:?} track={:?} playing={} {}",
            label,
            got_row,
            got_trk,
            row_playing,
            if pass { "OK" } else { "FAIL" }
        );
    }
    println!("{:-<72}", "");
    println!("{}", if ok { "PASS" } else { "FAIL" });
}

/// Headless end-to-end check of the real data layer (no window). Prints what
/// the UI would show, so the network/auth path can be verified without a screen.
async fn probe() {
    let mut s = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("LOAD ERROR: {e}");
            return;
        }
    };
    match s.ensure_fresh().await {
        Ok(()) => println!("token OK — user: {:?}", s.display_name()),
        Err(e) => {
            println!("REFRESH ERROR: {e}");
            return;
        }
    }
    match s.playlists().await {
        Ok(p) => {
            println!("playlists: {}", p.len());
            for pl in p.iter().take(8) {
                println!("  - {} ({} tracks)", pl.name, pl.tracks);
            }
        }
        Err(e) => println!("PLAYLISTS ERROR: {e}"),
    }
    match s.playback().await {
        Ok(Some(pb)) => {
            println!(
                "now playing: {} — {} [{}] {}/{}ms playing={}",
                pb.track.as_ref().map(|t| t.name.as_str()).unwrap_or("-"),
                pb.track.as_ref().map(|t| t.artists.as_str()).unwrap_or("-"),
                pb.device_name,
                pb.progress_ms,
                pb.duration_ms,
                pb.is_playing
            );
            // What the library highlight + the like button resolve to for this state.
            println!(
                "  context: {:?} — library row: {:?}",
                pb.context_uri,
                context_playlist_id(&pb.context_uri)
            );
            let tid = pb.track.as_ref().map(|t| t.id.clone()).unwrap_or_default();
            if !tid.is_empty() {
                println!("  liked ({tid}): {:?}", s.is_track_saved(&tid).await);
            }
        }
        Ok(None) => println!("now playing: (nothing active on any device)"),
        Err(e) => println!("PLAYBACK ERROR: {e}"),
    }
}

/// Headless check of the Beatport path: scrape the Overall Top 100, print the
/// first rows, and resolve a few to Spotify via the scored matcher.
/// `--probe-search <q>`: print the popularity/follower fields search returns for
/// artists, tracks and albums (dev-mode tokens have had fields stripped before).
async fn probe_search(q: &str) {
    let mut s = match Session::load() {
        Ok(s) => s,
        Err(e) => return println!("no session: {e}"),
    };
    if let Err(e) = s.ensure_fresh().await {
        return println!("auth: {e}");
    }
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.spotify.com/v1/search")
        .bearer_auth(s.access_token())
        .query(&[("q", q), ("type", "artist,track,album"), ("limit", "5"), ("market", "from_token")])
        .send()
        .await;
    let v: serde_json::Value = match resp {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => return println!("request: {e}"),
    };
    for a in v["artists"]["items"].as_array().cloned().unwrap_or_default() {
        println!("artist  {:<28} followers={:?} popularity={:?} genres={}", a["name"].as_str().unwrap_or(""), a["followers"]["total"], a["popularity"], a["genres"]);
    }
    for t in v["tracks"]["items"].as_array().cloned().unwrap_or_default() {
        println!("track   {:<28} popularity={:?} explicit={:?}", t["name"].as_str().unwrap_or(""), t["popularity"], t["explicit"]);
    }
    for a in v["albums"]["items"].as_array().cloned().unwrap_or_default() {
        println!("album   {:<28} type={:?} popularity={:?}", a["name"].as_str().unwrap_or(""), a["album_type"], a["popularity"]);
    }
}

async fn probe_beatport() {
    let mut s = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("LOAD ERROR: {e}");
            return;
        }
    };
    if let Err(e) = s.ensure_fresh().await {
        println!("AUTH ERROR: {e}");
        return;
    }
    println!("scraping Beatport Overall Top 100…");
    let chart = match s.beatport_chart("", "tracks").await {
        Ok(t) => t,
        Err(e) => {
            println!("CHART ERROR: {e}");
            return;
        }
    };
    println!("got {} tracks", chart.len());
    for t in chart.iter().take(6) {
        println!("  {:>3}. {} — {}{}", t.rank, t.name, t.artists,
            if t.label.is_empty() { String::new() } else { format!("  [{}]", t.label) });
    }
    println!("resolving first 3 to Spotify (scored match ≥160)…");
    for t in chart.iter().take(3) {
        match s.beatport_match(&t.name, &t.artists).await {
            Ok(Some(m)) => println!("  \u{2713} {} — {}  →  {} ({})", t.name, t.artists, m.name, m.uri),
            Ok(None) => println!("  \u{2717} {} — {}  →  no match", t.name, t.artists),
            Err(e) => println!("  ! {} — {}  →  error: {e}", t.name, t.artists),
        }
    }
}

/// Headless end-to-end test of the sidebar toggles: drives the SAME
/// `next_sidebar_mode` state machine the worker uses through a realistic
/// press sequence, loading real queue/recent data for each opened mode, and
/// prints a transcript. Verifies open / re-press-closes / mode-switch semantics
/// and that each mode loads the right data — the part the GUI can't be clicked
/// for in this environment.
async fn selftest_sidebar() {
    let mut s = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            println!("LOAD ERROR: {e}");
            return;
        }
    };
    if let Err(e) = s.ensure_fresh().await {
        println!("AUTH ERROR: {e}");
        return;
    }
    let pb = s.playback().await.ok().flatten();
    let pid = pb.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.id.clone()).unwrap_or_default();
    println!("sidebar toggle self-test (now-playing id: {})", if pid.is_empty() { "—" } else { &pid });
    println!("{:-<72}", "");

    // The exact button presses a user would make, top to bottom.
    let presses = [
        ("press QUEUE", 1),
        ("press QUEUE again", 1),
        ("press RECENT", 2),
        ("press QUEUE", 1),
        ("press DOWNLOADS", 3),
        ("press DOWNLOADS again", 3),
    ];
    let mut mode = 0;
    let mut ok = true;
    for (label, pressed) in presses {
        let before = mode;
        mode = next_sidebar_mode(mode, pressed);
        let expected = if before == pressed { 0 } else { pressed };
        if mode != expected {
            ok = false;
        }
        let detail = match mode {
            1 => {
                let t = s.queue().await.unwrap_or_default();
                let rows = build_side_rows(&t, &pid, false);
                let playing = rows.iter().filter(|r| r.playing).count();
                format!("OPEN queue   — {} rows, {} marked now-playing", rows.len(), playing)
            }
            2 => {
                let t = s.recently_played(50).await.unwrap_or_default();
                let rows = build_side_rows(&t, &pid, true);
                let first = rows.first().map(|r| r.time.as_str().to_string()).unwrap_or_default();
                format!("OPEN recent  — {} rows, newest '{}' ago", rows.len(), first)
            }
            3 => "OPEN downloads — placeholder (no downloader bridge)".to_string(),
            _ => "CLOSED".to_string(),
        };
        println!("  {label:22} → mode {mode}: {detail}");
    }
    println!("{:-<72}", "");
    println!("toggle state-machine: {}", if ok { "PASS" } else { "FAIL" });
}

/// Poll now-playing once. Returns `Some(secs)` when Spotify rate-limited us (so the
/// caller can back off polling for that window) — in which case the current UI is left
/// untouched and no error is shown; `None` otherwise.
/// Liked-Songs state for the current track: the id it was last resolved for plus
/// the answer, so the extra `me/tracks/contains` call only fires on a track change.
#[derive(Default)]
struct LikeState {
    id: String,
    on: bool,
}

async fn refresh_playback(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    last: &mut Option<PlaybackState>,
    last_art: &mut String,
    liked: &mut LikeState,
) -> Option<u64> {
    match session.playback().await {
        Ok(Some(mut pb)) => {
            // A shuffle/repeat change the user just made wins over a poll that
            // hasn't caught up with it yet.
            apply_options_hold(&mut pb);
            // Real playback supersedes a resume offer, and becomes the next one.
            take_resume_offer();
            save_resume(&pb);
            // Fetch + decode the cover only when the art URL changes (not every poll).
            let art_url = pb.track.as_ref().map(|t| t.album_art.clone()).unwrap_or_default();
            // Before the push, so the media flyout picks up the new cover together
            // with the new title.
            #[cfg(windows)]
            media_controls::set_art_url(&art_url);
            push_now_playing(weak, &pb);
            if art_url != *last_art {
                *last_art = art_url.clone();
                // A failed fetch/decode clears the art rather than leaving the
                // previous track's cover (and accent) up for the whole new track.
                if art_url.is_empty() {
                    clear_art(weak);
                } else {
                    let decoded = match take_prefetched_art(&art_url) {
                        Some(d) => Some(d),
                        None => session.fetch_bytes(&art_url).await.ok().and_then(|b| decode_art(&b)),
                    };
                    match decoded {
                        Some((rgba, w, h, glow)) => set_art(weak, rgba, w, h, glow),
                        None => clear_art(weak),
                    }
                }
            }
            // Liked state follows the track: one me/tracks/contains per track change,
            // not per poll (mirrors the host's checkLiked on trackchanged).
            let tid = pb.track.as_ref().map(|t| t.id.clone()).unwrap_or_default();
            if tid != liked.id {
                liked.id = tid.clone();
                liked.on = if tid.is_empty() {
                    false
                } else {
                    session.is_track_saved(&tid).await.unwrap_or(false)
                };
                push_liked(weak, liked.on, false);
            }
            *last = Some(pb);
            None
        }
        Ok(None) if resume_offered().is_some() => {
            // Idle, but the resume offer is on screen: leave it there.
            *last = None;
            None
        }
        Ok(None) => {
            #[cfg(windows)]
            media_controls::set_art_url("");
            push_idle(weak);
            if !last_art.is_empty() {
                last_art.clear();
                clear_art(weak);
            }
            if !liked.id.is_empty() || liked.on {
                *liked = LikeState::default();
                push_liked(weak, false, false);
            }
            *last = None;
            None
        }
        Err(e) => {
            // A 429 is handled by the caller's backoff (leave the UI as-is, don't spam).
            if let Some(secs) = rate_limit_backoff(&e) {
                return Some(secs);
            }
            // Unreachable network: the poll loop shows one "Offline" line and backs
            // off, rather than an error per poll.
            if lightify_core::net::is_offline_error(&e) {
                return None;
            }
            set_status(weak, format!("Playback error \u{2014} {e}"));
            None
        }
    }
}

/// Hardware media keys (Play/Pause, Next, Previous, Stop).
///
/// The shipped app gets these for free: WebView2 delivers them as ordinary
/// `keydown`s with `e.key == "MediaPlayPause"` while the window has focus. Slint has
/// no key code for them at all, so the shell registers them with Win32 instead.
///
/// `RegisterHotKey` is **global**, which is the usual behaviour for a desktop music
/// player — the keys work whether or not Lightify has focus. It is also first-come,
/// first-served: if another player already holds a key, registration for that one
/// fails and we simply don't get it, which is the polite outcome rather than a fight.
#[cfg(windows)]
fn spawn_media_key_listener(tx: tokio::sync::mpsc::UnboundedSender<Cmd>) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    // (hotkey id, virtual-key code, what it does)
    const KEYS: [(i32, u32, &str); 4] = [
        (1, 0xB3, "play/pause"), // VK_MEDIA_PLAY_PAUSE
        (2, 0xB0, "next"),       // VK_MEDIA_NEXT_TRACK
        (3, 0xB1, "previous"),   // VK_MEDIA_PREV_TRACK
        (4, 0xB2, "stop"),       // VK_MEDIA_STOP
    ];

    std::thread::spawn(move || unsafe {
        let mut got = Vec::new();
        for (id, vk, what) in KEYS {
            // A thread-associated hotkey (null hwnd) posts WM_HOTKEY to this thread's
            // queue, so no window is needed — just the message loop below.
            if RegisterHotKey(std::ptr::null_mut(), id, 0, vk) != 0 {
                got.push(what);
            }
        }
        if got.is_empty() {
            eprintln!("[lightify] media keys: none available (another player holds them)");
            return;
        }
        eprintln!("[lightify] media keys: {}", got.join(", "));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message != WM_HOTKEY {
                continue;
            }
            let cmd = match msg.wParam as i32 {
                1 => Cmd::TogglePlay,
                2 => Cmd::Next,
                3 => Cmd::Prev,
                // There is no separate stop; pausing is the closest honest thing.
                4 => Cmd::TogglePlay,
                _ => continue,
            };
            if tx.send(cmd).is_err() {
                break; // the worker is gone; so is the app
            }
        }
    });
}

#[cfg(not(windows))]
fn spawn_media_key_listener(_tx: tokio::sync::mpsc::UnboundedSender<Cmd>) {}

/// Which lane currently owns "what plays next".
///
/// The shipped app juggles three (its own mirror, Spotify's queue, a context) and the
/// bugs always came from one lane's leftovers surviving into another. The shell only
/// has two — Spotify's context and Spotify's user queue — but **two different
/// features** write into that user queue ahead of the user, and either one's leftovers
/// can bleed into whatever is started next the same way:
///
/// * a **station**, which queues seed-adjacent tracks *and* switches spirc autoplay on
///   (so it leaves autoplay residue too — `autoplay = true` would keep radio-ing past
///   whatever context comes next); and
/// * a **Beatport chart**, which queues the chart's own upcoming rows behind the
///   playing track (`bp_refill`) exactly the same way a station does, just without the
///   autoplay flag.
///
/// This was only ever built for the station case (`end_station_authority`) — Beatport's
/// own queued-ahead tracks had no equivalent cleanup, which is exactly the reported bug:
/// a Beatport chart queues rows 2, 3, 4… ahead; the user then plays a library song
/// (which replaces the *context*, but never touches what was already sitting in the
/// queue from `add_to_queue`); Next/autoplay then serves one of the stale Beatport
/// rows instead of continuing the new library selection — the identical failure mode
/// `end_station_authority`'s own doc comment already described for stations, just for
/// the other lane that writes to the same place.
///
/// The original funnels every non-station play through `clearStationAutoplay` for the
/// autoplay flag; this generalizes that same idea to whichever of the two actually
/// wrote something. The queue reset is only issued when our own leftovers are
/// *actually* still queued, because the reset is all-or-nothing and would also drop
/// anything the user queued by hand.
///
/// The reset itself (`clearqueue`) is the same spirc disconnect+reactivate the
/// Clear/Remove-track buttons use, which only does anything real when Lightify's own
/// engine is the currently active device (`engine_owns_playback`) — there is no
/// Web-API endpoint that can touch someone else's queue. Both buttons already learned
/// to check this and say so honestly instead of lying about success; this function
/// used to skip that check entirely AND unconditionally forget `bp_queued`/
/// `station_queued` regardless of whether the reset actually landed, so a leftover
/// caught while the engine didn't own playback (a device handoff still settling, a
/// stale cached `last` read, …) was wiped from our own bookkeeping and then sat in
/// the *real* queue forever — invisible to every future call, reappearing (and
/// eventually playing) no matter what the user picked next: starting a new context
/// does not clear tracks Spotify already had explicitly queued ahead of it.
/// Bookkeeping is now only cleared once we've either confirmed there's nothing left
/// to do, or actually issued a reset that could reach it.
#[allow(clippy::too_many_arguments)]
async fn end_autoplay_authority(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    station_active: &mut bool,
    station_queued: &mut std::collections::HashSet<String>,
    bp_autoplay: &mut Option<BpAutoplay>,
    bp_queued: &mut std::collections::HashSet<String>,
    bp_active_seq: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
) {
    end_autoplay_authority_inner(
        weak, session, station_active, station_queued, bp_autoplay, bp_queued,
        bp_active_seq, last, engine_device_id, false,
    )
    .await
}

/// `end_autoplay_authority` for callers that keep the current song playing — a new
/// station queues behind it — so the leftovers are cleared without stopping it.
#[allow(clippy::too_many_arguments)]
async fn end_autoplay_authority_keep_playing(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    station_active: &mut bool,
    station_queued: &mut std::collections::HashSet<String>,
    bp_autoplay: &mut Option<BpAutoplay>,
    bp_queued: &mut std::collections::HashSet<String>,
    bp_active_seq: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
) {
    end_autoplay_authority_inner(
        weak, session, station_active, station_queued, bp_autoplay, bp_queued,
        bp_active_seq, last, engine_device_id, true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn end_autoplay_authority_inner(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    station_active: &mut bool,
    station_queued: &mut std::collections::HashSet<String>,
    bp_autoplay: &mut Option<BpAutoplay>,
    bp_queued: &mut std::collections::HashSet<String>,
    bp_active_seq: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
    keep_current: bool,
) {
    // Before anything else, and whatever the bookkeeping says: a station whose tracks
    // are still being resolved isn't in `station_active`/`station_queued` yet, so the
    // early return below can't see it — but it must not land after the user has moved
    // on. (A new station request bumps the epoch again right after this.)
    cancel_pending_station();
    let was_station = *station_active;
    // NOT `bp_autoplay.is_some()`: the poll tick already drops `bp_autoplay` back to
    // `None` on its own the moment the playing track leaves the chart's sequence
    // (`bp_remaining_ahead` returning `None`, above) — well before the user takes
    // whatever NEW action calls this function. `bp_queued` deliberately outlives that,
    // so the leftover check below still runs even when `bp_autoplay` is already gone.
    let had_bp_leftovers = !bp_queued.is_empty();
    if !was_station && !had_bp_leftovers {
        return;
    }
    *station_active = false;
    *bp_autoplay = None;
    // Synchronously, in the same breath as the reset above: any `bp_refill_background`
    // task still in flight for the abandoned sequence checks this before every
    // `add_to_queue` it makes and stops rather than writing a leftover we'd have no
    // record of (see that function's doc comment).
    bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
    if was_station {
        // Autoplay off first: cheap, and the thing that would otherwise silently turn
        // a playlist into endless radio. Beatport never touches this flag, so there's
        // nothing to undo for that lane.
        engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": false }).to_string());
    }

    // Peek without discarding: `station_queued`/`bp_queued` are only cleared once we
    // know the check below actually accounted for them, one way or the other.
    let mut queued: std::collections::HashSet<String> = station_queued.clone();
    queued.extend(bp_queued.iter().cloned());
    let Ok(q) = session.queue().await else {
        // Couldn't read the real queue at all — we don't know whether these
        // survived, so keep them tracked rather than guessing "gone".
        return;
    };
    if !q.iter().any(|t| queued.contains(&t.uri)) {
        // Confirmed gone (or never really landed) — safe to forget.
        station_queued.clear();
        bp_queued.clear();
        return;
    }
    if !engine_owns_playback(last, engine_device_id) {
        // The only real clear mechanism is spirc disconnect+reactivate against
        // Lightify's own device (see the Clear/Remove-track buttons' identical
        // guard) — with someone/something else as the active device right now,
        // sending it would silently hit nothing. Leave the bookkeeping in place so
        // the next call, once ownership is back, still catches this.
        return;
    }
    station_queued.clear();
    bp_queued.clear();
    set_status(weak, "Cleared the queued-ahead tracks".to_string());
    let reloaded_uri = if keep_current {
        last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default()
    } else {
        String::new()
    };
    if reloaded_uri.is_empty() {
        // About to start something else: the plain reset is enough. The device is
        // briefly deactivated by it; let it come back before the Web-API play that
        // follows targets it.
        engine::send(&clear_queue_command(&None, false));
        tokio::time::sleep(Duration::from_millis(400)).await;
    } else {
        // The song stays: reset + bring it back, and wait until it is — otherwise the
        // next poll can see "nothing playing" and a new station would take over
        // outright instead of queueing behind the song.
        clear_device_queue(session, last).await;
        let _ = queue_after_clear(session, &reloaded_uri).await;
    }
}

/// How many station tracks to put in the queue. Each one is a separate
/// `POST me/player/queue`, so this is a balance between a station worth listening to
/// and a burst of calls against the shared dev-mode quota.
/// Beatport chart autoplay — the shell's port of `state.beatportAutoplay`
/// (`app.js:3868 playBeatportSequenceFromIndex` + `app.js:3800 pumpBeatportAutoplay`).
///
/// Clicking a chart row in the original doesn't play one track: it plays from that
/// row and keeps **queueing the rest of the chart behind it**, resolving each
/// Beatport track to its Spotify match a batch at a time and topping the queue up as
/// playback advances. The shell used to play the single matched track and stop, which
/// is why the queue read empty after a Beatport click.
struct BpAutoplay {
    /// Identifies which sequence a background refill's result belongs to — see
    /// `Cmd::BpRefillDone`. A fresh chart click starts a new one; a stale result
    /// arriving for an abandoned/replaced sequence is discarded by id mismatch
    /// rather than misapplied to whatever is playing now.
    id: u64,
    /// The chart as it was when playback started (the visible list can change under us).
    source: Vec<BeatportTrack>,
    /// How far into `source` matching has got.
    next_index: usize,
    /// Spotify ids already queued, in queue order — `loadedIds` in the original.
    loaded_ids: Vec<String>,
    /// Spotify uris already used, so a chart with duplicates doesn't double-queue.
    seen: std::collections::HashSet<String>,
    /// Nothing left to match.
    completed: bool,
    /// A background refill (see `bp_refill_background`) is currently in flight for
    /// this sequence — don't dispatch a second one on top of it.
    refill_pending: bool,
}

/// `BEATPORT_INITIAL_MATCH_COUNT` — how many rows are matched before playback starts.
/// Deliberately tiny: this is the only part the user waits on.
///
/// **Must stay 1.** Playback starts with `play_uris(&first)`, and every further track is
/// added with `add_to_queue`. Spotify plays *queued* tracks before whatever is left of
/// the play context, so with two rows here the second one was the context's next track
/// and the rest of the chart — queued after it — jumped ahead of it: click #1 and the
/// queue came out 3, 4, 5, 6, … then 2 last (reported 2026-09-28). One track played, the
/// rest queued in chart order, keeps the order the chart shows. (Also one match less to
/// wait for before the music starts.)
const BP_INITIAL_MATCH: usize = 1;
/// `BEATPORT_AUTOPLAY_PREFILL_TARGET` — keep this many matched tracks queued ahead.
const BP_QUEUE_AHEAD: usize = 6;
/// Matches resolved per refill pass. The original doubles its batch up to 32 because
/// its refills run off the UI thread; the shell's worker owns the `Session`, so a
/// 32-track pass would block every other click for ~10s. Bounding the pass instead
/// and refilling on each poll tick keeps the queue just as far ahead without the
/// freeze — the one deliberate difference from `nextBeatportQueueBatchSize`.
const BP_REFILL_CHUNK: usize = 4;

const STATION_QUEUE_MAX: usize = 20;

/// How long to give Spotify's own queue ordering to settle after a batch
/// `me/player/queue` write before trusting `me/player/next` to advance by exactly
/// one track. Verified live (raw API, no Lightify code involved) that this is a
/// genuine server-side race, not something in our own `next()` call: queueing 2
/// tracks then calling `next()` with no delay reliably skips an extra one (lands
/// on the *2nd* queued track); with enough of a gap it lands on the 1st, as
/// expected. The race is non-deterministic in how long it needs — even 1.5s
/// wasn't 100% reliable in testing — so this reduces how often it's hit rather
/// than guaranteeing it can never happen; there is no client-side fix for a
/// server-side ordering race. 2s is a compromise between "meaningfully safer"
/// and "Next still feels responsive" for the rare case of clicking it within a
/// couple seconds of queueing a whole selection/playlist.
const QUEUE_SETTLE: Duration = Duration::from_millis(2000);

/// The Connect device name the bundled engine registers (`DEVICE_NAME` in
/// `lightify-audio`), used to tell our own device apart from everyone else's.
const ENGINE_DEVICE_NAME: &str = "Lightify";

/// Did playback move to some *other* Connect device?
///
/// The engine dying looks identical from here whatever the cause, but the right
/// response is opposite in the two common ones:
///
/// * Spotify closed our session because the account started playing elsewhere (the
///   desktop app, a phone). Another device is active. Restarting would re-adopt our
///   device (`PUT me/player`) and **steal playback back** from what the user just
///   chose — so don't.
/// * Our audio sink died: the output device was unplugged or Windows moved the
///   default ("The requested device is no longer available"). Nothing else is
///   active, playback simply stopped. Coming back on the freshly-resolved default is
///   exactly what the user wants.
/// Accept a reported volume only when it is allowed to move the slider.
///
/// Rejects anything that arrives while the user is dragging, and - the flicker this
/// exists for - anything that contradicts a change they just made and the service
/// has not echoed back yet. A report that matches clears the latch.
fn apply_reported_volume(app: &MainWindow, volume: f32) {
    if app.get_scrubbing_volume() {
        return;
    }
    if app.get_volume_pending() {
        // 1% either way: the wire carries whole percent, the UI carries 0..1.
        if (volume - app.get_volume_target()).abs() > 0.011 {
            return;
        }
        app.set_volume_pending(false);
    }
    app.set_volume(volume);
}

/// Is our own engine the thing currently playing?
///
/// When it is, transport should go through the engine rather than the Web API.
/// `me/player/next` is a *request to Spotify's connect state*, and that state can
/// refuse it — a station or a single-track context comes back
/// `403 Player command failed: Restriction violated`, which is a policy answer, not
/// a real inability: our device holds the tracks and can simply advance. Going
/// local is also instant and costs no API quota. The shipped host draws the same
/// line for seeking (`stationOwnsPlayback() ? cmd_engine_seek : cmd_seek`).
///
/// Matched by **device id**, not `device_name`. The name alone can't tell our live
/// engine apart from any *other* Spotify Connect device also named "Lightify" —
/// which is not hypothetical: librespot mints a fresh random device id on every
/// launch (`SessionConfig::default()`, never persisted), so a previous run's engine
/// still shows up under the identical name for as long as Spotify takes to notice
/// it's gone. A false match here had no fallback (unlike a false *miss*, which
/// already retries through the engine on a 403 below) — `spirc.next()`/`prev()` would
/// silently go to a session that isn't actually the one playing, and nothing would
/// happen. `PlaybackState.device_id` comes from the same `/me/player` read as
/// `device_name`, so this is strictly more precise at no extra cost.
fn engine_owns_playback(last: &Option<PlaybackState>, our_device_id: &str) -> bool {
    engine::running()
        && !our_device_id.is_empty()
        && last.as_ref().is_some_and(|p| p.device_id == our_device_id)
}

/// Did Spotify refuse a player command on policy grounds rather than because
/// something was actually wrong? Those are the ones worth retrying on the engine.
fn is_player_restriction(e: &str) -> bool {
    e.contains("Restriction violated") || e.contains("restriction")
}

/// Has our engine moved on to a track `last` doesn't know about yet? Happens while a
/// rate-limit backoff blocks the refresh that would normally follow `Event::Track`.
fn last_is_stale(last: &Option<PlaybackState>, engine_track_name: &str) -> bool {
    let known = last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.name.as_str()).unwrap_or("");
    // Only while a cool-down is what's blocking the refresh: the engine's metadata
    // name and the Web API's can legitimately differ (market-localised titles), and
    // that must not lock these actions out forever.
    lightify_core::ratelimit::blocked_for().is_some()
        && !engine_track_name.is_empty()
        && known != engine_track_name
}

const STALE_TRACK_NOTE: &str =
    "Waiting for Spotify to confirm the new track \u{2014} try again in a moment.";

fn another_device_active(devices: &[Device], ours: &str) -> bool {
    devices.iter().any(|d| d.is_active && d.name != ours)
}

/// Restart budget for the playback engine. A lost audio device is worth recovering
/// from automatically, but an engine that cannot start at all (no output, revoked
/// credential) must not spin forever hammering Spotify and the log.
struct EngineWatchdog {
    restarts: u32,
    window_start: Option<std::time::Instant>,
}

impl EngineWatchdog {
    const MAX_RESTARTS: u32 = 3;
    const WINDOW: Duration = Duration::from_secs(300);

    fn new() -> Self {
        Self { restarts: 0, window_start: None }
    }

    /// Consumes one attempt. The budget refills once `WINDOW` has elapsed since the
    /// first restart in it, so an engine that dies once an hour is always recovered.
    fn should_restart(&mut self, now: std::time::Instant) -> bool {
        match self.window_start {
            Some(start) if now.duration_since(start) < Self::WINDOW => {}
            _ => {
                self.window_start = Some(now);
                self.restarts = 0;
            }
        }
        if self.restarts < Self::MAX_RESTARTS {
            self.restarts += 1;
            true
        } else {
            false
        }
    }

    /// An engine that registered its device is healthy; forget the old crash run.
    fn note_healthy(&mut self) {
        self.restarts = 0;
        self.window_start = None;
    }
}

/// Start the bundled playback engine and pipe its events onto the worker channel.
/// Failure is never fatal: the shell still works as a remote for an existing device,
/// it just says so plainly instead of telling the user to go open Spotify.
fn start_engine(
    weak: &slint::Weak<MainWindow>,
    session: &Session,
    tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
) {
    let Some(bin) = engine::find_binary() else {
        let msg = "Playback engine not found \u{2014} build it with `cargo build --release` in lightify-audio";
        push_engine_status(weak, "Not found".to_string(), false);
        set_status(weak, msg.to_string());
        return;
    };
    // Never pass a device name we haven't just confirmed exists (see resolve_output).
    let dir = lightify_core::config::data_dir();
    let configured = lightify_core::config::load_config(&dir).audio_output;
    let (output, stale) = match engine::resolve_output(&configured) {
        Ok(o) => (o, None),
        Err(missing) => {
            // Heal the stored preference rather than silently falling back to the
            // system default forever: a pinned device that has vanished (unplugged,
            // renamed, a Bluetooth pairing that didn't survive a reinstall) used to
            // leave `audio_output` non-empty on disk permanently. That string is also
            // what gates "live-follow the OS default" below (`configured_output.trim()
            // .is_empty()`) — so a stale pin didn't just pick the wrong device once at
            // startup, it silently disabled ever noticing a *future* default-device
            // change too, which is exactly the "changing Windows' default output does
            // nothing" bug this fixes. Settings' own row already shows "System default"
            // as checked in this situation (`push_outputs`'s `missing` fallback); this
            // makes that actually true on disk, not just in the display.
            let _ = lightify_core::config::update_config(&dir, |cfg| cfg.audio_output.clear());
            (None, Some(missing))
        }
    };
    let tx = tx.clone();
    match engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(Cmd::Engine(ev));
    }) {
        Ok(()) => {
            let msg = if let Some(missing) = stale {
                format!("Audio output \u{201c}{missing}\u{201d} isn\u{2019}t available \u{2014} playing on the system default")
            } else if engine::has_cached_login() {
                "Starting Lightify playback\u{2026}".to_string()
            } else {
                "One-time Spotify streaming sign-in \u{2014} finish it in your browser".to_string()
            };
            push_engine_status(weak, msg.clone(), false);
            set_status(weak, msg);
        }
        Err(e) => {
            push_engine_status(weak, format!("Failed \u{2014} {e}"), false);
            set_status(weak, format!("Playback engine \u{2014} {e}"));
        }
    }
}

/// Build the audio-output rows: "System default" first, then the real devices, with
/// the configured one ticked. When the configured device has vanished the note says
/// so plainly instead of leaving the user staring at an unticked list.
fn push_outputs(weak: &slint::Weak<MainWindow>) {
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let outputs = engine::audio_outputs();
    let want = configured.trim();
    let missing = !want.is_empty()
        && !want.eq_ignore_ascii_case("default")
        && !outputs.iter().any(|n| n == want);
    let default_active = want.is_empty() || want.eq_ignore_ascii_case("default") || missing;

    let mut rows: Vec<DeviceRow> = Vec::with_capacity(outputs.len() + 1);
    rows.push(DeviceRow { name: "System default".into(), active: default_active });
    for name in &outputs {
        rows.push(DeviceRow { name: name.clone().into(), active: !default_active && name == want });
    }
    let note = if missing {
        format!("Configured output \u{201c}{want}\u{201d} isn\u{2019}t connected \u{2014} playing on the system default.")
    } else {
        String::new()
    };
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_settings_outputs(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        app.set_settings_output_note(note.into());
    });
}

fn push_engine_status(weak: &slint::Weak<MainWindow>, text: String, ok: bool) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_engine_status(text.into());
        app.set_engine_ok(ok);
    });
}

/// The engine's own Connect device, as the **Web API** sees it.
///
/// The id librespot reports over stdout is its internal one and does not have to
/// match the id Spotify hands out, and registration propagates to `me/player/devices`
/// a second or two after the engine says `ready` — so match on the id when it does
/// line up, otherwise on the device name, and poll until it shows up. This is the
/// same wait the shipped host does in `ensureDevice`.
async fn find_engine_device(
    session: &mut Session,
    device_id: &str,
    attempts: usize,
) -> Option<Device> {
    for i in 0..attempts.max(1) {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        let devices = session.devices().await.unwrap_or_default();
        let found = devices
            .iter()
            .find(|d| !device_id.is_empty() && d.id == device_id)
            .or_else(|| devices.iter().find(|d| d.name.to_lowercase().contains("lightify")))
            .cloned();
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Make the engine's Connect device the active one, so every Web-API transport call
/// lands on Lightify itself. Returns the device's display name.
/// Point the account's playback at our own engine and remember the device so every
/// later transport call names it explicitly.
///
/// Two things here were wrong and are the reason playback could fail outright:
///
/// * **Never transfer to a device Spotify already reports as active.** `PUT me/player`
///   answers a redundant transfer with a **500**, and the old code transferred
///   unconditionally — so on the common path (our engine registers and Spotify marks
///   it active straight away) adoption failed every time. The shipped host guards the
///   same way (`ensure_active_device_id`: `if !chosen.is_active && !already_selected`).
/// * **A failed transfer is not fatal.** The device is registered either way, and
///   player calls now carry `?device_id=`, so they land regardless. Returning `Err`
///   here only produced a scary status line for a working setup.
async fn adopt_engine_device(session: &mut Session, device_id: &str) -> Result<String, String> {
    let d = find_engine_device(session, device_id, 12)
        .await
        .ok_or("the engine registered but Spotify hasn\u{2019}t listed it yet")?;
    // Aim every subsequent player call at this device, transfer or not.
    session.remember_device(&d.id);
    if !d.is_active {
        if let Err(e) = session.transfer_playback(&d.id, false).await {
            eprintln!("[lightify] transfer to {} failed (continuing): {e}", d.id);
        }
    }
    Ok(d.name)
}

/// Is the settings modal currently showing the device list?
/// Whether the Settings modal is showing, readable from the worker thread. The old
/// `weak.upgrade()` read always failed there — Slint only upgrades a component handle
/// on the thread that created it — so this was permanently false and the device
/// list never refreshed after the engine came up.
static SETTINGS_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn note_settings_open(open: bool) {
    SETTINGS_OPEN.store(open, std::sync::atomic::Ordering::Relaxed);
}

fn devices_open(_weak: &slint::Weak<MainWindow>) -> bool {
    SETTINGS_OPEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Seconds to back off if `e` is a Spotify rate-limit error (from `Retry-After`),
/// else None. Matches the "Rate limited by Spotify — retry in {n}s" core formatting.
/// Respects the server's value (bounded to 1h) — retrying before it elapses only
/// wastes quota and can re-trigger the penalty window; a QUOTA_EXCEEDED backoff can
/// legitimately be many minutes.
fn rate_limit_backoff(e: &str) -> Option<u64> {
    if !e.starts_with("Rate limited") {
        return None;
    }
    let marker = "retry in ";
    let idx = e.find(marker)? + marker.len();
    let secs: String = e[idx..].chars().take_while(|c| c.is_ascii_digit()).collect();
    secs.parse::<u64>().ok().map(|s| s.clamp(1, 3600))
}

/// Human-readable backoff duration — minutes for long (quota) waits, seconds for short spikes.
fn fmt_backoff(secs: u64) -> String {
    if secs >= 90 {
        format!("{}m", (secs + 59) / 60)
    } else {
        format!("{secs}s")
    }
}

// ── UI marshaling helpers (run the closure on the Slint thread) ──────────────

fn set_status(weak: &slint::Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_status_text(text.into()));
}

/// Library rows: "Liked Songs" pinned at the top (the saved-tracks pseudo-playlist),
/// then the user's playlists in `order` (the sort toolbar's display order), each with
/// the two-line subtitle. `active_id` marks the row playback is currently coming from.
fn build_rows(
    pls: &[lightify_core::Playlist],
    order: &[usize],
    active_id: &str,
    playing: bool,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::with_capacity(order.len() + 1);
    let liked_active = active_id == LIKED_SOURCE;
    rows.push(Row {
        id: LIKED_SOURCE.into(),
        name: "Liked Songs".into(),
        sub: "Your saved Spotify tracks".into(),
        active: liked_active,
        playing: liked_active && playing,
        liked: true,
    });
    for &idx in order {
        let Some(p) = pls.get(idx) else { continue };
        let owner = if p.owner.is_empty() {
            String::new()
        } else {
            format!(" \u{2022} {}", p.owner)
        };
        let active = !active_id.is_empty() && p.id == active_id;
        rows.push(Row {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            sub: format!("{} tracks{}", group_thousands(p.tracks), owner).into(),
            active,
            playing: active && playing,
            liked: false,
        });
    }
    rows
}

/// Pseudo-playlist id for the pinned "Liked Songs" row (the host uses the same token).
const LIKED_SOURCE: &str = "liked";

/// On-disk copy of the last fetched library (UI-PLAN D2), shown at launch before the
/// network answers. Lives beside the other shell state in the data dir.
fn library_cache_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_library.json")
}

fn load_library_cache() -> Option<Vec<lightify_core::Playlist>> {
    let bytes = std::fs::read(library_cache_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the library, skipping the write when nothing changed (the common case —
/// every launch fetches it again).
fn remember_library(pls: &[lightify_core::Playlist]) {
    let Ok(bytes) = serde_json::to_vec(pls) else { return };
    let path = library_cache_path();
    if std::fs::read(&path).is_ok_and(|old| old == bytes) {
        return;
    }
    let _ = lightify_core::config::write_atomic(&path, &bytes);
}

/// `localeCompare(.., { sensitivity: 'base', numeric: true })` in Rust: case-insensitive,
/// with digit runs compared as numbers so "Set 2" sorts before "Set 10".
fn cmp_display_text(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    fn take_num(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
        let mut n = String::new();
        while let Some(c) = it.peek().copied() {
            if c.is_ascii_digit() {
                n.push(c);
                it.next();
            } else {
                break;
            }
        }
        let trimmed = n.trim_start_matches('0');
        trimmed.to_string()
    }
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ac), Some(bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let an = take_num(&mut ai);
                    let bn = take_num(&mut bi);
                    match an.len().cmp(&bn.len()).then_with(|| an.cmp(&bn)) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                }
                let al = ac.to_lowercase().next().unwrap_or(ac);
                let bl = bc.to_lowercase().next().unwrap_or(bc);
                match al.cmp(&bl) {
                    Ordering::Equal => {
                        ai.next();
                        bi.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

/// Display order for the playlist list: API order ("recent") or by name, honouring
/// the direction toggle. Mirrors `sortPlaylistsForDisplay`.
// ── Type-to-filter (UI-PLAN D7) ─────────────────────────────────────────────
//
// One filter for the playlist list, one for the open track list. They live here,
// inside the display-order functions, so every path that rebuilds a view (sort, page
// load, library refresh, …) filters too, and row clicks keep mapping through the
// view to the right item. Liked Songs stays pinned as row 0 of the library.
static PL_FILTER: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static TRK_FILTER: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn filter_of(f: &std::sync::Mutex<String>) -> String {
    f.lock().map(|g| g.to_lowercase()).unwrap_or_default()
}

fn set_filter(f: &std::sync::Mutex<String>, text: &str) {
    if let Ok(mut g) = f.lock() {
        *g = text.to_string();
    }
}

/// Clear both filters and the pill (on drill-in / back).
fn clear_list_filters(weak: &slint::Weak<MainWindow>) {
    set_filter(&PL_FILTER, "");
    set_filter(&TRK_FILTER, "");
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_list_filter("".into());
        app.set_list_filter_count(-1);
    });
}

fn sort_playlist_view(pls: &[lightify_core::Playlist], mode: &str, dir: &str) -> Vec<usize> {
    let q = filter_of(&PL_FILTER);
    let mut view: Vec<usize> = (0..pls.len())
        .filter(|&i| q.is_empty() || pls[i].name.to_lowercase().contains(&q) || pls[i].owner.to_lowercase().contains(&q))
        .collect();
    if mode != "alpha" {
        return view;
    }
    let asc = dir != "desc";
    view.sort_by(|&l, &r| {
        let o = cmp_display_text(&pls[l].name, &pls[r].name);
        if asc { o } else { o.reverse() }
    });
    view
}

/// A track's "added to this playlist / saved at" stamp. ISO-8601, so it orders
/// lexicographically; "" (absent) sorts last, matching `trackAddedAtMs`'s 0 fallback.
fn track_added_key(t: &Track) -> &str {
    t.added_at.as_deref().unwrap_or("")
}

/// Display order for the drilled track list. Mirrors `sortTracksForDisplay`:
/// alpha -> name then artist (direction-aware); recent -> `added_at` newest-first;
/// both fall back to the original API index so the sort stays stable.
fn sort_track_view(tracks: &[Track], mode: &str, dir: &str) -> Vec<usize> {
    let q = filter_of(&TRK_FILTER);
    let mut view: Vec<usize> = (0..tracks.len())
        .filter(|&i| {
            q.is_empty()
                || tracks[i].name.to_lowercase().contains(&q)
                || tracks[i].artists.to_lowercase().contains(&q)
                || tracks[i].album.to_lowercase().contains(&q)
        })
        .collect();
    let asc = dir != "desc";
    view.sort_by(|&l, &r| {
        let a = &tracks[l];
        let b = &tracks[r];
        let primary = if mode == "alpha" {
            let o = cmp_display_text(&a.name, &b.name)
                .then_with(|| cmp_display_text(&a.artists, &b.artists));
            if asc { o } else { o.reverse() }
        } else {
            track_added_key(b).cmp(track_added_key(a))
        };
        primary.then_with(|| l.cmp(&r))
    });
    view
}

/// The playlist id inside a playback context URI (`spotify:playlist:ID`, and the
/// legacy `spotify:user:<id>:playlist:ID`). `None` for albums/artists/no context.
fn context_playlist_id(ctx: &Option<String>) -> Option<String> {
    let uri = ctx.as_deref()?;
    let rest = uri.rsplit_once(":playlist:")?.1;
    let id = rest.split(':').next().unwrap_or(rest).trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Which library row playback is coming from: the live context URI wins (it also
/// reflects playback started elsewhere), else the source the user last launched from
/// a row (Liked Songs plays as bare `uris`, so it carries no context).
fn active_source(last: &Option<PlaybackState>, remembered: &Option<String>) -> (String, bool) {
    let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
    let from_ctx = last.as_ref().and_then(|p| context_playlist_id(&p.context_uri));
    let id = from_ctx.or_else(|| remembered.clone()).unwrap_or_default();
    (id, playing)
}

fn push_sort(weak: &slint::Weak<MainWindow>, mode: &str, dir: &str) {
    let mode = mode.to_string();
    let dir = dir.to_string();
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_lib_sort_mode(mode.into());
        app.set_lib_sort_dir(dir.into());
    });
}

fn push_liked(weak: &slint::Weak<MainWindow>, liked: bool, pending: bool) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_liked_current(liked);
        app.set_like_pending(pending);
    });
}

fn push_rows(weak: &slint::Weak<MainWindow>, rows: Vec<Row>) {
    let v = list_pushed(0, list_identity(rows.iter().map(|r| r.id.as_str())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        // Unchanged rows (a refresh that found the same library) touch nothing.
        let cur = app.get_playlists();
        let same = cur.row_count() == rows.len() && rows.iter().enumerate().all(|(i, r)| cur.row_data(i).as_ref() == Some(r));
        if !same {
            let model = slint::ModelRc::from(Rc::new(slint::VecModel::from(rows)));
            app.set_playlist_art(thumbs::art_model(&model));
            app.set_playlists(model);
        }
        list_shown(0, v);
    });
}

fn push_now_playing(weak: &slint::Weak<MainWindow>, pb: &PlaybackState) {
    let (name, artist) = pb
        .track
        .as_ref()
        .map(|t| (t.name.clone(), t.artists.clone()))
        .unwrap_or_else(|| ("\u{2014}".into(), String::new()));
    let dur = pb.duration_ms;
    let pos = pb.progress_ms.min(dur.max(1));
    let progress = if dur > 0 { pos as f32 / dur as f32 } else { 0.0 };
    let elapsed = fmt_time(pos);
    let duration = fmt_time(dur);
    let playing = pb.is_playing;
    let volume = (pb.volume_percent as f32 / 100.0).clamp(0.0, 1.0);
    let shuffle = pb.shuffle_state;
    let repeat = pb.repeat_state.clone();
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_track_name(name.into());
        app.set_track_artist(artist.into());
        app.set_progress(progress);
        app.set_elapsed(elapsed.into());
        app.set_duration(duration.into());
        app.set_playing(playing);
        apply_reported_volume(&app, volume);
        app.set_shuffle_on(shuffle);
        app.set_repeat_mode(repeat.into());
    });
}

// ── Resume where you left off (UI-PLAN D3) ───────────────────────────────────

/// The last track this account played, with where it stopped — saved on every
/// playback read, offered at launch when nothing is playing anywhere.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ResumePoint {
    track_uri: String,
    context_uri: Option<String>,
    position_ms: u64,
    duration_ms: u64,
    name: String,
    artists: String,
    album_art: String,
}

/// The offer currently on screen (`None` once taken, or once real playback shows up).
static RESUME_OFFER: std::sync::Mutex<Option<ResumePoint>> = std::sync::Mutex::new(None);

fn resume_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_resume.json")
}

fn load_resume() -> Option<ResumePoint> {
    let rp: ResumePoint = serde_json::from_slice(&std::fs::read(resume_path()).ok()?).ok()?;
    (!rp.track_uri.is_empty()).then_some(rp)
}

/// Persist the resume point. Skips the write unless the track changed or the
/// position moved >= 5 s since the last save (polls are >= 10 s apart while playing,
/// so this is at most one small write per poll).
fn save_resume(pb: &PlaybackState) {
    static SAVED: std::sync::Mutex<Option<ResumePoint>> = std::sync::Mutex::new(None);
    let Some(t) = pb.track.as_ref() else { return };
    if t.uri.is_empty() {
        return;
    }
    let rp = ResumePoint {
        track_uri: t.uri.clone(),
        context_uri: pb.context_uri.clone(),
        position_ms: pb.progress_ms.min(pb.duration_ms),
        duration_ms: pb.duration_ms,
        name: t.name.clone(),
        artists: t.artists.clone(),
        album_art: t.album_art.clone(),
    };
    let Ok(mut saved) = SAVED.lock() else { return };
    if let Some(prev) = saved.as_ref() {
        if prev.track_uri == rp.track_uri && prev.position_ms.abs_diff(rp.position_ms) < 5_000 {
            return;
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&rp) {
        let _ = lightify_core::config::write_atomic(&resume_path(), &bytes);
    }
    *saved = Some(rp);
}

/// A resume point as a paused `PlaybackState` (headless renders).
fn resume_as_playback(rp: &ResumePoint) -> PlaybackState {
    PlaybackState {
        is_playing: false,
        progress_ms: rp.position_ms,
        duration_ms: rp.duration_ms,
        shuffle_state: false,
        repeat_state: "off".into(),
        volume_percent: 100,
        device_name: String::new(),
        device_id: String::new(),
        track: Some(Track {
            id: rp.track_uri.rsplit(':').next().unwrap_or("").to_string(),
            name: rp.name.clone(),
            artists: rp.artists.clone(),
            artist_ids: Vec::new(),
            album: String::new(),
            album_id: String::new(),
            duration_ms: rp.duration_ms,
            uri: rp.track_uri.clone(),
            album_art: rp.album_art.clone(),
            added_at: None,
            is_playable: true,
        }),
        context_uri: rp.context_uri.clone(),
    }
}

fn resume_offered() -> Option<ResumePoint> {
    RESUME_OFFER.lock().ok().and_then(|g| g.clone())
}

fn take_resume_offer() -> Option<ResumePoint> {
    RESUME_OFFER.lock().ok().and_then(|mut g| g.take())
}

/// Show the resume point as a paused track (title, artist, cover, position).
async fn offer_resume(weak: &slint::Weak<MainWindow>, session: &mut Session, rp: &ResumePoint, last_art: &mut String) {
    if let Ok(mut g) = RESUME_OFFER.lock() {
        *g = Some(rp.clone());
    }
    #[cfg(windows)]
    media_controls::set_art_url(&rp.album_art);
    let (name, artists) = (rp.name.clone(), rp.artists.clone());
    let dur = rp.duration_ms;
    let pos = rp.position_ms.min(dur);
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_track_name(name.into());
        app.set_track_artist(artists.into());
        app.set_progress(if dur > 0 { pos as f32 / dur as f32 } else { 0.0 });
        app.set_elapsed(fmt_time(pos).into());
        app.set_duration(fmt_time(dur).into());
        app.set_playing(false);
    });
    set_status(weak, "Press play to pick up where you left off".to_string());
    if !rp.album_art.is_empty() && *last_art != rp.album_art {
        *last_art = rp.album_art.clone();
        if let Some((rgba, w, h, glow)) = session.fetch_bytes(&rp.album_art).await.ok().and_then(|b| decode_art(&b)) {
            set_art(weak, rgba, w, h, glow);
        }
    }
}

// ── Shuffle / repeat state ───────────────────────────────────────────────────

/// off → context (repeat all) → track (repeat one) → off.
fn next_repeat_mode(cur: &str) -> &'static str {
    match cur {
        "off" => "context",
        "context" => "track",
        _ => "off",
    }
}

/// A shuffle/repeat value the user just chose, held against polls that still report
/// the old one (the Web API lags a change by a moment). Each field lets go as soon as
/// a poll agrees, or when the hold runs out.
struct OptionsHold {
    shuffle: Option<bool>,
    repeat: Option<String>,
    until: std::time::Instant,
}

static OPTIONS_HOLD: std::sync::Mutex<Option<OptionsHold>> = std::sync::Mutex::new(None);
const OPTIONS_HOLD_FOR: Duration = Duration::from_secs(6);

fn release_options_hold() {
    if let Ok(mut g) = OPTIONS_HOLD.lock() {
        *g = None;
    }
}

/// Show a shuffle/repeat change at once (UI + `last`) and hold it.
fn show_options(
    weak: &slint::Weak<MainWindow>,
    last: &mut Option<PlaybackState>,
    shuffle: Option<bool>,
    repeat: Option<String>,
) {
    if let Some(pb) = last.as_mut() {
        if let Some(s) = shuffle {
            pb.shuffle_state = s;
        }
        if let Some(r) = repeat.as_ref() {
            pb.repeat_state = r.clone();
        }
    }
    if let Ok(mut g) = OPTIONS_HOLD.lock() {
        let h = g.get_or_insert(OptionsHold { shuffle: None, repeat: None, until: std::time::Instant::now() });
        if shuffle.is_some() {
            h.shuffle = shuffle;
        }
        if repeat.is_some() {
            h.repeat = repeat.clone();
        }
        h.until = std::time::Instant::now() + OPTIONS_HOLD_FOR;
    }
    let _ = weak.upgrade_in_event_loop(move |app| {
        if let Some(s) = shuffle {
            app.set_shuffle_on(s);
        }
        if let Some(r) = repeat {
            app.set_repeat_mode(r.into());
        }
    });
}

/// Apply a held shuffle/repeat choice to a fresh poll (see `OptionsHold`).
fn apply_options_hold(pb: &mut PlaybackState) {
    let Ok(mut g) = OPTIONS_HOLD.lock() else { return };
    let Some(h) = g.as_mut() else { return };
    if std::time::Instant::now() >= h.until {
        *g = None;
        return;
    }
    if let Some(s) = h.shuffle {
        if pb.shuffle_state == s {
            h.shuffle = None; // caught up
        } else {
            pb.shuffle_state = s;
        }
    }
    if let Some(r) = h.repeat.clone() {
        if pb.repeat_state == r {
            h.repeat = None;
        } else {
            pb.repeat_state = r;
        }
    }
    if h.shuffle.is_none() && h.repeat.is_none() {
        *g = None;
    }
}

fn push_idle(weak: &slint::Weak<MainWindow>) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_track_name("\u{2014}".into());
        app.set_track_artist("Nothing playing".into());
        app.set_progress(0.0);
        app.set_elapsed("0:00".into());
        app.set_duration("0:00".into());
        app.set_playing(false);
        app.set_shuffle_on(false);
        app.set_repeat_mode("off".into());
    });
}

/// A `tokio::time::interval` that never bursts. The default `MissedTickBehavior::Burst`
/// fires every missed tick back-to-back once the loop gets free again — for a poll,
/// that is N redundant Web-API requests in a row right after a slow command.
fn poll_interval(every: Duration) -> tokio::time::Interval {
    let mut i = tokio::time::interval(every);
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    i
}

/// Playback-poll floor while our own engine owns playback. The engine already pushes
/// play/pause, seeks, volume and every track change (`engine::Event`), so the Web-API
/// poll is only reconciliation there (shuffle/repeat changed elsewhere, a handoff).
const ENGINE_OWNED_POLL: Duration = Duration::from_secs(15);
/// Playback-poll floor while nothing is playing: nothing to animate, and a resume from
/// another device showing up a few seconds late is harmless.
const IDLE_POLL: Duration = Duration::from_secs(10);

/// How long until the next `me/player` poll. The user's Settings choice is the
/// cadence while the state can only be learned by asking (another device playing);
/// otherwise it's stretched, and doubled again whenever the rolling rate window is
/// busy so background traffic yields to what the user is actually doing.
fn playback_poll_every(base: Duration, last: &Option<PlaybackState>, engine_owned: bool) -> Duration {
    let playing = last.as_ref().is_some_and(|p| p.is_playing);
    let every = if engine_owned {
        base.max(ENGINE_OWNED_POLL)
    } else if !playing {
        base.max(IDLE_POLL)
    } else {
        base
    };
    if lightify_core::ratelimit::background_ok() { every } else { every * 2 }
}

/// How often the local progress clock redraws the seek bar.
const PROGRESS_CLOCK: Duration = Duration::from_millis(500);

/// Local progress extrapolation.
///
/// The seek bar used to move only when a network poll landed — a visible 5-second
/// jump on the default SLOW setting, which is exactly what pushed people to the 1 s
/// setting and five times the Web-API traffic. Now the bar runs off a local clock
/// anchored to the last known position (from a poll or an engine `Position` event),
/// so the display cadence no longer depends on the network cadence at all.
///
/// It also knows when the track *should* end: for playback on another device (no
/// engine events to tell us), `tick` asks for one poll right after that moment so the
/// next track shows up promptly even though polling is slow.
#[derive(Default)]
struct ProgressClock {
    anchor_ms: u64,
    anchor_at: Option<tokio::time::Instant>,
    track: String,
    /// The track an end-of-track poll was already requested for (once per play).
    end_polled: String,
    /// Time left in the track at the last tick (0 = unknown / not playing).
    remaining_ms: u64,
}

impl ProgressClock {
    /// Advance the bar. Returns true when an end-of-track poll is due now.
    fn tick(&mut self, weak: &slint::Weak<MainWindow>, last: &Option<PlaybackState>, engine_owned: bool) -> bool {
        let now = tokio::time::Instant::now();
        self.remaining_ms = 0;
        let Some(pb) = last.as_ref() else {
            self.anchor_at = None;
            return false;
        };
        let id = pb.track.as_ref().map(|t| t.id.as_str()).unwrap_or("");
        let moved = pb.progress_ms != self.anchor_ms || id != self.track;
        if !pb.is_playing || self.anchor_at.is_none() || moved {
            if id != self.track || pb.progress_ms < self.anchor_ms {
                // New track, or the same one again (repeat-one / seek back).
                self.end_polled.clear();
            }
            self.track = id.to_string();
            self.anchor_ms = pb.progress_ms;
            self.anchor_at = Some(now);
            // The fresh value was already drawn by whoever set it.
            return false;
        }
        let dur = pb.duration_ms;
        let Some(at) = self.anchor_at else { return false };
        if dur == 0 {
            return false;
        }
        let pos = self.anchor_ms + (now - at).as_millis() as u64;
        let shown = pos.min(dur);
        self.remaining_ms = dur - shown;
        let frac = (shown as f32 / dur as f32).clamp(0.0, 1.0);
        // Minimized: nothing to see, so don't wake the UI thread to lay out a frame.
        // The first tick after restore draws the right position (it's computed from
        // the anchor, not accumulated), so there's nothing to catch up on.
        if !visibility::hidden() {
            let elapsed = fmt_time(shown);
            let _ = weak.upgrade_in_event_loop(move |app| {
                if !app.get_scrubbing_progress() {
                    app.set_progress(frac);
                    app.set_elapsed(elapsed.into());
                }
            });
        }
        if !engine_owned && pos >= dur + 1000 && self.end_polled != self.track {
            self.end_polled = self.track.clone();
            return true;
        }
        false
    }
}

async fn settle_refresh(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    last: &mut Option<PlaybackState>,
    last_art: &mut String,
    liked: &mut LikeState,
) {
    tokio::time::sleep(Duration::from_millis(350)).await;
    refresh_playback(weak, session, last, last_art, liked).await;
}

fn connected_status(session: &Session) -> String {
    let n = session.display_name();
    if n.is_empty() { "Connected".to_string() } else { format!("Connected as {n}") }
}

fn set_drill(weak: &slint::Weak<MainWindow>, drilled: bool, title: String) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_drilled(drilled);
        app.set_drill_title(title.into());
    });
}

/// Push the drilled track list in `view` order (the sort toolbar's display order;
/// row clicks map back through the same view).
fn push_tracks(weak: &slint::Weak<MainWindow>, tracks: &[Track], view: &[usize]) {
    let rows: Vec<Trk> = view
        .iter()
        .filter_map(|&i| tracks.get(i))
        .map(|t| Trk {
            uri: t.uri.clone().into(),
            name: t.name.clone().into(),
            artist: t.artists.clone().into(),
            playable: t.is_playable,
            active: false,
            playing: false,
            selected: false,
        })
        .collect();
    let v = list_pushed(SEL_TRACKS as usize, list_identity(rows.iter().map(|r| r.uri.as_str())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        list_shown(SEL_TRACKS as usize, v);
    });
}

/// The single Spotify search type behind a filter ("" for All).
fn search_kind(filter: &str) -> &'static str {
    match filter {
        "tracks" => "track",
        "artists" => "artist",
        "albums" => "album",
        "playlists" => "playlist",
        _ => "",
    }
}

/// How well `name` matches the query: 0 exact · 1 prefix · 2 a word starts with it ·
/// 3 contains it · 4 no match. Case-insensitive.
fn search_match_rank(name: &str, q: &str) -> u8 {
    let n = name.to_lowercase();
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return 4;
    }
    if n == q {
        0
    } else if n.starts_with(&q) {
        1
    } else if n.match_indices(&q).any(|(i, _)| n[..i].ends_with(|c: char| !c.is_alphanumeric())) {
        2
    } else if n.contains(&q) {
        3
    } else {
        4
    }
}

fn search_track_row(r: &SearchResults, t: &Track) -> (SearchRow, SearchAction) {
    (
        SearchRow {
            name: t.name.clone().into(),
            sub: t.artists.clone().into(),
            kind: "track".into(),
            selected: false,
            art: r.track_thumbs.get(&t.id).cloned().unwrap_or_default().into(),
            meta: if t.duration_ms > 0 { fmt_time(t.duration_ms).into() } else { Default::default() },
        },
        SearchAction::Track { uri: t.uri.clone(), id: t.id.clone() },
    )
}

fn search_artist_row(a: &lightify_core::Artist) -> (SearchRow, SearchAction) {
    let sub = if a.followers > 0 {
        format!("Artist \u{2022} {} followers", compact_count(a.followers))
    } else {
        "Artist".to_string()
    };
    (
        SearchRow {
            name: a.name.clone().into(),
            sub: sub.into(),
            kind: "artist".into(),
            selected: false,
            art: a.thumb.clone().into(),
            meta: Default::default(),
        },
        SearchAction::Artist { id: a.id.clone(), name: a.name.clone() },
    )
}

fn search_album_row(a: &lightify_core::Album) -> (SearchRow, SearchAction) {
    (
        SearchRow {
            name: a.name.clone().into(),
            sub: format!("Album \u{2022} {}", a.artists).into(),
            kind: "album".into(),
            selected: false,
            art: a.thumb.clone().into(),
            meta: Default::default(),
        },
        SearchAction::Album { id: a.id.clone(), uri: a.uri.clone(), name: a.name.clone() },
    )
}

fn search_playlist_row(p: &lightify_core::Playlist) -> (SearchRow, SearchAction) {
    let by = if p.owner.is_empty() { String::new() } else { format!(" \u{2022} {}", p.owner) };
    (
        SearchRow {
            name: p.name.clone().into(),
            sub: format!("Playlist \u{2022} {} tracks{by}", group_thousands(p.tracks)).into(),
            kind: "playlist".into(),
            selected: false,
            art: p.image.clone().into(),
            meta: Default::default(),
        },
        SearchAction::Playlist { id: p.id.clone(), uri: p.uri.clone(), name: p.name.clone() },
    )
}

/// 1234 → "1,234", 45_600 → "45.6K", 2_300_000 → "2.3M".
fn compact_count(n: u32) -> String {
    match n {
        0..=9_999 => group_thousands(n),
        10_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0).replace(".0K", "K"),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0).replace(".0M", "M"),
    }
}

/// Collapse duplicate songs (the same title by the same artists — Spotify often
/// returns an explicit and a clean cut, or a single and its album copy) and
/// duplicate albums, keeping the first (Spotify's own relevance order).
fn dedupe_results(r: &SearchResults) -> (Vec<&Track>, Vec<&lightify_core::Album>) {
    let mut seen = std::collections::HashSet::new();
    let tracks = r
        .tracks
        .iter()
        .filter(|t| seen.insert((t.name.to_lowercase(), t.artists.to_lowercase())))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let albums = r
        .albums
        .iter()
        .filter(|a| seen.insert((a.name.to_lowercase(), a.artists.to_lowercase())))
        .collect();
    (tracks, albums)
}

/// How many of the top songs credit each artist — the only popularity signal this
/// client's search offers (see `build_search`).
fn artist_presence(r: &SearchResults, artist_id: &str) -> usize {
    r.tracks.iter().take(10).filter(|t| t.artist_ids.iter().any(|a| a == artist_id)).count()
}

/// Artist indices: names that actually match the query before Spotify's fuzzy extras
/// ("Robyn" for "toby"), then most-credited-on-the-top-songs, then Spotify's order.
fn artists_by_presence(r: &SearchResults, q: &str) -> Vec<usize> {
    let mut order: Vec<usize> = (0..r.artists.len()).collect();
    order.sort_by_key(|&i| {
        let a = &r.artists[i];
        (search_match_rank(&a.name, q) >= 4, std::cmp::Reverse(artist_presence(r, &a.id)), i)
    });
    order
}

/// The All view's Top result, as (kind, index into its list).
///
/// 1. An artist whose name is (or starts) the query and who is credited on the top
///    songs ("toby" → Toby Keith, "m83" → M83).
/// 2. Else a top-3 song whose title is (or starts with) the query ("midnight city"
///    → M83's song, not an unknown band that happens to be named that).
/// 3. Else an artist named exactly the query, then one matching at a word start,
///    then an album or playlist titled exactly the query.
/// 4. Else no Top result: Spotify pads even gibberish with fuzzy guesses, and a
///    "Top result" card that doesn't match the query at all would mislead.
fn pick_top_result(
    r: &SearchResults,
    tracks: &[&Track],
    albums: &[&lightify_core::Album],
    artist_order: &[usize],
    q: &str,
) -> Option<(&'static str, usize)> {
    let artist = |max_rank: u8, need_presence: bool| {
        artist_order.iter().copied().find(|&i| {
            let a = &r.artists[i];
            search_match_rank(&a.name, q) <= max_rank && (!need_presence || artist_presence(r, &a.id) > 0)
        })
    };
    if let Some(i) = artist(1, true) {
        return Some(("artist", i));
    }
    if let Some(i) = tracks.iter().take(3).position(|t| search_match_rank(&t.name, q) <= 1) {
        return Some(("track", i));
    }
    if let Some(i) = artist(0, false).or_else(|| artist(2, false)) {
        return Some(("artist", i));
    }
    if let Some(i) = albums.iter().take(3).position(|a| search_match_rank(&a.name, q) == 0) {
        return Some(("album", i));
    }
    r.playlists.iter().take(3).position(|p| search_match_rank(&p.name, q) == 0).map(|i| ("playlist", i))
}

/// Rows shown per section in the All view.
const SEARCH_SECTION_ROWS: usize = 4;

/// Build the search-result rows (+ parallel click actions) for a filter.
///
/// All: a **Top result** card, then Songs · Artists · Albums · Playlists sections of
/// up to four, each titled with a "See all" link to its filter. A single filter: the
/// full (paged) list of that type. Songs and albums are de-duplicated everywhere.
///
/// The Top result: this client's search carries no popularity or follower counts
/// (checked live with `--probe-search` — all null), so "who is meant" comes from
/// Spotify's own ordering plus a cross-type signal: an artist credited on several of
/// the top songs is the popular one ("toby" → Toby Keith, on most of the top songs,
/// over Toby Romeo, listed first but on none). See `pick_top_result`.
fn build_search(r: &SearchResults, filter: &str, q: &str) -> (Vec<SearchRow>, Vec<SearchAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    let (tracks, albums) = dedupe_results(r);
    let artist_order = artists_by_presence(r, q);
    let mut push = |(row, action): (SearchRow, SearchAction), rows: &mut Vec<SearchRow>| {
        rows.push(row);
        actions.push(action);
    };
    match filter {
        "tracks" => tracks.iter().for_each(|t| push(search_track_row(r, t), &mut rows)),
        "artists" => artist_order.iter().for_each(|&i| push(search_artist_row(&r.artists[i]), &mut rows)),
        "albums" => albums.iter().for_each(|a| push(search_album_row(a), &mut rows)),
        "playlists" => r.playlists.iter().for_each(|p| push(search_playlist_row(p), &mut rows)),
        _ => {
            let top = pick_top_result(r, &tracks, &albums, &artist_order, q);
            if let Some((kind, i)) = top {
                // The card draws its cover at 128px, so it takes the large image.
                let (mut row, action) = match kind {
                    "artist" => {
                        let a = &r.artists[i];
                        let (mut row, act) = search_artist_row(a);
                        row.art = if a.image.is_empty() { a.thumb.clone() } else { a.image.clone() }.into();
                        (row, act)
                    }
                    "track" => {
                        let t = tracks[i];
                        let (mut row, act) = search_track_row(r, t);
                        row.art = if t.album_art.is_empty() { row.art.to_string() } else { t.album_art.clone() }.into();
                        (row, act)
                    }
                    "album" => {
                        let a = albums[i];
                        let (mut row, act) = search_album_row(a);
                        row.art = if a.image.is_empty() { a.thumb.clone() } else { a.image.clone() }.into();
                        (row, act)
                    }
                    _ => search_playlist_row(&r.playlists[i]),
                };
                row.meta = row.kind.clone();
                row.kind = "top".into();
                // The chip names the type, so the line beside it is just the credit.
                row.sub = match kind {
                    "track" => tracks[i].artists.clone(),
                    "album" => albums[i].artists.clone(),
                    "playlist" => r.playlists[i].owner.clone(),
                    _ => String::new(),
                }
                .into();
                push((row, action), &mut rows);
            }
            let skip = |k: &str, i: usize| top == Some((k, i));
            let mut section = |title: &str, see_all: &str, items: Vec<(SearchRow, SearchAction)>, rows: &mut Vec<SearchRow>| {
                if items.is_empty() {
                    return;
                }
                rows.push(SearchRow {
                    name: title.into(),
                    sub: Default::default(),
                    kind: "header".into(),
                    selected: false,
                    art: Default::default(),
                    meta: "See all".into(),
                });
                actions.push(SearchAction::Header { see_all: see_all.to_string() });
                for (row, action) in items.into_iter().take(SEARCH_SECTION_ROWS) {
                    rows.push(row);
                    actions.push(action);
                }
            };
            let songs = tracks.iter().enumerate().filter(|(i, _)| !skip("track", *i)).map(|(_, t)| search_track_row(r, t)).collect();
            section("Songs", "tracks", songs, &mut rows);
            let artists = artist_order.iter().filter(|&&i| !skip("artist", i)).map(|&i| search_artist_row(&r.artists[i])).collect();
            section("Artists", "artists", artists, &mut rows);
            let albs = albums.iter().enumerate().filter(|(i, _)| !skip("album", *i)).map(|(_, a)| search_album_row(a)).collect();
            section("Albums", "albums", albs, &mut rows);
            let pls = r.playlists.iter().enumerate().filter(|(i, _)| !skip("playlist", *i)).map(|(_, p)| search_playlist_row(p)).collect();
            section("Playlists", "playlists", pls, &mut rows);
        }
    }
    (rows, actions)
}

// ── Recent searches ──────────────────────────────────────────────────────────

/// How many past queries are kept.
const RECENT_SEARCHES_MAX: usize = 8;

fn recent_searches_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_recent_searches.json")
}

fn load_recent_searches() -> Vec<String> {
    std::fs::read(recent_searches_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Put `q` at the front (case-insensitively unique), keeping the newest few.
fn remember_search(q: &str) {
    let q = q.trim();
    if q.is_empty() {
        return;
    }
    let mut list = load_recent_searches();
    list.retain(|x| !x.eq_ignore_ascii_case(q));
    list.insert(0, q.to_string());
    list.truncate(RECENT_SEARCHES_MAX);
    if let Ok(bytes) = serde_json::to_vec(&list) {
        let _ = lightify_core::config::write_atomic(&recent_searches_path(), &bytes);
    }
}

fn forget_recent_searches() {
    let _ = std::fs::remove_file(recent_searches_path());
}

/// The empty-box view: "Recent searches" (with a Clear link) and the queries.
fn build_recents(list: &[String]) -> (Vec<SearchRow>, Vec<SearchAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if list.is_empty() {
        return (rows, actions);
    }
    rows.push(SearchRow {
        name: "Recent searches".into(),
        sub: Default::default(),
        kind: "header".into(),
        selected: false,
        art: Default::default(),
        meta: "Clear".into(),
    });
    actions.push(SearchAction::ClearRecents);
    for q in list {
        rows.push(SearchRow {
            name: q.clone().into(),
            sub: Default::default(),
            kind: "recent".into(),
            selected: false,
            art: Default::default(),
            meta: Default::default(),
        });
        actions.push(SearchAction::Recent(q.clone()));
    }
    (rows, actions)
}

fn set_search_loading(weak: &slint::Weak<MainWindow>, on: bool) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_search_loading(on));
}

fn push_search(weak: &slint::Weak<MainWindow>, rows: Vec<SearchRow>) {
    let keys: Vec<String> = rows.iter().map(|r| format!("{}\u{1f}{}\u{1f}{}", r.kind, r.name, r.sub)).collect();
    let v = list_pushed(SEL_SEARCH as usize, list_identity(keys.iter().map(|k| k.as_str())));
    thumbs::request_search(weak, &rows);
    let _ = weak.upgrade_in_event_loop(move |app| {
        // A page loaded under a filter extends the list: append in place, so the
        // list keeps its scroll position instead of jumping back to the top.
        let cur = app.get_search_results();
        let art = app.get_search_art();
        let appendable = cur.row_count() > 0
            && rows.len() > cur.row_count()
            && art.row_count() == cur.row_count()
            && (0..cur.row_count()).all(|i| cur.row_data(i).as_ref() == rows.get(i));
        if let (true, Some(vm), Some(am)) = (
            appendable,
            cur.as_any().downcast_ref::<slint::VecModel<SearchRow>>(),
            art.as_any().downcast_ref::<slint::VecModel<slint::Image>>(),
        ) {
            for r in rows.into_iter().skip(vm.row_count()) {
                am.push(thumbs::search_art(&r));
                vm.push(r);
            }
        } else {
            let art: Vec<slint::Image> = rows.iter().map(thumbs::search_art).collect();
            app.set_search_art(slint::ModelRc::from(Rc::new(slint::VecModel::from(art))));
            app.set_search_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        }
        list_shown(SEL_SEARCH as usize, v);
    });
}

fn set_tab(weak: &slint::Weak<MainWindow>, tab: i32) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_tab(tab));
}

// ── Sidebar (queue / recent) marshaling ──────────────────────────────────────

fn current_id(last: &Option<PlaybackState>) -> String {
    last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.id.clone()).unwrap_or_default()
}

/// Sidebar toggle semantics: pressing the *active* mode's button closes it (→ 0);
/// pressing any other button opens that mode. Shared by the worker and the self-test.
fn next_sidebar_mode(current: i32, pressed: i32) -> i32 {
    if current == pressed {
        0
    } else {
        pressed
    }
}

/// Build sidebar rows, marking the row that matches the currently-playing track,
/// and (Recent only) attaching a relative timestamp from `added_at` (= played_at).
fn build_side_rows(tracks: &[Track], playing_id: &str, show_time: bool) -> Vec<SideRow> {
    tracks
        .iter()
        .map(|t| SideRow {
            name: t.name.clone().into(),
            artist: t.artists.clone().into(),
            time: if show_time {
                t.added_at.as_deref().map(rel_time).unwrap_or_default().into()
            } else {
                slint::SharedString::new()
            },
            playing: !playing_id.is_empty() && t.id == *playing_id,
            selected: false,
        })
        .collect()
}

/// Push the sidebar mode + rows to the UI. `empty` (when non-blank) sets the
/// no-rows placeholder text; a blank `empty` leaves the current text untouched.
fn set_sidebar(weak: &slint::Weak<MainWindow>, mode: i32, rows: Vec<SideRow>, empty: String) {
    // Identity is the track, not its relative "5m ago" time, which ticks on its own.
    let keys: Vec<String> = rows.iter().map(|r| format!("{mode}\u{1f}{}\u{1f}{}", r.name, r.artist)).collect();
    let v = list_pushed(SEL_SIDEBAR as usize, list_identity(keys.iter().map(|k| k.as_str())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        list_shown(SEL_SIDEBAR as usize, v);
        app.set_sidebar_mode(mode);
        if !empty.is_empty() {
            app.set_sidebar_empty(empty.into());
        }
        if let Some(m) = patch_model(&app.get_sidebar_rows(), rows) {
            app.set_sidebar_rows(m);
        }
    });
}

/// UI thread: bring a list model up to `rows` with the least churn (UI-PLAN C7).
/// Identical → nothing at all (no relayout, no redraw); same length → only the rows
/// that differ are rewritten in place (the list keeps its scroll position); otherwise
/// a fresh model is returned for the caller to install. The queue panel re-derives its
/// rows every 1.5 s while open, and replacing the whole model each time rebuilt and
/// repainted the list even when nothing had changed.
fn patch_model<T: Clone + PartialEq + 'static>(current: &slint::ModelRc<T>, rows: Vec<T>) -> Option<slint::ModelRc<T>> {
    let patchable = current.as_any().downcast_ref::<slint::VecModel<T>>().is_some();
    if patchable && current.row_count() == rows.len() {
        for (i, r) in rows.into_iter().enumerate() {
            if current.row_data(i).as_ref() != Some(&r) {
                current.set_row_data(i, r);
            }
        }
        return None;
    }
    Some(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))))
}

/// Keep an open sidebar current on each poll: the queue advances as tracks play
/// (re-fetch), while Recent is stable (just re-mark the now-playing row).
/// Keep the QUEUE/RECENT sidebar current — called both on open and, while it's
/// open, on every `queue_ticker` tick. Never blocks on the network itself: a
/// `me/player/queue` round trip must not hold up the command loop from handling
/// anything else meanwhile (a click, a search, Next — all of it), which is exactly
/// what made opening or refreshing the panel feel like it was "holding the whole
/// app back" (fixed 2026-09-17, see `spawn_sidebar_fetch`).
async fn refresh_sidebar(
    weak: &slint::Weak<MainWindow>,
    self_tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
    mode: i32,
    tracks: &mut Vec<Track>,
    last: &Option<PlaybackState>,
    mirror: &mut Option<QueueMirror>,
    generation: &mut u64,
) {
    let pid = current_id(last);
    match mode {
        1 => {
            // A list-driven play owns the ordering; show the mirror rather than
            // Spotify's context order, and skip the network entirely.
            let playing_uri = last
                .as_ref()
                .and_then(|p| p.track.as_ref())
                .map(|t| t.uri.clone())
                .unwrap_or_default();
            if let Some(m) = mirror.as_mut() {
                if m.advance(&playing_uri) {
                    *tracks = m.upcoming.clone();
                    persist_mirror(mirror);
                    set_sidebar(weak, 1, build_side_rows(tracks, &pid, false), "No queued tracks".to_string());
                    return;
                }
                *mirror = None;
                save_queue("", &[]);
            }
            // No mirror to fold forward locally — this is the common path for a
            // whole-playlist/album play or a station, which never built one. Ask
            // Spotify directly, in the background — see `Cmd::SidebarFetched` for
            // where the result (and the mirror backfill this used to do inline)
            // lands once it comes back.
            *generation += 1;
            set_sidebar(weak, 1, vec![], "Loading queue\u{2026}".to_string());
            spawn_sidebar_fetch(self_tx, 1, *generation);
        }
        2 => {
            set_sidebar(weak, 2, build_side_rows(tracks, &pid, true), "No recent tracks yet".to_string());
        }
        _ => {}
    }
}

/// Record tracks the user just queued in the mirror (`QueueMirror::enqueue`), so
/// the panel shows them where Spotify will actually play them. Only the queue paths
/// for "current track" and track-list rows used to do this — queueing from search,
/// a search drill-in, the sidebar, Beatport or a selection left the mirror (which
/// *is* the panel while it exists) silently out of date.
///
/// `pools` are the loaded lists to find each uri's `Track` in. If any queued uri
/// can't be found, the mirror can no longer be kept honest, so it's dropped and the
/// panel falls back to reading Spotify's own queue.
fn mirror_queued(
    mirror: &mut Option<QueueMirror>,
    generation: &mut u64,
    queued: &[String],
    pools: &[&[Track]],
) {
    let Some(m) = mirror.as_mut() else { return };
    let find = |uri: &str| pools.iter().flat_map(|p| p.iter()).find(|t| t.uri == uri).cloned();
    match queued.iter().map(|u| find(u)).collect::<Option<Vec<Track>>>() {
        Some(found) => found.into_iter().for_each(|t| m.enqueue(t)),
        None => {
            *mirror = None;
            *generation += 1;
        }
    }
    persist_mirror(mirror);
}

/// The QUEUE panel's background refresh (`queue_ticker`). Same sources as
/// `refresh_sidebar`'s mode 1, with two differences that matter at a 1.5 s cadence:
/// it touches the model (and the on-disk mirror) only when the mirror actually moved,
/// and a network refresh keeps the rows on screen instead of blanking them to
/// "Loading queue…" every tick — that flicker, plus a disk write every 1.5 s, was
/// the old behaviour.
fn tick_queue_sidebar(
    weak: &slint::Weak<MainWindow>,
    self_tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
    tracks: &mut Vec<Track>,
    last: &Option<PlaybackState>,
    mirror: &mut Option<QueueMirror>,
    generation: &mut u64,
) {
    let playing_uri = last
        .as_ref()
        .and_then(|p| p.track.as_ref())
        .map(|t| t.uri.clone())
        .unwrap_or_default();
    if let Some(m) = mirror.as_mut() {
        if m.advance(&playing_uri) {
            let moved = m.upcoming.len() != tracks.len()
                || m.upcoming.first().map(|t| &t.uri) != tracks.first().map(|t| &t.uri);
            if moved {
                *tracks = m.upcoming.clone();
                persist_mirror(mirror);
                let pid = current_id(last);
                set_sidebar(weak, 1, build_side_rows(tracks, &pid, false), "No queued tracks".to_string());
            }
            return;
        }
        *mirror = None;
        save_queue("", &[]);
    }
    *generation += 1;
    spawn_sidebar_fetch(self_tx, 1, *generation);
}

/// A local mirror of what plays next, in the order the user is *looking at*.
///
/// Ports the host's `queueSource === 'context'` (`setContextQueue`, `app.js:803`).
/// Playing a track from a list hands Spotify a real context (`me/player/play` with
/// `context_uri` + offset) so playback continues correctly — but Spotify then
/// reports the queue in the **playlist's own order**, which is not what the user
/// sees whenever the list is sorted (Recent / A-Z). Starting mid-list therefore
/// showed tracks from the top of the list as "up next". The host solves it by
/// keeping its own ordered mirror alongside the context play, and showing that; so
/// does this.
///
/// Bonus: the panel is then instant. No `me/player/queue` round trip, which is what
/// made queued tracks take seconds to appear.
struct QueueMirror {
    /// Upcoming tracks, display order, excluding the one playing.
    upcoming: Vec<Track>,
    /// The track this mirror was launched on, so the advance rule can tell
    /// "still on the launch track" from "moved somewhere else entirely".
    current_uri: String,
    /// How many of `upcoming`'s leading entries were queued by hand. Spotify plays
    /// user-queued tracks *before* the rest of the context, so an addition belongs
    /// after the earlier additions but ahead of the context remainder. Appending it
    /// to the end (the old behaviour) put it behind the whole playlist on screen —
    /// and the moment it started playing, `advance` drained every row above it, so
    /// the panel emptied.
    queued: usize,
}

impl QueueMirror {
    fn new(current_uri: String, upcoming: Vec<Track>) -> Self {
        Self { upcoming, current_uri, queued: 0 }
    }

    /// A track the user just queued: plays after earlier additions, before the
    /// context remainder.
    fn enqueue(&mut self, t: Track) {
        let at = self.queued.min(self.upcoming.len());
        self.upcoming.insert(at, t);
        self.queued = at + 1;
    }

    /// Fold the mirror forward to `playing_uri`.
    ///
    /// Returns false when playback has left the mirror altogether (a station, a
    /// Beatport chart, a search hit...), which is the signal to drop it and go back
    /// to Spotify's own queue. Self-healing on purpose: every other way of starting
    /// playback would otherwise need its own "and clear the mirror" line, and
    /// missing one would leave a stale list on screen.
    fn advance(&mut self, playing_uri: &str) -> bool {
        if playing_uri.is_empty() || playing_uri == self.current_uri {
            return true;
        }
        let Some(at) = self.upcoming.iter().position(|t| t.uri == playing_uri) else {
            return false;
        };
        self.upcoming.drain(..=at);
        self.queued = self.queued.saturating_sub(at + 1);
        self.current_uri = playing_uri.to_string();
        true
    }
}

// ── Downloads sidebar ───────────────────────────────────────────────────────

/// Map the bridge's queue onto the DOWNLOADS sidebar rows — ports
/// `renderSidebarDownloads` (`app.js:5910`).
fn build_dl_rows(items: &[downloader::Item]) -> Vec<DlRow> {
    items
        .iter()
        .map(|it| {
            let name = if it.name.is_empty() { "Download item".to_string() } else { it.name.clone() };
            let state = if it.failed() {
                2
            } else if it.finished() {
                1
            } else {
                0
            };
            DlRow {
                name: name.into(),
                sub: it.summary().into(),
                progress: it.progress.min(100) as i32,
                state,
            }
        })
        .collect()
}

fn set_dl_rows(
    weak: &slint::Weak<MainWindow>,
    rows: Vec<DlRow>,
    empty: String,
    clearable: bool,
) {
    let v = list_pushed(LIST_DOWNLOADS, list_identity(rows.iter().map(|r| r.name.as_str())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        list_shown(LIST_DOWNLOADS, v);
        if !empty.is_empty() {
            app.set_sidebar_empty(empty.into());
        }
        app.set_downloads_clearable(clearable);
        app.set_sidebar_dl_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    });
}

/// Fetch QUEUE (`mode` 1) or RECENT (`mode` 2) in the background and self-send the
/// result as `Cmd::SidebarFetched`, so opening or refreshing the sidebar never blocks
/// the command loop — the same reasoning, and the same "own independent `Session`"
/// approach, as `bp_refill_background`. `generation` is stamped on the result so a
/// stale reply (the panel since closed, switched modes, or was asked to refresh
/// again) can be told apart from the one that's actually still wanted.
static SIDEBAR_FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Sidebar fetches currently on the wire — the background refresh never stacks one
/// on top of another.
fn sidebar_fetches_in_flight() -> usize {
    SIDEBAR_FETCHES.load(std::sync::atomic::Ordering::SeqCst)
}

fn spawn_sidebar_fetch(self_tx: &tokio::sync::mpsc::UnboundedSender<Cmd>, mode: i32, generation: u64) {
    let tx = self_tx.clone();
    SIDEBAR_FETCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        struct InFlight;
        impl Drop for InFlight {
            fn drop(&mut self) {
                SIDEBAR_FETCHES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _in_flight = InFlight;
        let result = async {
            let mut session = Session::load()?;
            session.ensure_fresh().await?;
            if mode == 1 { session.queue().await } else { session.recently_played(50).await }
        }
        .await;
        let _ = tx.send(Cmd::SidebarFetched { generation, mode, result });
    });
}

/// Re-read the bridge's queue in the background and push it.
///
/// Background on purpose: the bridge's *first* call cold-starts a Python process
/// and can take seconds, and the worker's command loop must stay responsive
/// through that (the same reason `spawn_delayed_queue_refresh` exists). The
/// in-flight latch means the poll tick can fire this every interval without
/// stacking up requests when one is slow.
fn spawn_downloads_refresh(weak: &slint::Weak<MainWindow>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = weak.clone();
    tokio::spawn(async move {
        // Clear the latch on the way out whatever happens: if the task ever unwound
        // with the flag still set, downloads would silently stop refreshing for the
        // rest of the session.
        struct Latch;
        impl Drop for Latch {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, Ordering::SeqCst);
            }
        }
        let _latch = Latch;
        let result = downloader::snapshot().await;
        match result {
            Ok(items) => {
                // `updateSidebarClearButton`: Clear is live only when something is
                // actually in a finished state for `/api/clear_items` to drop.
                let clearable = items.iter().any(|i| i.finished());
                set_dl_rows(
                    &weak,
                    build_dl_rows(&items),
                    "No downloads queued".to_string(),
                    clearable,
                );
            }
            Err(e) => set_dl_rows(&weak, vec![], format!("Downloads \u{2014} {e}"), false),
        }
    });
}

/// Run one bridge call off the command loop, reporting either `ok` or the bridge's
/// own error in the status bar, then refresh the list if the sidebar is showing it.
fn spawn_download_action<F>(weak: &slint::Weak<MainWindow>, refresh: bool, ok: String, task: F)
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let weak = weak.clone();
    tokio::spawn(async move {
        match task.await {
            Ok(()) => set_status(&weak, ok),
            Err(e) => set_status(&weak, e),
        }
        if refresh {
            spawn_downloads_refresh(&weak);
        }
    });
}

/// Queue a download of one open.spotify.com URL. `label` is what the status line
/// names — ports `enqueueDownloadUrl` (`app.js:5272`), including its
/// "open the downloads sidebar afterwards" behaviour.
fn spawn_enqueue_download(
    weak: &slint::Weak<MainWindow>,
    self_tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
    url: String,
    label: String,
) {
    let weak = weak.clone();
    let tx = self_tx.clone();
    set_status(&weak, format!("Queueing download: {label}\u{2026}"));
    tokio::spawn(async move {
        match downloader::enqueue(&url).await {
            Ok(_) => {
                set_status(&weak, format!("Download queued: {label}"));
                let _ = tx.send(Cmd::RefreshDownloads);
            }
            Err(e) => set_status(&weak, e),
        }
        // `enqueue` starts a downloader sign-in itself when the account isn't ready
        // yet — same reclaim reasoning as `Cmd::DownloaderLogin` above applies here.
        let _ = tx.send(Cmd::ReclaimPlaybackAfterDownloaderLogin);
    });
}

/// Push the downloader's account state into the settings panel.
fn push_downloader_status(weak: &slint::Weak<MainWindow>, text: String, ready: bool, busy: bool) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_downloader_status(text.into());
        app.set_downloader_ready(ready);
        app.set_downloader_busy(busy);
    });
}

/// Ask the bridge how its own Spotify account is doing. Only ever called while the
/// settings panel is open, so an idle app never cold-starts the bridge.
fn spawn_downloader_status(weak: &slint::Weak<MainWindow>) {
    let weak = weak.clone();
    tokio::spawn(async move {
        match downloader::status().await {
            Ok(st) => push_downloader_status(&weak, st.summary(), st.spotify_ready, st.login_in_progress),
            Err(e) => push_downloader_status(&weak, e, false, false),
        }
    });
}

/// Resolve the next few chart rows and append them to the play queue.
///
/// Returns how many were queued. Bounded by `BP_REFILL_CHUNK` so one pass can never
/// stall the worker for long; the poll tick calls it again while the queue is short.
async fn bp_refill(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    seq: &mut BpAutoplay,
    bp_queued: &mut std::collections::HashSet<String>,
) -> usize {
    let mut queued = 0usize;
    while queued < BP_REFILL_CHUNK && seq.next_index < seq.source.len() {
        let bt = seq.source[seq.next_index].clone();
        seq.next_index += 1;
        let matched = match session.beatport_match(&bt.name, &bt.artists).await {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                // A rate limit here must not spin: stop this pass and let the next
                // tick retry, exactly like the original's refill error path bailing.
                set_status(weak, format!("Beatport queue refill \u{2014} {e}"));
                break;
            }
        };
        if !seq.seen.insert(matched.uri.clone()) {
            continue;
        }
        if let Err(e) = session.add_to_queue(&matched.uri).await {
            set_status(weak, format!("Beatport queue refill \u{2014} {e}"));
            break;
        }
        // Tracked so `end_autoplay_authority` can tell whether this chart's own
        // queued-ahead tracks are still sitting there once the user moves on to
        // something else (a library song, a search hit, ...) — see its doc comment.
        bp_queued.insert(matched.uri.clone());
        if !matched.id.is_empty() {
            seq.loaded_ids.push(matched.id);
        }
        queued += 1;
    }
    seq.completed = seq.next_index >= seq.source.len();
    queued
}

/// The recurring, poll-tick-driven half of keeping a Beatport chart queued ahead —
/// see `bp_refill`, which this otherwise matches move for move. Runs on its **own**,
/// independently-loaded `Session` rather than the worker's shared one, entirely
/// inside a `tokio::spawn`ed task, so a match+queue pass in flight never blocks the
/// command loop from handling anything else meanwhile (a sidebar toggle, a search,
/// Next — all of it). `bp_refill` itself stays synchronous and is left alone for
/// `Cmd::BpPlay`'s one-off *initial* fill, which only ever runs once per chart click
/// and whose "X queued, Y left to match" status line depends on knowing the count
/// immediately — converting that one too would need restructuring how that message
/// gets built; not worth it for a one-time, non-recurring cost.
///
/// A second, independent `Session` sharing only the on-disk token cache with the
/// main one is a deliberate, narrow choice over making `Session` itself `Clone`/
/// concurrent: every method this needs (`beatport_match`, `add_to_queue`,
/// `ensure_fresh`) is reused completely unchanged, at the cost of a redundant
/// `Session::load()` (a local file read, not a network call) per refill pass and a
/// vanishingly small chance of two independent token refreshes racing on that same
/// cache file — survivable either way, since the loser's write is simply overwritten
/// and the next request re-triggers a refresh if it actually needed one.
///
/// Returns `(queued, seen, next_index, completed, error)`: the `(uri, id)` pairs
/// actually queued (in order), the updated `seen` set and `next_index` to fold back
/// into the live `BpAutoplay`, whether the chart is now fully matched, and any error
/// to surface. The caller (`Cmd::BpRefillDone`) discards the whole result if the
/// sequence it was for has since been abandoned or replaced (`seq_id` mismatch) — but
/// that alone isn't enough: discarding the *result* doesn't undo an `add_to_queue`
/// this task already made against the real, shared Spotify queue. A chart abandoned
/// mid-refill (the user switches to a library song, starts a station, …) used to let
/// this task keep matching and queuing anyway, landing tracks nobody was tracking any
/// more — invisible to `end_autoplay_authority`'s leftover cleanup because they were
/// never folded into `bp_queued`, and indistinguishable on screen from whatever the
/// user switched to. `bp_active_seq` closes that: it names whichever sequence is
/// currently authoritative, updated *synchronously* by the worker loop (never through
/// a channel, which could arrive after the damage) the instant it moves on, so this
/// loop only has to check it before each further write.
async fn bp_refill_background(
    source: Vec<BeatportTrack>,
    mut next_index: usize,
    mut seen: std::collections::HashSet<String>,
    seq_id: u64,
    active_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> (Vec<(String, String)>, std::collections::HashSet<String>, usize, bool, Option<String>) {
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            let completed = next_index >= source.len();
            return (Vec::new(), seen, next_index, completed, Some(e));
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        let completed = next_index >= source.len();
        return (Vec::new(), seen, next_index, completed, Some(e));
    }

    let mut queued = Vec::new();
    let mut error = None;
    let mut count = 0usize;
    while count < BP_REFILL_CHUNK && next_index < source.len() {
        // Checked on every iteration, not just once at entry: the whole point is that
        // ownership can change *while this loop is mid-flight*, and each iteration
        // makes two more network calls' worth of room for exactly that.
        if active_seq.load(std::sync::atomic::Ordering::SeqCst) != seq_id {
            break;
        }
        let bt = source[next_index].clone();
        next_index += 1;
        let matched = match session.beatport_match(&bt.name, &bt.artists).await {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        if !seen.insert(matched.uri.clone()) {
            continue;
        }
        // Re-checked here too: the match call above just awaited a round trip, which is
        // exactly the window a `PlayTrack`/`StartStation`/etc. can land in.
        if active_seq.load(std::sync::atomic::Ordering::SeqCst) != seq_id {
            break;
        }
        if let Err(e) = session.add_to_queue(&matched.uri).await {
            error = Some(e);
            break;
        }
        queued.push((matched.uri, matched.id));
        count += 1;
    }
    let completed = next_index >= source.len();
    (queued, seen, next_index, completed, error)
}

/// How many queued-ahead tracks are still in front of what is playing.
///
/// `None` means the playing track isn't part of this sequence any more — the user
/// started something else, and the original drops the sequence on exactly that signal
/// (`pumpBeatportAutoplay`: `if (currentIndex < 0) { clearBeatportAutoplay(); }`).
fn bp_remaining_ahead(seq: &BpAutoplay, playing_id: &str) -> Option<usize> {
    if playing_id.is_empty() {
        return Some(usize::MAX); // nothing resolved yet — don't drop the sequence
    }
    let at = seq.loaded_ids.iter().position(|id| id == playing_id)?;
    Some(seq.loaded_ids.len() - at - 1)
}

/// Fire a delayed, self-sent `Cmd::RefreshQueueSidebar` after a batch queue write.
/// Non-blocking on purpose: doing this inline (`.await`ing a sleep in the same match
/// arm) would freeze the whole command loop — every other click, tab switch, etc. —
/// for the length of the wait, not just delay the queue read. Spawning it means the
/// wait happens in the background and only the eventual refresh comes back through
/// the normal channel. See `QUEUE_SETTLE`'s doc comment for why this wait exists at all.
fn spawn_delayed_queue_refresh(self_tx: &tokio::sync::mpsc::UnboundedSender<Cmd>) {
    let tx = self_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(QUEUE_SETTLE).await;
        let _ = tx.send(Cmd::RefreshQueueSidebar);
    });
}

/// A compact "just now / 5m / 2h / 3d" from an ISO-8601 `played_at` (assumed UTC).
fn rel_time(iso: &str) -> String {
    let Some(then) = parse_rfc3339_utc(iso) else { return String::new() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let d = now - then;
    if d < 45 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m", (d + 30) / 60)
    } else if d < 86400 {
        format!("{}h", d / 3600)
    } else {
        format!("{}d", d / 86400)
    }
}

/// Parse `YYYY-MM-DDTHH:MM:SS…` to Unix seconds (UTC), enough for Spotify's
/// `played_at` without pulling in chrono. `None` on any malformed field.
fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // days_from_civil (Howard Hinnant), valid across the Gregorian calendar.
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

// ── Beatport marshaling ──────────────────────────────────────────────────────

fn bp_chart_label(kind: &str) -> &'static str {
    match kind {
        "hype" => "Hype 100",
        "releases" => "Top 100 Releases",
        _ => "Top 100",
    }
}

fn build_bp_rows(tracks: &[BeatportTrack]) -> Vec<BpItem> {
    tracks
        .iter()
        .map(|t| BpItem {
            title: format!("{}. {}", t.rank, t.name).into(),
            sub: if t.label.is_empty() {
                t.artists.clone().into()
            } else {
                format!("{} \u{2022} {}", t.artists, t.label).into()
            },
            selected: false,
        })
        .collect()
}

fn push_bp_genres(weak: &slint::Weak<MainWindow>, genres: &[(String, String)]) {
    let labels: Vec<slint::SharedString> = genres.iter().map(|(n, _)| n.clone().into()).collect();
    let first = genres.first().map(|(n, _)| n.clone()).unwrap_or_default();
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_bp_genres(slint::ModelRc::from(Rc::new(slint::VecModel::from(labels))));
        app.set_bp_genre_label(first.into());
        app.set_bp_genre_idx(0);
    });
}

fn set_bp_genre_selected(weak: &slint::Weak<MainWindow>, idx: usize, label: String) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_bp_genre_idx(idx as i32);
        app.set_bp_genre_label(label.into());
    });
}

fn set_bp_kind(weak: &slint::Weak<MainWindow>, kind: String) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_bp_kind(kind.into()));
}

fn set_bp_status(weak: &slint::Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_bp_status(text.into()));
}

fn push_bp_rows(weak: &slint::Weak<MainWindow>, rows: Vec<BpItem>) {
    let keys: Vec<String> = rows.iter().map(|r| format!("{}\u{1f}{}", r.title, r.sub)).collect();
    let v = list_pushed(SEL_BEATPORT as usize, list_identity(keys.iter().map(|k| k.as_str())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_bp_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        list_shown(SEL_BEATPORT as usize, v);
    });
}

/// Scrape + display a Beatport chart for the current genre/kind selection.
async fn bp_fetch(
    weak: &slint::Weak<MainWindow>,
    session: &Session,
    genres: &[(String, String)],
    idx: usize,
    kind: &str,
    out: &mut Vec<BeatportTrack>,
) {
    let (label, slug) = match genres.get(idx) {
        Some((l, s)) => (l.clone(), s.clone()),
        None => return,
    };
    let chart = bp_chart_label(kind);
    push_bp_rows(weak, vec![]);
    set_bp_status(weak, format!("Fetching {chart} \u{2014} {label}\u{2026}"));
    match session.beatport_chart(&slug, kind).await {
        Ok(t) => {
            push_bp_rows(weak, build_bp_rows(&t));
            let n = t.len();
            *out = t;
            set_bp_status(weak, format!("{n} tracks \u{2014} {label} {chart}"));
        }
        Err(e) => {
            out.clear();
            push_bp_rows(weak, vec![]);
            set_bp_status(weak, e);
        }
    }
}

/// Open a URL in the user's default browser.
///
/// Through the shell's URL protocol handler, NOT `cmd /C start`: cmd treats `&` as
/// a command separator, so any URL with a query string (the Spotify sign-in URL has
/// five `&`s) was cut off at the first one — the browser opened a broken page and
/// cmd tried to run the rest as commands.
fn open_url(url: &str) -> bool {
    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .is_ok()
}

// ── Sign-in ───────────────────────────────────────────────────────────────────

/// How the sign-in status line is coloured (`.status`, `.status.error`, `.status.ok`).
#[derive(Clone, Copy, PartialEq)]
enum SignInTone {
    Info,
    Error,
    Ok,
}

fn push_signin(weak: &slint::Weak<MainWindow>, open: bool, status: String, tone: SignInTone, waiting: bool) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        // The sign-in page needs the full content area. Launch never starts in a mini
        // layout, but a sign-in revoked mid-session can land here while one is up.
        if open && app.get_mini_mode() != "off" {
            apply_mini_mode(&app, "off");
        }
        app.set_signin_open(open);
        app.set_signin_status(status.into());
        app.set_signin_error(tone == SignInTone::Error);
        app.set_signin_ok(tone == SignInTone::Ok);
        app.set_signin_waiting(waiting);
    });
}

/// Give up on a pending browser sign-in after this long (the shipped app's 300 s).
const SIGNIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Return a signed-in, token-fresh `Session`, showing the sign-in page for as long
/// as it takes. `None` only when the app is closing.
async fn obtain_session(
    weak: &slint::Weak<MainWindow>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
) -> Option<Session> {
    let mut notice: Option<(String, SignInTone)> = None;
    loop {
        if let Ok(mut s) = Session::load() {
            match s.ensure_fresh().await {
                Ok(()) => {
                    push_signin(weak, false, String::new(), SignInTone::Info, false);
                    return Some(s);
                }
                Err(e) => {
                    notice = Some((
                        format!("Your Spotify sign-in has expired or was revoked \u{2014} authorise again. ({e})"),
                        SignInTone::Error,
                    ));
                }
            }
        }
        let saved = lightify_core::auth::saved_client_id();
        let _ = weak.upgrade_in_event_loop(move |app| {
            if app.get_signin_client_id().is_empty() {
                app.set_signin_client_id(saved.into());
            }
        });
        let (msg, tone) = notice.take().unwrap_or((String::new(), SignInTone::Info));
        push_signin(weak, true, msg, tone, false);
        set_status(weak, "Sign in to Spotify to start".to_string());

        // Wait for the user to authorise. Everything else is ignored until then —
        // there's nothing it could act on without a session.
        loop {
            let raw = match rx.recv().await? {
                Cmd::SignInAuthorise(cid) | Cmd::SignInRetry(cid) => cid,
                Cmd::SignInOpenDashboard => {
                    open_url(lightify_core::auth::DASHBOARD_URL);
                    continue;
                }
                _ => continue,
            };
            match sign_in(weak, rx, &raw).await {
                Ok(Some(name)) => {
                    push_signin(weak, true, format!("Signed in as {name}"), SignInTone::Ok, false);
                    break; // back to the top: load the session that was just saved
                }
                Ok(None) => return None, // closing
                Err(e) => push_signin(weak, true, e, SignInTone::Error, false),
            }
        }
    }
}

/// One browser sign-in. `Ok(None)` means the app is closing. While it waits, Retry
/// re-opens the browser on the same pending request (the shipped app's
/// `cmd_retry_auth_browser`), and the dashboard link still works.
async fn sign_in(
    weak: &slint::Weak<MainWindow>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    raw: &str,
) -> Result<Option<String>, String> {
    use lightify_core::auth;
    let cid = auth::normalize_client_id(raw)?;
    let callback = auth::bind_callback().await?;
    let pkce = auth::PkceAuth::new(&cid);
    let url = pkce.auth_url();
    if !open_url(&url) {
        return Err("Couldn't open your web browser to sign in.".to_string());
    }
    push_signin(weak, true, "Opening browser \u{2014} complete the login\u{2026}".to_string(), SignInTone::Info, true);
    let finish = pkce.finish(callback);
    tokio::pin!(finish);
    let deadline = tokio::time::sleep(SIGNIN_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            r = &mut finish => return r.map(Some),
            _ = &mut deadline => {
                return Err("Timed out waiting for Spotify \u{2014} click Authorise to try again.".to_string());
            }
            cmd = rx.recv() => match cmd {
                None => return Ok(None),
                Some(Cmd::SignInRetry(_)) => {
                    open_url(&url);
                    push_signin(weak, true, "Re-opening browser \u{2014} complete the login\u{2026}".to_string(), SignInTone::Info, true);
                }
                Some(Cmd::SignInOpenDashboard) => {
                    open_url(auth::DASHBOARD_URL);
                }
                Some(_) => {}
            },
        }
    }
}

// ── Mini-player ───────────────────────────────────────────────────────────────

/// Target window size (logical px) for each mini mode — mirrors the shipped
/// MINI_MODE_SIZES (square 360×440 · bar 480×136 · nano 240×270).
fn mini_size(mode: &str) -> (f64, f64) {
    match mode {
        "bar" => (480.0, 136.0),
        "nano" => (240.0, 270.0),
        _ => (360.0, 440.0), // square (also the default/fallback)
    }
}

/// Enter / switch / exit a mini-player mode. Mirrors the shipped `setMiniMode`:
/// relax the min-size first, then **unmaximize → setMinSize(target) → setSize(target)**
/// (the 2.0.9 fix order — a maximized window ignores resize and the targets are below
/// the full min), toggle always-on-top, capture the pre-mini size once on entry, and
/// restore it on exit. Runs on the UI thread (from the `set-mini-mode` callback).
fn apply_mini_mode(app: &MainWindow, mode: &str) {
    use slint::winit_030::winit::dpi::LogicalSize;
    use slint::winit_030::winit::window::WindowLevel;
    use slint::winit_030::WinitWindowAccessor;

    let was_off = app.get_mini_mode() == "off";
    // Set the property first so the bound min-width/min-height relax before we resize.
    app.set_mini_mode(mode.into());
    if mode != "off" {
        app.set_mini_last(mode.into());
        save_mini_mode(mode);
    }

    app.window().with_winit_window(|w| {
        use slint::winit_030::winit::dpi::LogicalPosition;

        if mode == "off" {
            // Drop always-on-top and the mini min-size FIRST: a window whose minimum is
            // still the mini size silently clamps the restore, which is half of what
            // "the window comes back wrong" was.
            let _ = w.set_window_level(WindowLevel::Normal);
            w.set_min_inner_size(Some(LogicalSize::new(640.0, 460.0)));

            // A window that was maximized before going mini comes back maximized;
            // restoring its *pixel* size instead would produce a merely-large window
            // that no longer fills the screen and is not in the maximized state.
            if app.get_mini_prev_max() {
                w.set_maximized(true);
            } else {
                let (rw, rh) = {
                    let pw = app.get_mini_prev_w() as f64;
                    let ph = app.get_mini_prev_h() as f64;
                    if pw >= 640.0 && ph >= 460.0 { (pw, ph) } else { (980.0, 660.0) }
                };
                let _ = w.request_inner_size(LogicalSize::new(rw, rh));
                // ...and put it back where it was. The mini player is draggable and
                // always-on-top, so by the time it is dismissed it is usually nowhere
                // near where the full window started; restoring only the size left the
                // big window parked at the mini player's corner, which is the "spawns
                // in a weird location" report. Position goes AFTER the resize so the
                // window manager doesn't re-clamp it against the old geometry.
                if app.get_mini_prev_pos_valid() {
                    w.set_outer_position(LogicalPosition::new(
                        app.get_mini_prev_x() as f64,
                        app.get_mini_prev_y() as f64,
                    ));
                }
            }
        } else {
            // Capture the pre-mini geometry once, only when entering from full.
            if was_off {
                let sf = w.scale_factor();
                let maximized = w.is_maximized();
                app.set_mini_prev_max(maximized);
                if !maximized {
                    let sz = w.inner_size();
                    app.set_mini_prev_w((sz.width as f64 / sf) as f32);
                    app.set_mini_prev_h((sz.height as f64 / sf) as f32);
                    match w.outer_position() {
                        Ok(pos) => {
                            app.set_mini_prev_x((pos.x as f64 / sf) as f32);
                            app.set_mini_prev_y((pos.y as f64 / sf) as f32);
                            app.set_mini_prev_pos_valid(true);
                        }
                        // Wayland and friends refuse to report a position; fall back to
                        // "restore the size only" rather than guessing at coordinates.
                        Err(_) => app.set_mini_prev_pos_valid(false),
                    }
                }
            }
            let (mw, mh) = mini_size(mode);
            w.set_maximized(false);
            w.set_min_inner_size(Some(LogicalSize::new(mw, mh)));
            let _ = w.request_inner_size(LogicalSize::new(mw, mh));
            let _ = w.set_window_level(WindowLevel::AlwaysOnTop);
        }
    });
}

/// On-disk store of the last active mini layout (the shell analogue of the shipped
/// app's localStorage `lightify.miniMode.v1`). Lives in the shared data dir.
fn mini_mode_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_mini.txt")
}

fn save_mini_mode(mode: &str) {
    let _ = std::fs::write(mini_mode_path(), mode);
}

/// Read the persisted mini layout, validated; defaults to "square" (matches the shipped
/// `lastMiniMode()`). Never returns "off" — the toggle recalls the last *active* layout.
fn load_mini_mode() -> String {
    match std::fs::read_to_string(mini_mode_path()) {
        Ok(s) => {
            let s = s.trim();
            if s == "square" || s == "bar" || s == "nano" {
                s.to_string()
            } else {
                "square".to_string()
            }
        }
        Err(_) => "square".to_string(),
    }
}

// ── Queue persistence ─────────────────────────────────────────────────────────
//
// Spotify Connect's own "up next" queue does not outlive the device connection
// that built it, and every launch of the shell IS a brand-new connection: librespot
// mints a fresh random device id every time (`SessionConfig::default()`, verified
// in the vendored `librespot-core` source — see `engine_owns_playback`'s doc
// comment), so Spotify has no way to recognise a relaunch as "the same device
// reconnecting" and hand its queue back. The original never persisted a queue
// either (no such `localStorage` key) — this is new, not a restored feature.
//
// What's saved is just the ordered track uris (not full `Track` metadata): on
// restore, each is re-added with `add_to_queue`, then a single `queue()` read
// gets back real `Track`s for display — cheaper to persist, and it means the
// restored list reflects whatever Spotify actually accepted, not a stale guess.

/// On-disk snapshot of "what's queued next", the shell analogue of the mini-mode
/// file above.
fn queue_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_queue.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedQueue {
    /// The track this queue was captured behind — restored only if we're still on
    /// it (or have advanced somewhere further into `upcoming`) next launch.
    current_uri: String,
    /// Upcoming track uris, in display order, as of the last time it was captured.
    upcoming: Vec<String>,
}

/// The most queue entries ever persisted, restored, or rebuilt one POST at a time.
///
/// A list-play mirror holds *everything below the clicked row* — thousands of tracks
/// after scrolling a big playlist — and restoring it replayed one `me/player/queue`
/// POST per track, inline, at startup: the worker stalled and the shared dev-mode
/// quota went straight to 429. Twenty is what Spotify's own `me/player/queue` read
/// reports back, so nothing visible is lost that the service would have shown.
const QUEUE_KEEP_MAX: usize = 20;

/// Save the queue, or clear the file when there's nothing worth remembering.
fn save_queue(current_uri: &str, upcoming: &[String]) {
    if current_uri.is_empty() || upcoming.is_empty() {
        let _ = std::fs::remove_file(queue_path());
        return;
    }
    let keep = &upcoming[..upcoming.len().min(QUEUE_KEEP_MAX)];
    let pq = PersistedQueue { current_uri: current_uri.to_string(), upcoming: keep.to_vec() };
    if let Ok(json) = serde_json::to_string(&pq) {
        let _ = lightify_core::config::write_atomic(&queue_path(), json.as_bytes());
    }
}

/// `save_queue`, but reading straight from the live mirror — every call site below
/// looks like this, so this is what they actually call.
fn persist_mirror(mirror: &Option<QueueMirror>) {
    match mirror {
        Some(m) if !m.upcoming.is_empty() => {
            save_queue(&m.current_uri, &m.upcoming.iter().map(|t| t.uri.clone()).collect::<Vec<_>>());
        }
        _ => save_queue("", &[]),
    }
}

fn load_queue() -> Option<PersistedQueue> {
    serde_json::from_str(&std::fs::read_to_string(queue_path()).ok()?).ok()
}

/// Where in a persisted queue does `playing_uri` put us? `Some(&[])` is a valid
/// answer (we're on the very last persisted track — nothing left to restore).
/// `None` means playback has moved somewhere the persisted list never covered
/// (another device since, or simply nothing left to resume) — restoring nothing is
/// the right call rather than re-queuing a stale batch behind an unrelated song.
///
/// Pulled out of `restore_queue` so the matching logic — the part worth getting
/// exactly right — is testable without a `Session`.
fn queue_restore_point<'a>(pq: &'a PersistedQueue, playing_uri: &str) -> Option<&'a [String]> {
    if playing_uri.is_empty() {
        return None;
    }
    if playing_uri == pq.current_uri {
        Some(&pq.upcoming)
    } else {
        pq.upcoming.iter().position(|u| u == playing_uri).map(|at| &pq.upcoming[at + 1..])
    }
}

/// Re-queue whatever was queued the last time the app closed, once our own device
/// is up and we know what's actually playing. Only restores behind the exact track
/// it was captured on, or one further into the same list (see `queue_restore_point`).
/// Best-effort: never surfaces an error, since a failed restore just means the
/// queue stays empty, exactly as it would have without this feature.
async fn restore_queue(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    last: &Option<PlaybackState>,
    mirror: &mut Option<QueueMirror>,
) {
    let Some(pq) = load_queue() else { return };
    let playing_uri = last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default();
    let Some(remaining) = queue_restore_point(&pq, &playing_uri) else { return };
    if remaining.is_empty() {
        return;
    }
    let mut queued = 0usize;
    for uri in remaining.iter().take(QUEUE_KEEP_MAX) {
        match session.add_to_queue(uri).await {
            Ok(()) => queued += 1,
            Err(_) => break,
        }
    }
    if queued == 0 {
        return;
    }
    if let Ok(t) = session.queue().await {
        if !t.is_empty() {
            set_status(weak, format!("Restored queue \u{2014} {} track{}", t.len(), if t.len() == 1 { "" } else { "s" }));
            *mirror = Some(QueueMirror::new(playing_uri, t));
        }
    }
}

// ── Settings marshaling ──────────────────────────────────────────────────────

fn set_account(weak: &slint::Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_account(text.into()));
}

fn set_device_status(weak: &slint::Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_settings_device_status(text.into()));
}

fn build_device_rows(devices: &[Device]) -> Vec<DeviceRow> {
    devices
        .iter()
        .map(|d| DeviceRow {
            name: if d.kind.is_empty() {
                d.name.clone().into()
            } else {
                format!("{} \u{00b7} {}", d.name, d.kind).into()
            },
            active: d.is_active,
        })
        .collect()
}

fn push_devices(weak: &slint::Weak<MainWindow>, rows: Vec<DeviceRow>) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_settings_devices(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    });
}

// ── Command palette ──────────────────────────────────────────────────────────

/// One palette command: what's shown + the worker command to run.
struct PaletteCmd {
    title: String,
    hint: String,
    keywords: String,
    cmd: Cmd,
}

/// The static + playlist command list (everything except the query-dependent
/// "Search for …" row, which is prepended on query). Mirrors buildCommandPaletteBase.
fn build_palette_base(
    playlists: &[lightify_core::Playlist],
    order: &[usize],
    playing: bool,
) -> Vec<PaletteCmd> {
    let mut v = vec![
        PaletteCmd { title: if playing { "Pause" } else { "Play" }.into(), hint: "Playback".into(), keywords: "toggle playback resume".into(), cmd: Cmd::TogglePlay },
        PaletteCmd { title: "Next track".into(), hint: "Playback".into(), keywords: "skip forward".into(), cmd: Cmd::Next },
        PaletteCmd { title: "Previous track".into(), hint: "Playback".into(), keywords: "back rewind".into(), cmd: Cmd::Prev },
        PaletteCmd { title: "Toggle shuffle".into(), hint: "Playback".into(), keywords: "random".into(), cmd: Cmd::ToggleShuffle },
        PaletteCmd { title: "Cycle repeat".into(), hint: "Playback".into(), keywords: "loop".into(), cmd: Cmd::CycleRepeat },
        PaletteCmd { title: "Open settings".into(), hint: "App".into(), keywords: "preferences config devices poll rate".into(), cmd: Cmd::OpenSettings },
        PaletteCmd { title: "Show queue".into(), hint: "Panel".into(), keywords: "up next sidebar".into(), cmd: Cmd::ToggleSidebar(1) },
        PaletteCmd { title: "Show recent".into(), hint: "Panel".into(), keywords: "history sidebar".into(), cmd: Cmd::ToggleSidebar(2) },
        PaletteCmd { title: "Collapse left panel".into(), hint: "Window".into(), keywords: "hide library toggle".into(), cmd: Cmd::CollapseLeft },
        PaletteCmd { title: "Mini player: square".into(), hint: "Window".into(), keywords: "compact miniplayer cover".into(), cmd: Cmd::SetMiniMode("square".into()) },
        PaletteCmd { title: "Mini player: bar".into(), hint: "Window".into(), keywords: "compact miniplayer strip".into(), cmd: Cmd::SetMiniMode("bar".into()) },
        PaletteCmd { title: "Mini player: nano (cover only)".into(), hint: "Window".into(), keywords: "compact miniplayer cover tiny".into(), cmd: Cmd::SetMiniMode("nano".into()) },
        PaletteCmd { title: "Exit mini player".into(), hint: "Window".into(), keywords: "full restore miniplayer off".into(), cmd: Cmd::SetMiniMode("off".into()) },
        PaletteCmd { title: "Liked Songs".into(), hint: "Library".into(), keywords: "favorites hearts saved".into(), cmd: Cmd::Drill(0) },
    ];
    // Playlists map to Drill(row+1) — Drill takes the *display* row index (0 = Liked),
    // so walk the same sorted order the library list is showing.
    for (row, &idx) in order.iter().enumerate() {
        let Some(p) = playlists.get(idx) else { continue };
        v.push(PaletteCmd {
            title: p.name.clone(),
            hint: "Playlist".into(),
            keywords: format!("playlist {}", p.owner),
            cmd: Cmd::Drill(row + 1),
        });
    }
    v
}

/// True if every char of `q` appears in `hay` in order (classic fuzzy match).
fn is_subsequence(q: &str, hay: &str) -> bool {
    let mut hay_chars = hay.chars();
    'outer: for qc in q.chars() {
        for hc in hay_chars.by_ref() {
            if hc == qc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Score a command against a lowercased query. Higher = better; 0 = no match.
/// Ranking mirrors the host: exact title > prefix > substring > keyword > subsequence.
fn command_match_score(q: &str, title: &str, hay: &str) -> i32 {
    if q.is_empty() {
        return 1;
    }
    if title == q {
        return 1000;
    }
    if title.starts_with(q) {
        return 800 - title.len() as i32;
    }
    if let Some(i) = title.find(q) {
        return 600 - i as i32;
    }
    if let Some(i) = hay.find(q) {
        return 400 - i as i32;
    }
    if is_subsequence(q, hay) {
        200
    } else {
        0
    }
}

/// Filter + rank a command list against a query. Empty query keeps order (stable).
fn filter_commands(query: &str, cmds: Vec<PaletteCmd>) -> Vec<PaletteCmd> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return cmds;
    }
    let mut scored: Vec<(i32, usize, PaletteCmd)> = cmds
        .into_iter()
        .enumerate()
        .filter_map(|(idx, c)| {
            let title = c.title.to_lowercase();
            let hay = format!("{} {}", title, c.keywords.to_lowercase());
            let s = command_match_score(&q, &title, &hay);
            if s > 0 {
                Some((s, idx, c))
            } else {
                None
            }
        })
        .collect();
    // Score desc, then original index asc (stable — equal scores keep source order).
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, c)| c).collect()
}

/// Filter the palette, push the visible rows + reset selection, and return the
/// parallel command list to run on Enter/click.
fn apply_palette_filter(
    weak: &slint::Weak<MainWindow>,
    query: &str,
    playlists: &[lightify_core::Playlist],
    order: &[usize],
    playing: bool,
) -> Vec<Cmd> {
    // The query-dependent "Search for …" row is prepended to the base list.
    let mut source = build_palette_base(playlists, order, playing);
    let q = query.trim();
    if !q.is_empty() {
        source.insert(
            0,
            PaletteCmd {
                title: format!("Search for \u{201c}{q}\u{201d}"),
                hint: "Search".into(),
                keywords: format!("find spotify {q}"),
                cmd: Cmd::PaletteSearch(q.to_string()),
            },
        );
    }
    let filtered = filter_commands(query, source);
    let rows: Vec<CmdRow> = filtered
        .iter()
        .map(|c| CmdRow { title: c.title.clone().into(), hint: c.hint.clone().into() })
        .collect();
    let actions: Vec<Cmd> = filtered.into_iter().map(|c| c.cmd).collect();
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_cmdk_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        app.set_cmdk_selected(0);
    });
    actions
}

// ── Infinite-scroll pager for the drilled track list ─────────────────────────
//
// Ports `startPagedTrackList` / `loadNextTrackPage`. The shipped app pages the same
// way; the shell previously loaded one capped batch instead (200 liked / 500
// playlist tracks), which silently truncated large libraries.

#[derive(Clone, PartialEq)]
enum PagerSource {
    None,
    Liked,
    Playlist(String),
}

struct TrackPager {
    source: PagerSource,
    name: String,
    /// Next offset to request.
    offset: u32,
    /// Spotify's reported item count (0 when the endpoint doesn't report one).
    total: u32,
    /// No more pages: a short page arrived, or `total` is reached.
    done: bool,
    /// A page is in flight — mirrors the original's `pager.loading` guard.
    loading: bool,
    /// The rows came from the public embed page, not the Web API, so the list stops
    /// at the ~100 tracks the embed exposes.
    partial: bool,
}

impl Default for TrackPager {
    fn default() -> Self {
        Self {
            source: PagerSource::None,
            name: String::new(),
            offset: 0,
            total: 0,
            done: true,
            loading: false,
            partial: false,
        }
    }
}

impl TrackPager {
    fn start(source: PagerSource, name: &str) -> Self {
        Self {
            source,
            name: name.to_string(),
            offset: 0,
            total: 0,
            done: false,
            loading: false,
            partial: false,
        }
    }

    fn page_size(&self) -> u32 {
        match self.source {
            PagerSource::Liked => Session::LIKED_PAGE,
            _ => Session::PLAYLIST_PAGE,
        }
    }
}

/// Re-arm the playlist-hit pager after the results list is rebuilt. Only the
/// PLAYLISTS filter pages (`searchResultsPagerIsActive` in the original), and a first
/// page shorter than the page size means there is nothing more to fetch.
fn reset_playlist_pager(
    weak: &slint::Weak<MainWindow>,
    filter: &str,
    rows: &[SearchRow],
    offset: &mut u32,
    done: &mut bool,
) {
    let kind = search_kind(filter);
    let shown = rows.iter().filter(|r| !kind.is_empty() && r.kind == kind).count() as u32;
    // The initial search already consumed one page's worth of *raw* items, so the
    // next offset is one page on — not the number that happened to parse (Spotify
    // puts `null`s in playlist search results, so those differ). The All view never
    // pages; each single-type filter does.
    *offset = Session::SEARCH_PAGE;
    *done = kind.is_empty() || shown == 0;
    let more = !*done;
    let _ = weak.upgrade_in_event_loop(move |app| app.set_search_more(more));
}

/// Show / hide the search tab's own drill-in (`#search-drill` + `#btn-search-back`).
fn set_search_drill(weak: &slint::Weak<MainWindow>, on: bool, title: String) {
    let v = (!on).then(|| list_pushed(SEL_SEARCH_DRILL as usize, list_identity(std::iter::empty())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        if let Some(v) = v {
            list_shown(SEL_SEARCH_DRILL as usize, v);
        }
        app.set_search_drilled(on);
        app.set_search_drill_title(title.into());
        if !on {
            app.set_search_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(
                Vec::<Trk>::new(),
            ))));
            app.set_search_tracks_more(false);
        }
    });
}

/// Return the search tab from a drilled playlist/album/artist to the flat results
/// list. Used by the explicit Back button, but ALSO by typing a new query or
/// picking a different filter chip — the original always does this unconditionally
/// (`doSearch`: `hide(searchDrill); show(searchResults)`; the filter buttons call
/// `renderSearchResults`, which starts with `collapseSearchExpansion()`). Without
/// it here, searching or changing the filter while looking at a drilled result kept
/// showing that stale drilled content on screen — the underlying results list *did*
/// refresh, invisibly, until the user hit Back and searched again, which read as
/// "changing the filter mid-playlist does nothing."
// ── List versions: the index-drift guard ─────────────────────────────────────
//
// Row clicks and menu actions reach the worker as *display indices*. When the list
// underneath is replaced or reordered between the click and the worker handling it —
// search results landing while typing, the queue panel advancing a track, a Beatport
// chart swapping in, a menu left open while a page loads — the index silently
// resolves to a different row, and the action lands on something the user never
// clicked. Each list surface (the `open-context` kinds / `SEL_*` scopes, 0 library …
// 7 downloads) now has a version the worker bumps whenever the *identity* of its
// rows changes (not on cosmetic re-pushes such as playing marks or relative times).
// The UI records which version it is showing, clicks carry it, and the worker drops
// a click made against an older version instead of acting on the wrong row.

const LIST_SURFACES: usize = 8;

/// Worker side: the version of each surface's rows as last pushed.
static LIST_VERSION: [std::sync::atomic::AtomicU64; LIST_SURFACES] =
    [const { std::sync::atomic::AtomicU64::new(0) }; LIST_SURFACES];
/// UI side: the version actually on screen (set when the pushed model is applied).
static LIST_SHOWN: [std::sync::atomic::AtomicU64; LIST_SURFACES] =
    [const { std::sync::atomic::AtomicU64::new(0) }; LIST_SURFACES];
/// Identity hash of each surface's last push, so a cosmetic re-push isn't a change.
static LIST_IDENT: [std::sync::atomic::AtomicU64; LIST_SURFACES] =
    [const { std::sync::atomic::AtomicU64::new(0) }; LIST_SURFACES];

/// Hash the identity keys of a list's rows, in display order.
fn list_identity<'a>(keys: impl Iterator<Item = &'a str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut n = 0usize;
    for k in keys {
        k.hash(&mut h);
        n += 1;
    }
    n.hash(&mut h);
    h.finish()
}

/// Worker: about to push rows with identity `ident` to `surface`. Returns the version
/// the UI should record once it shows them.
fn list_pushed(surface: usize, ident: u64) -> u64 {
    use std::sync::atomic::Ordering::SeqCst;
    let s = surface.min(LIST_SURFACES - 1);
    if LIST_IDENT[s].swap(ident, SeqCst) != ident {
        LIST_VERSION[s].fetch_add(1, SeqCst) + 1
    } else {
        LIST_VERSION[s].load(SeqCst)
    }
}

/// UI thread: the rows for `version` are now on screen.
fn list_shown(surface: usize, version: u64) {
    LIST_SHOWN[surface.min(LIST_SURFACES - 1)].store(version, std::sync::atomic::Ordering::SeqCst);
}

fn list_version(surface: usize) -> u64 {
    LIST_VERSION[surface.min(LIST_SURFACES - 1)].load(std::sync::atomic::Ordering::SeqCst)
}

/// UI thread: stamp a click with the version of the list the user is looking at.
fn stamp_shown(surface: usize, cmd: Cmd) -> Cmd {
    let version = LIST_SHOWN[surface.min(LIST_SURFACES - 1)].load(std::sync::atomic::Ordering::SeqCst);
    Cmd::Stamped { surface, version, inner: Box::new(cmd) }
}

/// Worker: stamp a menu action with the version the menu was built from.
fn stamp_current(surface: usize, cmd: Cmd) -> Cmd {
    Cmd::Stamped { surface, version: list_version(surface), inner: Box::new(cmd) }
}

/// Worker: open a stamped command — the command itself if its list is unchanged,
/// otherwise `StaleClick`. Unstamped commands pass straight through.
fn unstamp(cmd: Cmd) -> Cmd {
    match cmd {
        Cmd::Stamped { surface, version, inner } => {
            if list_version(surface) == version { unstamp(*inner) } else { Cmd::StaleClick }
        }
        c => c,
    }
}

/// Forget the selection if it belongs to `scope` — called wherever that surface's
/// rows are replaced wholesale (a new search, a filter change, a new Beatport chart).
/// The selection is held as row indices, so once the rows underneath change it
/// silently points at different tracks: "Play 3 selected" then played rows the user
/// never picked.
fn drop_selection(
    weak: &slint::Weak<MainWindow>,
    scope: i32,
    selected: &mut std::collections::BTreeSet<usize>,
    sel_anchor: &mut Option<usize>,
    sel_scope: &mut i32,
) {
    if *sel_scope != scope {
        return;
    }
    selected.clear();
    *sel_anchor = None;
    *sel_scope = SEL_NONE;
    push_selection(weak, scope, selected, &[]);
}

fn exit_search_drill(
    weak: &slint::Weak<MainWindow>,
    selected: &mut std::collections::BTreeSet<usize>,
    sel_anchor: &mut Option<usize>,
    sel_scope: &mut i32,
    s_tracks: &mut Vec<Track>,
    s_context: &mut Option<String>,
    s_pager: &mut TrackPager,
) {
    if *sel_scope == SEL_SEARCH_DRILL {
        selected.clear();
        *sel_anchor = None;
        *sel_scope = SEL_NONE;
    }
    s_tracks.clear();
    *s_context = None;
    *s_pager = TrackPager::default();
    set_search_drill(weak, false, String::new());
}

/// Push the search drill-in's rows. It has no sort toolbar (neither does the
/// original's `#search-drill`), so the display order is the API order.
fn push_search_tracks(weak: &slint::Weak<MainWindow>, tracks: &[Track]) {
    let rows: Vec<Trk> = tracks
        .iter()
        .map(|t| Trk {
            uri: t.uri.clone().into(),
            name: t.name.clone().into(),
            artist: t.artists.clone().into(),
            playable: t.is_playable,
            active: false,
            playing: false,
            selected: false,
        })
        .collect();
    let v = list_pushed(SEL_SEARCH_DRILL as usize, list_identity(rows.iter().map(|r| r.uri.as_str())));
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_search_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        list_shown(SEL_SEARCH_DRILL as usize, v);
    });
}

/// The search drill-in's pager. Same contract as `load_next_track_page`, minus the
/// sort view and presence marks the library list carries.
async fn load_next_search_page(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    pager: &mut TrackPager,
    tracks: &mut Vec<Track>,
) {
    if pager.done || pager.loading || pager.source == PagerSource::None {
        let more = !pager.done && !pager.loading;
        let _ = weak.upgrade_in_event_loop(move |app| app.set_search_tracks_more(more));
        return;
    }
    pager.loading = true;
    let _ = weak.upgrade_in_event_loop(|app| app.set_search_tracks_more(false));

    let limit = pager.page_size();
    let fetched = match &pager.source {
        PagerSource::Liked => session.saved_tracks_page(pager.offset).await,
        PagerSource::Playlist(id) => session.playlist_tracks_page(id, pager.offset).await,
        PagerSource::None => Ok(lightify_core::TrackPage {
            tracks: Vec::new(),
            total: 0,
            partial: false,
        }),
    };
    match fetched {
        Ok(page) => {
            pager.partial |= page.partial;
            let n = page.tracks.len() as u32;
            if page.total > 0 {
                pager.total = page.total;
            }
            tracks.extend(page.tracks);
            pager.offset += limit;
            pager.done = n < limit || (pager.total > 0 && tracks.len() as u32 >= pager.total);
            push_search_tracks(weak, tracks);
            let loaded = tracks.len();
            if loaded == 0 {
                set_status(weak, "No tracks to show".to_string());
            } else if pager.done && pager.partial {
                set_status(
                    weak,
                    format!(
                        "Loaded {loaded} tracks from \u{201c}{}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        pager.name
                    ),
                );
            } else if pager.done {
                set_status(weak, format!("Loaded {loaded} tracks from \u{201c}{}\u{201d}", pager.name));
            } else {
                set_status(
                    weak,
                    format!("Loaded {loaded} tracks from \u{201c}{}\u{201d}\u{2026}", pager.name),
                );
            }
        }
        Err(e) => {
            if tracks.is_empty() {
                set_status(weak, format!("\u{201c}{}\u{201d} failed: {e}", pager.name));
            } else {
                set_status(
                    weak,
                    format!("\u{201c}{}\u{201d} stopped after {} tracks: {e}", pager.name, tracks.len()),
                );
            }
            pager.done = true;
        }
    }
    pager.loading = false;
    let more = !pager.done;
    let _ = weak.upgrade_in_event_loop(move |app| app.set_search_tracks_more(more));
}

/// Tell the UI whether another page can be requested. False while a page is in
/// flight or once the list is complete, so the scroll handler can't double-fire.
fn set_tracks_more(weak: &slint::Weak<MainWindow>, more: bool) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_tracks_more(more));
}

/// Fetch the next page and append it, keeping the sort view and the presence marks
/// in step. A no-op when the pager is finished or already loading.
#[allow(clippy::too_many_arguments)]
async fn load_next_track_page(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    pager: &mut TrackPager,
    tracks: &mut Vec<Track>,
    trk_view: &mut Vec<usize>,
    trk_mode: &str,
    trk_dir: &str,
    presence: &Presence,
) {
    if pager.done || pager.loading || pager.source == PagerSource::None {
        set_tracks_more(weak, !pager.done && !pager.loading);
        return;
    }
    pager.loading = true;
    set_tracks_more(weak, false);
    if pager.offset == 0 {
        set_status(weak, format!("Loading \u{201c}{}\u{201d}\u{2026}", pager.name));
    }

    let limit = pager.page_size();
    let fetched = match &pager.source {
        PagerSource::Liked => session.saved_tracks_page(pager.offset).await,
        PagerSource::Playlist(id) => session.playlist_tracks_page(id, pager.offset).await,
        PagerSource::None => Ok(lightify_core::TrackPage {
            tracks: Vec::new(),
            total: 0,
            partial: false,
        }),
    };

    match fetched {
        Ok(page) => {
            let (batch, total) = (page.tracks, page.total);
            pager.partial |= page.partial;
            let n = batch.len() as u32;
            if total > 0 {
                pager.total = total;
            }
            tracks.extend(batch);
            pager.offset += limit;
            // Stop on a short page, or once Spotify's own total is covered.
            pager.done = n < limit || (pager.total > 0 && tracks.len() as u32 >= pager.total);
            *trk_view = sort_track_view(tracks, trk_mode, trk_dir);
            push_tracks(weak, tracks, trk_view);
            let p = presence.clone();
            let _ = weak.upgrade_in_event_loop(move |app| apply_presence(&app, &p));

            let loaded = tracks.len();
            if loaded == 0 {
                set_status(weak, "No tracks to show".to_string());
            } else if pager.done && pager.partial {
                // Non-owned playlists can only be read from the public embed page,
                // which stops at ~100 tracks. Say so rather than letting a 460-track
                // playlist quietly look like a 100-track one.
                set_status(
                    weak,
                    format!(
                        "Loaded {loaded} tracks from \u{201c}{}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        pager.name
                    ),
                );
            } else if pager.done {
                set_status(weak, format!("Loaded {loaded} tracks from \u{201c}{}\u{201d}", pager.name));
            } else {
                let total_text = if pager.total > 0 {
                    format!(" / {}", group_thousands(pager.total))
                } else {
                    String::new()
                };
                set_status(
                    weak,
                    format!(
                        "Loaded {}{total_text} tracks from \u{201c}{}\u{201d}",
                        group_thousands(loaded as u32),
                        pager.name
                    ),
                );
            }
        }
        Err(e) => {
            // Whatever already arrived stays on screen; the original does the same and
            // just stops paging rather than blanking the list.
            if tracks.is_empty() {
                set_status(weak, format!("\u{201c}{}\u{201d} failed: {e}", pager.name));
            } else {
                set_status(
                    weak,
                    format!("\u{201c}{}\u{201d} stopped after {} tracks: {e}", pager.name, tracks.len()),
                );
            }
            pager.done = true;
        }
    }

    pager.loading = false;
    set_tracks_more(weak, !pager.done);
}

// ── Track-list multi-select ──────────────────────────────────────────────────



/// Mark the selected rows in place (`set_row_data`, like the presence highlight) so
/// the list keeps its scroll position. `selected` holds indices into the *source*
/// track vector; `view` maps display rows to it.
/// Which list a selection belongs to. Mirrors the original's per-container
/// selection scopes: selecting in one list clears the others.
/// The DOWNLOADS sidebar's list surface (its `open-context` kind). Not a selection
/// scope — download rows never take part in multi-select.
const LIST_DOWNLOADS: usize = 7;
const SEL_NONE: i32 = 0;
const SEL_TRACKS: i32 = 1;
const SEL_SEARCH: i32 = 2;
const SEL_BEATPORT: i32 = 3;
const SEL_SIDEBAR: i32 = 4;
/// The search tab's own drill-in list (`#search-drill`) — a separate container in
/// the original, so it gets its own selection scope too.
const SEL_SEARCH_DRILL: i32 = 5;

/// The stable key a display row maps to. The drilled track list is sortable, so it
/// keys off the *source* index and a re-sort keeps the same songs selected; the other
/// lists are unsorted, so the display row is the key.
fn sel_key(scope: i32, row: usize, view: &[usize]) -> Option<usize> {
    if scope == SEL_TRACKS {
        view.get(row).copied()
    } else {
        Some(row)
    }
}

fn selection_flags(
    selected: &std::collections::BTreeSet<usize>,
    view: &[usize],
) -> Vec<bool> {
    view.iter().map(|src| selected.contains(src)).collect()
}

/// Flags for an unsorted list, where the display row is the key.
fn selection_flags_direct(
    selected: &std::collections::BTreeSet<usize>,
    len: usize,
) -> Vec<bool> {
    (0..len).map(|i| selected.contains(&i)).collect()
}

/// Mark the selected rows in place (`set_row_data`, like the presence highlight) so
/// the list keeps its scroll position. Runs on the UI thread.
fn apply_selection(app: &MainWindow, flags: &[bool]) {
    let rows = app.get_tracks();
    for (i, want) in flags.iter().enumerate() {
        if let Some(mut row) = rows.row_data(i) {
            if row.selected != *want {
                row.selected = *want;
                rows.set_row_data(i, row);
            }
        }
    }
    app.set_sel_count(flags.iter().filter(|f| **f).count() as i32);
}

fn apply_search_selection(app: &MainWindow, flags: &[bool]) {
    let rows = app.get_search_results();
    for (i, want) in flags.iter().enumerate() {
        if let Some(mut row) = rows.row_data(i) {
            if row.selected != *want {
                row.selected = *want;
                rows.set_row_data(i, row);
            }
        }
    }
    app.set_sel_count(flags.iter().filter(|f| **f).count() as i32);
}

fn apply_bp_selection(app: &MainWindow, flags: &[bool]) {
    let rows = app.get_bp_rows();
    for (i, want) in flags.iter().enumerate() {
        if let Some(mut row) = rows.row_data(i) {
            if row.selected != *want {
                row.selected = *want;
                rows.set_row_data(i, row);
            }
        }
    }
    app.set_sel_count(flags.iter().filter(|f| **f).count() as i32);
}

fn apply_search_drill_selection(app: &MainWindow, flags: &[bool]) {
    let rows = app.get_search_tracks();
    for (i, want) in flags.iter().enumerate() {
        if let Some(mut row) = rows.row_data(i) {
            if row.selected != *want {
                row.selected = *want;
                rows.set_row_data(i, row);
            }
        }
    }
    app.set_sel_count(flags.iter().filter(|f| **f).count() as i32);
}

fn apply_sidebar_selection(app: &MainWindow, flags: &[bool]) {
    let rows = app.get_sidebar_rows();
    for (i, want) in flags.iter().enumerate() {
        if let Some(mut row) = rows.row_data(i) {
            if row.selected != *want {
                row.selected = *want;
                rows.set_row_data(i, row);
            }
        }
    }
    app.set_sel_count(flags.iter().filter(|f| **f).count() as i32);
}

fn push_selection(
    weak: &slint::Weak<MainWindow>,
    scope: i32,
    selected: &std::collections::BTreeSet<usize>,
    view: &[usize],
) {
    let selected = selected.clone();
    let flags = if scope == SEL_TRACKS { Some(selection_flags(&selected, view)) } else { None };
    let _ = weak.upgrade_in_event_loop(move |app| match scope {
        SEL_TRACKS => apply_selection(&app, &flags.unwrap_or_default()),
        SEL_SEARCH => {
            let n = app.get_search_results().row_count();
            apply_search_selection(&app, &selection_flags_direct(&selected, n));
        }
        SEL_BEATPORT => {
            let n = app.get_bp_rows().row_count();
            apply_bp_selection(&app, &selection_flags_direct(&selected, n));
        }
        SEL_SIDEBAR => {
            let n = app.get_sidebar_rows().row_count();
            apply_sidebar_selection(&app, &selection_flags_direct(&selected, n));
        }
        SEL_SEARCH_DRILL => {
            let n = app.get_search_tracks().row_count();
            apply_search_drill_selection(&app, &selection_flags_direct(&selected, n));
        }
        _ => {}
    });
}

/// The selected URIs for whichever surface currently owns the selection. Beatport is
/// deliberately absent: its rows are Beatport tracks that must be resolved to Spotify
/// first, which needs the network and so lives in the worker.
fn scoped_selection_uris(
    scope: i32,
    selected: &std::collections::BTreeSet<usize>,
    view: &[usize],
    tracks: &[Track],
    search_actions: &[SearchAction],
    sidebar_tracks: &[Track],
    search_drill: &[Track],
    playable_only: bool,
) -> Vec<String> {
    match scope {
        SEL_TRACKS => selection_uris(selected, view, tracks, playable_only),
        SEL_SEARCH_DRILL => {
            let mut seen = std::collections::HashSet::new();
            search_drill
                .iter()
                .enumerate()
                .filter(|(i, _)| selected.contains(i))
                .map(|(_, t)| t)
                .filter(|t| !playable_only || t.is_playable)
                .filter(|t| !t.uri.is_empty())
                .filter(|t| seen.insert(t.uri.clone()))
                .map(|t| t.uri.clone())
                .take(50)
                .collect()
        }
        SEL_SEARCH => {
            let mut seen = std::collections::HashSet::new();
            search_actions
                .iter()
                .enumerate()
                .filter(|(i, _)| selected.contains(i))
                .filter_map(|(_, a)| match a {
                    SearchAction::Track { uri, .. } if !uri.is_empty() => Some(uri.clone()),
                    _ => None,
                })
                .filter(|u| seen.insert(u.clone()))
                .take(50)
                .collect()
        }
        SEL_SIDEBAR => {
            let mut seen = std::collections::HashSet::new();
            sidebar_tracks
                .iter()
                .enumerate()
                .filter(|(i, _)| selected.contains(i))
                .map(|(_, t)| t)
                .filter(|t| !playable_only || t.is_playable)
                .filter(|t| !t.uri.is_empty())
                .filter(|t| seen.insert(t.uri.clone()))
                .map(|t| t.uri.clone())
                .take(50)
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The selected tracks' URIs in display order, de-duplicated. `playable_only` drops
/// region-locked rows the way `playableTracks` does for playback.
fn selection_uris(
    selected: &std::collections::BTreeSet<usize>,
    view: &[usize],
    tracks: &[Track],
    playable_only: bool,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    view.iter()
        .filter(|src| selected.contains(src))
        .filter_map(|&src| tracks.get(src))
        .filter(|t| !playable_only || t.is_playable)
        .filter(|t| !t.uri.is_empty())
        .filter(|t| seen.insert(t.uri.clone()))
        .map(|t| t.uri.clone())
        // Spotify's play endpoint takes at most 50 uris; the host slices too.
        .take(50)
        .collect()
}

/// `buildTimestampedPlaylistName` — "Selection 2026-09-09 14:05" in **local** time,
/// which is what the browser's `new Date()` gives the shipped app.
fn timestamped_playlist_name(prefix: &str) -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut st) };
        return format!(
            "{prefix} {:04}-{:02}-{:02} {:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
        );
    }
    #[cfg(not(windows))]
    {
        // Same shape as the Windows branch, in local time. Returning a bare prefix
        // here (as this used to) meant every generated playlist on Linux was called
        // just "Queue" / "BP Selection", so a second one collided with the first.
        use chrono::Local;
        return format!("{prefix} {}", Local::now().format("%Y-%m-%d %H:%M"));
    }
}

/// Start Spotify's native song radio for a seed track — ports `startStationFromTrack`
/// (`app.js:5155`). The dead end here used to be that the shell never spoke the two
/// engine commands, even though `lightify-audio` has implemented them all along:
/// `autoplay` flips the live user attribute spirc reads, and `station` loads
/// `spotify:station:track:<id>` as a real context (the engine derives that URI, and
/// disconnects/activates around the load so leftover queued tracks can't survive).
///
/// Repeat is cleared **in the UI only**, exactly as the host does: the engine's
/// `handle_load` already calls `reset_options()`, and a Web-API repeat call would go
/// through `me/player` and can transfer with `play:false`, interrupting the very
/// station being started.
/// Which station request is the live one.
///
/// A station is built in two halves: the request goes to the engine, and — seconds
/// later, up to the engine's 10 s resolve timeout — its tracks come back as an event
/// and get queued. Nothing tied the answer to the request, so an answer to a station
/// the user had already abandoned (they played something else, cleared the queue,
/// started another station) was queued as if it were current: the first station's
/// tracks landing on top of the second. Every request now carries the epoch it was
/// made under, and everything that ends the station lane — a new play, Clear, a new
/// station — moves the epoch on, so a late answer is recognised and dropped.
static STATION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A new station request begins; returns the epoch to send with it.
fn next_station_epoch() -> u64 {
    STATION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

/// Whatever station request is still in flight is no longer wanted.
fn cancel_pending_station() {
    STATION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn station_epoch_is_current(seq: u64) -> bool {
    seq == STATION_EPOCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// The engine command that empties the device queue. With `keep_current` (and a
/// current track) the song keeps playing from where it is; otherwise it is the hard
/// reset that stops playback, for callers about to start something else anyway.
fn clear_queue_command(last: &Option<PlaybackState>, keep_current: bool) -> String {
    let current = last.as_ref().filter(|_| keep_current).and_then(|p| {
        p.track.as_ref().filter(|t| !t.uri.is_empty()).map(|t| (t.uri.clone(), p.progress_ms, p.is_playing))
    });
    match current {
        Some((uri, position_ms, playing)) => serde_json::json!({
            "cmd": "clearqueue", "keep_uri": uri, "position_ms": position_ms, "playing": playing,
        }),
        None => serde_json::json!({ "cmd": "clearqueue" }),
    }
    .to_string()
}

/// Empty the device's queue and keep the current song going from where it was.
///
/// Emptying it takes a real reset of the engine's Connect state (a plain reload spares
/// everything in the user queue — see the engine's `ClearQueue`). The catch is what
/// Spotify then believes: after the reset it keeps showing the device as paused, at the
/// moment it was reset, and ignores the device's own later updates until a command comes
/// from its side. The app reads that state back for its now-playing UI and play/pause
/// button, so "audio playing, UI paused" is the result of resuming from the engine alone
/// (measured 2026-09-28 with `--probe-clear-sync`: stuck for 40 s+). So:
///
/// * **playing** — the engine resets, then Spotify itself is told to play the song at
///   that position on this device. Server-driven, so both sides agree from the first
///   read; costs a second or so of silence.
/// * **paused** — the same route would play a moment of sound, so the engine reloads the
///   song paused and a seek from Spotify's side (silent) brings its view back in line.
///
/// If Spotify can't be reached for the play, the engine reloads the song itself, so the
/// music never just stops.
async fn clear_device_queue(session: &mut Session, last: &Option<PlaybackState>) {
    let current = last.as_ref().and_then(|p| {
        p.track.as_ref().filter(|t| !t.uri.is_empty()).map(|t| (t.uri.clone(), p.progress_ms, p.is_playing))
    });
    match current {
        Some((uri, position_ms, true)) => {
            engine::send(&clear_queue_command(&None, false));
            tokio::time::sleep(Duration::from_millis(600)).await;
            let mut resumed = false;
            for _ in 0..4 {
                if session.play_at(None, &uri, position_ms).await.is_ok() {
                    resumed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if !resumed {
                engine::send(&clear_queue_command(last, true));
            }
        }
        Some((_, position_ms, false)) => {
            engine::send(&clear_queue_command(last, true));
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let _ = session.seek(position_ms).await;
        }
        None => {
            engine::send(&clear_queue_command(&None, false));
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
}

/// After a queue clear: give the device time to come back (the reset deactivates it
/// for a moment), then read the queue as Spotify now has it. `None` = never got an
/// answer, so the caller must not assume anything about what survived.
async fn queue_after_clear(session: &mut Session, playing_uri: &str) -> Option<Vec<Track>> {
    for attempt in 0..10 {
        tokio::time::sleep(Duration::from_millis(if attempt == 0 { 900 } else { 500 })).await;
        // The reload is done once the same song is loaded again.
        let back = match session.playback().await {
            Ok(Some(pb)) => pb.track.as_ref().is_some_and(|t| playing_uri.is_empty() || t.uri == playing_uri),
            _ => false,
        };
        if back {
            if let Ok(q) = session.queue().await {
                return Some(q);
            }
        }
    }
    None
}

async fn start_station(weak: &slint::Weak<MainWindow>, seed_uri: &str) {
    if !seed_uri.starts_with("spotify:track:") {
        set_status(weak, "Start station needs a Spotify track".to_string());
        return;
    }
    if !engine::running() {
        set_status(
            weak,
            "Start station needs Lightify\u{2019}s own player \u{2014} Settings \u{2192} Restart"
                .to_string(),
        );
        return;
    }
    // Autoplay (which keeps the radio going once the queued tracks run out, the way the
    // host's station does) is NOT switched on here. It used to be, at request time —
    // before anything was known about the station. Play something else in the seconds
    // a station takes to resolve, or have the resolve fail, and no station lane was
    // "active" for the next play to end, so autoplay stayed on for good and every later
    // playlist drifted into radio. It is switched on where the station's tracks are
    // accepted (`Event::StationTracks`), which is also the point the lane becomes active.
    // Resolve only. Loading the station as a context (what this used to do, and what
    // the host does) replaces whatever is playing with the station's first track —
    // the tracks come back as an event and get queued behind the current song instead.
    engine::send(
        &serde_json::json!({
            "cmd": "stationtracks", "context_uri": seed_uri, "seq": next_station_epoch(),
        })
        .to_string(),
    );
    set_status(weak, "Building station\u{2026}".to_string());
}

// ── Row context menus (the native `.ctx-menu`) ───────────────────────────────

/// Honest notes for menu rows the shipped app has but this shell can't run yet.
/// The rows stay visible (dimmed) rather than vanishing — see PARITY.md.
const FOLLOW_ARTIST_NOTE: &str =
    "Follow artist needs the user-follow-modify scope, which this token doesn\u{2019}t carry";

/// One built menu row: what it says, how it reads, and what it runs.
struct CtxItem {
    label: String,
    danger: bool,
    muted: bool,
    cmd: Cmd,
}

impl CtxItem {
    fn new(label: impl Into<String>, cmd: Cmd) -> Self {
        Self { label: label.into(), danger: false, muted: false, cmd }
    }
    fn danger(label: impl Into<String>, cmd: Cmd) -> Self {
        Self { label: label.into(), danger: true, muted: false, cmd }
    }
    /// A row the shipped app performs but this build can't — dimmed, and clicking
    /// it explains why in the status bar.
    fn muted(label: impl Into<String>, note: &'static str) -> Self {
        Self { label: label.into(), danger: false, muted: true, cmd: Cmd::Note(note.to_string()) }
    }
}

/// The group menu shown when the right-clicked row is part of a multi-row
/// selection — ports `buildSpotifySelectionMenu` (`app.js:1253`).
fn selection_menu(count: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(format!("Play {count} selected"), Cmd::PlaySelection),
        CtxItem::new(format!("Add {count} to queue"), Cmd::QueueSelection),
        CtxItem::new("Create playlist from selection", Cmd::CreatePlaylistFromSelection),
    ]
}

/// The Liked Songs row menu — ports `app.js:1894`.
fn liked_row_menu() -> Vec<CtxItem> {
    vec![
        CtxItem::new("Open", Cmd::Drill(0)),
        CtxItem::new("Download liked songs", Cmd::DownloadLibraryRow(0)),
    ]
}

/// A playlist row menu — ports `app.js:1921`. `row` is the *display* index
/// (row 0 is Liked Songs, so a playlist is always >= 1).
fn playlist_row_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new("Play", Cmd::PlayRow(row)),
        CtxItem::new("Save to library", Cmd::FollowPlaylist(row)),
        CtxItem::new("Download", Cmd::DownloadLibraryRow(row)),
        CtxItem::danger("Delete", Cmd::ConfirmDeletePlaylist(row)),
    ]
}

/// Map a clicked track row back to its *current* display index. `row` is where it was
/// on screen when clicked; `uri` is what was there. If the list has been re-sorted or
/// extended since, find the same track again rather than trusting the stale index.
/// An empty `uri` (a row with no uri) falls back to the index as-is.
fn resolve_track_row(view: &[usize], tracks: &[Track], row: usize, uri: &str) -> Option<usize> {
    let at = |i: usize| view.get(i).and_then(|&k| tracks.get(k));
    if uri.is_empty() || at(row).is_some_and(|t| t.uri == uri) {
        return at(row).map(|_| row);
    }
    view.iter().position(|&k| tracks.get(k).is_some_and(|t| t.uri == uri))
}

/// A drilled track row menu — ports `buildSpotifyTrackMenu` (`app.js:1205`).
/// The optional "Like" row is deliberately absent: the original only adds it in
/// *search* containers (`buildSearchTrackMenu`), not in the library list.
fn track_row_menu(row: usize, t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new("Play", Cmd::PlayTrack { row, uri: t.uri.clone() })];
    if !t.uri.is_empty() {
        items.push(CtxItem::new("Add to queue", Cmd::QueueTrack { row, uri: t.uri.clone() }));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new("Share", Cmd::ShareTrack(row)));
        items.push(CtxItem::new("Start station", Cmd::StationFromUri(t.uri.clone())));
        if !t.artist_ids.is_empty() {
            items.push(CtxItem::muted("Follow artist", FOLLOW_ARTIST_NOTE));
        }
        items.push(CtxItem::new("Download", Cmd::Download {
            url: format!("https://open.spotify.com/track/{}", t.id),
            label: t.name.clone(),
        }));
    }
    items
}

/// A search *track* row — `buildSearchTrackMenu` (`app.js:1235`). Same as the
/// library track menu plus a "Like" row, which the original shows only when the
/// track is NOT already saved (it asks `cmd_check_liked` first; so do we).
fn search_track_menu(row: usize, uri: &str, id: &str, show_like: bool) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new("Play", Cmd::OpenSearch(row))];
    if !uri.is_empty() {
        items.push(CtxItem::new("Add to queue", Cmd::QueueSearchTrack(row)));
    }
    if !id.is_empty() {
        if show_like {
            items.push(CtxItem::new("Like", Cmd::LikeSearchTrack(row)));
        }
        items.push(CtxItem::new("Share", Cmd::ShareSearchTrack(row)));
        items.push(CtxItem::new("Start station", Cmd::StationFromUri(uri.to_string())));
        items.push(CtxItem::new("Download", Cmd::DownloadSearchItem(row)));
    }
    items
}

/// A search *playlist* hit — `appendSearchPlaylistResult` (`app.js:2764`).
fn search_playlist_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new("Play", Cmd::PlaySearchContext(row)),
        CtxItem::new("Show tracks", Cmd::OpenSearch(row)),
        CtxItem::new("Save playlist", Cmd::SaveSearchItem(row)),
        CtxItem::new("Download", Cmd::DownloadSearchItem(row)),
    ]
}

/// A search *album* hit — `app.js:2749`.
fn search_album_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new("Play", Cmd::PlaySearchContext(row)),
        CtxItem::new("Show tracks", Cmd::OpenSearch(row)),
        CtxItem::new("Save album", Cmd::SaveSearchItem(row)),
    ]
}

/// A search *artist* hit — `app.js:2731`.
fn search_artist_menu(row: usize) -> Vec<CtxItem> {
    vec![CtxItem::new("Show tracks", Cmd::OpenSearch(row))]
}

/// A Beatport chart row — `app.js:3489`.
fn beatport_row_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new("Play", Cmd::BpPlay(row)),
        CtxItem::new("Add to queue", Cmd::BpQueue(row)),
        CtxItem::new("Download", Cmd::DownloadBeatportRow(row)),
    ]
}

/// `buildBeatportSelectionMenu` (`app.js:4020`).
fn beatport_selection_menu(count: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(format!("Play {count} selected"), Cmd::BpPlaySelection),
        CtxItem::new(format!("Add {count} to queue"), Cmd::BpQueueSelection),
        CtxItem::new("Create playlist from selection", Cmd::BpCreatePlaylist),
    ]
}

/// `buildRecentSidebarSelectionMenu` (`app.js:5773`) — RECENT has no "create
/// playlist"; the QUEUE mode uses the ordinary `buildSpotifySelectionMenu`.
fn sidebar_recent_selection_menu(count: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new("Play first selected", Cmd::SidebarPlayFirstSelected),
        CtxItem::new(format!("Add {count} to queue"), Cmd::QueueSelection),
    ]
}

/// The now-playing menu — `showCurrentTrackContextMenu` (`app.js:5222`), bound to the
/// album art and the track info. It is `buildSpotifyTrackMenu` against the playing
/// track, so the rows match a track row's menu exactly.
fn now_playing_menu(t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new("Play", Cmd::PlayCurrentTrack)];
    if !t.uri.is_empty() {
        items.push(CtxItem::new("Add to queue", Cmd::AddQueue));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new("Share", Cmd::ShareCurrentTrack));
        items.push(CtxItem::new("Start station", Cmd::StationFromUri(t.uri.clone())));
        if !t.artist_ids.is_empty() {
            items.push(CtxItem::muted("Follow artist", FOLLOW_ARTIST_NOTE));
        }
        items.push(CtxItem::new("Download", Cmd::Download {
            url: format!("https://open.spotify.com/track/{}", t.id),
            label: t.name.clone(),
        }));
    }
    items
}

/// A row in the search tab's drill-in — `buildSpotifyTrackMenu` against a track of
/// the drilled playlist/album.
fn search_drill_menu(row: usize, t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new("Play", Cmd::PlaySearchDrillTrack(row))];
    if !t.uri.is_empty() {
        items.push(CtxItem::new("Add to queue", Cmd::QueueSearchDrillTrack(row)));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new("Share", Cmd::ShareSearchDrillTrack(row)));
        items.push(CtxItem::new("Start station", Cmd::StationFromUri(t.uri.clone())));
        if !t.artist_ids.is_empty() {
            items.push(CtxItem::muted("Follow artist", FOLLOW_ARTIST_NOTE));
        }
        items.push(CtxItem::new("Download", Cmd::Download {
            url: format!("https://open.spotify.com/track/{}", t.id),
            label: t.name.clone(),
        }));
    }
    items
}

/// A sidebar row — `buildSpotifyTrackMenu` against the queue/recent track.
fn sidebar_row_menu(row: usize, t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new("Play", Cmd::SidebarPlay(row))];
    if !t.uri.is_empty() {
        items.push(CtxItem::new("Add to queue", Cmd::SidebarQueueRow(row)));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new("Share", Cmd::ShareSidebarTrack(row)));
        items.push(CtxItem::new("Start station", Cmd::StationFromUri(t.uri.clone())));
        items.push(CtxItem::new("Download", Cmd::Download {
            url: format!("https://open.spotify.com/track/{}", t.id),
            label: t.name.clone(),
        }));
    }
    items
}

/// A DOWNLOADS sidebar row — ports `buildDownloadItemMenu` (`app.js:5697`).
/// Retry and Cancel are mutually exclusive and both disappear once a row is
/// finished; "Open folder" is always offered, and only a file that is really on
/// disk can be deleted.
fn download_row_menu(item: &downloader::Item) -> Vec<CtxItem> {
    let mut items = Vec::new();
    match item.status.as_str() {
        "Failed" | "Cancelled" => items.push(CtxItem::new("Retry", Cmd::DownloadRetry(item.local_id.clone()))),
        "Downloaded" | "Already Exists" | "Deleted" | "Unavailable" => {}
        _ => items.push(CtxItem::new("Cancel", Cmd::DownloadCancel(item.local_id.clone()))),
    }
    items.push(CtxItem::new(
        if item.file_path.is_empty() { "Open download folder" } else { "Open folder" },
        Cmd::DownloadOpenFolder(item.file_path.clone()),
    ));
    if item.on_disk() {
        items.push(CtxItem::danger("Delete file", Cmd::DownloadDelete(item.local_id.clone())));
    }
    items
}

/// Push a built menu to the UI (opening it) and return the parallel action list,
/// the same shape the command palette uses.
/// `.ctx-menu` is `min-width: 156px` and grows to fit its longest item. A Slint
/// element that isn't inside a layout can't size itself to its content, so the width
/// is estimated here from the label lengths (12px UI font ≈ 5.8px per glyph, plus the
/// 14px padding on each side) and clamped so a long label never runs off the panel.
fn menu_width(items: &[CtxItem]) -> f32 {
    let longest = items.iter().map(|i| i.label.chars().count()).max().unwrap_or(0);
    (longest as f32 * 5.8 + 30.0).clamp(156.0, 280.0)
}

fn ctx_rows(items: &[CtxItem]) -> Vec<MenuItem> {
    items
        .iter()
        .map(|i| MenuItem { label: i.label.clone().into(), danger: i.danger, muted: i.muted })
        .collect()
}

fn push_context(weak: &slint::Weak<MainWindow>, items: Vec<CtxItem>) -> Vec<Cmd> {
    let rows = ctx_rows(&items);
    let width = menu_width(&items);
    let open = !rows.is_empty();
    let actions: Vec<Cmd> = items.into_iter().map(|i| i.cmd).collect();
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        app.set_ctx_w(width);
        app.set_ctx_open(open);
    });
    actions
}

/// "Share" — copy `open.spotify.com/<kind>/<id>` and say so in the status bar,
/// like `shareSpotifyTrack`.
fn share_spotify_link(weak: &slint::Weak<MainWindow>, kind: &str, id: &str) {
    if id.is_empty() {
        set_status(weak, "Share \u{2014} no Spotify id".to_string());
        return;
    }
    let url = format!("https://open.spotify.com/{kind}/{id}");
    match copy_to_clipboard(&url) {
        Ok(()) => set_status(weak, "Copied Spotify link".to_string()),
        Err(e) => set_status(weak, format!("Share \u{2014} {e}")),
    }
}

/// Copy text to the clipboard — the menu's "Share" (`shareSpotifyTrack`).
/// Slint exposes no clipboard API, so this is the Win32 one directly: no child
/// process to flash a console window, and correct UTF-16.
#[cfg(windows)]
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("clipboard is busy".to_string());
        }
        // Everything past this point must close the clipboard before returning.
        let result = (|| {
            if EmptyClipboard() == 0 {
                return Err("could not clear the clipboard".to_string());
            }
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if handle.is_null() {
                return Err("out of memory".to_string());
            }
            let dst = GlobalLock(handle);
            if dst.is_null() {
                return Err("could not lock clipboard memory".to_string());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst as *mut u16, wide.len());
            GlobalUnlock(handle);
            // On success the system owns `handle` — it must NOT be freed here.
            if SetClipboardData(CF_UNICODETEXT, handle).is_null() {
                return Err("could not set the clipboard".to_string());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

/// Everywhere else: `arboard`. Kept out of the Windows build on purpose - the Win32
/// path above needs no dependency and no child process.
#[cfg(not(windows))]
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    cb.set_text(text.to_string()).map_err(|e| format!("could not set the clipboard: {e}"))
}

/// Decode album-art bytes to raw RGBA (Send, so it can cross to the UI thread) plus
/// the extracted accent-glow colour. The slint::Image itself is built on the UI thread.
fn decode_art(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32, slint::Color)> {
    let img = image::load_from_memory(bytes).ok()?;
    let glow = dominant_color(&img);
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h, glow))
}

/// Pick a vivid representative colour from a downscaled copy (most saturated·bright),
/// falling back to the average — the native analogue of the original 16×16 sampler.
fn dominant_color(img: &image::DynamicImage) -> slint::Color {
    let small = img.resize(16, 16, image::imageops::FilterType::Triangle).to_rgb8();
    let (mut br, mut bg, mut bb, mut best) = (52u8, 211u8, 153u8, 0.0f32);
    let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
    for p in small.pixels() {
        let (r, g, b) = (p[0], p[1], p[2]);
        sr += r as u64;
        sg += g as u64;
        sb += b as u64;
        n += 1;
        let (rf, gf, bf) = (r as f32, g as f32, b as f32);
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        let score = sat * (max / 255.0);
        if score > best {
            best = score;
            br = r;
            bg = g;
            bb = b;
        }
    }
    let (mut r, mut g, mut b) = if best < 0.12 && n > 0 {
        ((sr / n) as u8, (sg / n) as u8, (sb / n) as u8)
    } else {
        (br, bg, bb)
    };
    // Brighten to a luminous accent (hue preserved): scale so the top channel ≈ 215,
    // so the glow ADDS light over the dark background instead of muddying it. A dark
    // cover therefore still yields a visible, on-hue halo.
    let maxc = r.max(g).max(b) as f32;
    if maxc > 1.0 {
        let scale = (215.0 / maxc).min(6.0);
        r = ((r as f32) * scale).min(255.0) as u8;
        g = ((g as f32) * scale).min(255.0) as u8;
        b = ((b as f32) * scale).min(255.0) as u8;
    }
    slint::Color::from_argb_u8(120, r, g, b)
}

/// How long before the end of a track its successor's cover is fetched (UI-PLAN D9).
const PREFETCH_ART_BEFORE_MS: u64 = 15_000;

type DecodedArt = (Vec<u8>, u32, u32, slint::Color);

/// One prefetched, already-decoded cover: (url, pixels). One slot — only the next
/// track is ever warmed (≈1.6 MB while it waits).
static PREFETCHED_ART: std::sync::Mutex<Option<(String, DecodedArt)>> = std::sync::Mutex::new(None);
/// The URL last handed to the prefetcher, so each cover is fetched once.
static PREFETCH_ASKED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Fetch + decode `url` on a helper thread (never the worker or UI thread).
fn prefetch_art(url: &str, current: &str) {
    if url.is_empty() || url == current {
        return;
    }
    {
        let Ok(mut asked) = PREFETCH_ASKED.lock() else { return };
        if *asked == url {
            return;
        }
        *asked = url.to_string();
    }
    let url = url.to_string();
    let _ = std::thread::Builder::new().name("art-prefetch".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let bytes = rt.block_on(async {
            let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().ok()?;
            client.get(&url).send().await.ok()?.error_for_status().ok()?.bytes().await.ok()
        });
        if let Some(decoded) = bytes.as_deref().and_then(decode_art) {
            if let Ok(mut slot) = PREFETCHED_ART.lock() {
                *slot = Some((url, decoded));
            }
        }
    });
}

/// The prefetched cover, if it is the one wanted. Clears the slot either way.
fn take_prefetched_art(url: &str) -> Option<DecodedArt> {
    let mut slot = PREFETCHED_ART.lock().ok()?;
    match slot.take() {
        Some((u, d)) if u == url => Some(d),
        _ => None,
    }
}

fn set_art(weak: &slint::Weak<MainWindow>, rgba: Vec<u8>, w: u32, h: u32, glow: slint::Color) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
        // Keep the outgoing cover under the new one for the cross-fade (A6).
        app.set_prev_art(app.get_album_art());
        app.set_album_art(slint::Image::from_rgba8(buf));
        app.set_glow(glow);
        // Solid (opaque) form of the same album tint, for the mini-player frame + edge line.
        let solid = slint::Color::from_argb_u8(255, glow.red(), glow.green(), glow.blue());
        app.set_mini_accent(solid);
        // Re-point the global accent too — like the original's `applyDynamicAccent`
        // reassigning `--accent`, this is what recolours the play/pause button,
        // volume slider, progress bar, tabs, and every highlighted track/playlist row.
        app.global::<Pal>().set_accent(solid);
    });
}

fn clear_art(weak: &slint::Weak<MainWindow>) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_album_art(slint::Image::default());
        app.set_glow(slint::Color::from_argb_u8(0x55, 0x34, 0xd3, 0x99));
        let default_accent = slint::Color::from_argb_u8(0xff, 0x34, 0xd3, 0x99);
        app.set_mini_accent(default_accent);
        // Matches `resetDynamicAccent` — back to the default teal when no cover is loaded.
        app.global::<Pal>().set_accent(default_accent);
    });
}

fn fmt_time(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn group_thousands(n: u32) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A synchronously-fetched snapshot of real data, for headless live rendering.
struct Snapshot {
    playlists: Vec<lightify_core::Playlist>,
    now: Option<PlaybackState>,
    status: String,
    drill: Option<(String, Vec<Track>)>, // (title, tracks) when rendering a drilled view
    art: Option<(Vec<u8>, u32, u32, slint::Color)>, // decoded cover for headless render
    tab: i32,
    search: Vec<SearchRow>,
    shuffle: bool,
    repeat: String,
    sidebar: Option<(i32, Vec<Track>)>, // (mode, tracks) for headless sidebar render
    /// The downloader bridge's real queue, for a DOWNLOADS-mode sidebar shot.
    downloads: Option<Vec<downloader::Item>>,
    /// The downloader's account state (summary, ready), for the settings shot.
    downloader: Option<(String, bool)>,
    beatport: Option<(usize, String, Vec<BeatportTrack>)>, // (genre idx, kind, tracks)
    settings: Option<Vec<Device>>, // Some → render the settings modal open with these devices
    palette: bool, // true → render the command palette open (empty query)
    liked: bool,   // Liked-Songs state of the currently-playing track
    sort: (String, String), // library sort (mode, direction) the render should apply
    engine: (String, bool), // playback-engine status line + healthy flag
    mini: Option<String>, // Some(mode) → render a mini-player layout (square | bar | nano)
    /// Some((title, tracks)) → the search tab drilled into a hit (`#search-drill`).
    search_drill: Option<(String, Vec<Track>)>,
    /// Some(kind) → render a row menu (or the confirm dialog) open, for --shot-ctx.
    /// kind = library | liked | track | confirm.
    ctx: Option<String>,
}

/// Like fetch_snapshot but switched to the Search tab with results for `query`.
/// `--shot-search`'s query and filter, so the render shows them in the box and chips.
static SHOT_SEARCH: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

async fn fetch_search_snapshot(query: &str, filter: &str) -> Snapshot {
    let mut base = fetch_snapshot().await;
    base.tab = 1;
    if let Ok(mut s) = Session::load() {
        if let Ok(res) = s.search(query).await {
            base.search = build_search(&res, filter, query).0;
            thumbs::prefetch_search(&base.search).await;
        }
    }
    base
}

/// Like fetch_snapshot but also drills into a library row (0 = Liked Songs), for
/// headless verification of the track-list view.
async fn fetch_drill_snapshot(index: usize) -> Snapshot {
    let mut base = fetch_snapshot().await;
    // The first page only — the same thing the interactive drill-in shows before
    // the user scrolls, so the render matches the app.
    if index == 0 {
        if let Ok(mut s) = Session::load() {
            if let Ok(page) = s.saved_tracks_page(0).await {
                base.drill = Some(("Liked Songs".to_string(), page.tracks));
            }
        }
    } else if let Some(pl) = base.playlists.get(index - 1).cloned() {
        if let Ok(mut s) = Session::load() {
            if let Ok(page) = s.playlist_tracks_page(&pl.id, 0).await {
                // Mirror the status the interactive pager sets, so the render shows
                // the same honesty about an embed-only list.
                if page.partial {
                    base.status = format!(
                        "Loaded {} tracks from \u{201c}{}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        page.tracks.len(),
                        pl.name
                    );
                }
                base.drill = Some((pl.name.clone(), page.tracks));
            }
        }
    }
    base
}

async fn fetch_snapshot() -> Snapshot {
    let mut s = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            return Snapshot {
                playlists: vec![],
                now: None,
                status: format!("Not connected — {e}"),
                drill: None,
                art: None,
                tab: 0,
                search: vec![],
                shuffle: false,
                repeat: "off".into(),
                sidebar: None,
                downloads: None,
                downloader: None,
                beatport: None,
                settings: None,
                palette: false,
                liked: false,
                sort: default_sort(),
                engine: default_engine(),
                mini: None,
                ctx: None,
                search_drill: None,
            }
        }
    };
    if let Err(e) = s.ensure_fresh().await {
        return Snapshot {
            playlists: vec![],
            now: None,
            status: format!("Auth error — {e}"),
            drill: None,
            art: None,
            tab: 0,
            search: vec![],
            shuffle: false,
            repeat: "off".into(),
            sidebar: None,
        downloads: None,
        downloader: None,
            beatport: None,
            settings: None,
            palette: false,
            liked: false,
            sort: default_sort(),
            engine: default_engine(),
            mini: None,
            ctx: None,
            search_drill: None,
        };
    }
    let status = {
        let n = s.display_name();
        if n.is_empty() { "Connected".to_string() } else { format!("Connected as {n}") }
    };
    let playlists = s.playlists().await.unwrap_or_default();
    thumbs::prefetch(&playlists).await;
    let mut now = s.playback().await.ok().flatten();
    // Mirror the live app's resume offer (UI-PLAN D3): nothing playing anywhere +
    // a saved resume point = that track, paused where it stopped.
    let mut resume_status = None;
    if now.is_none() {
        if let Some(rp) = load_resume() {
            resume_status = Some("Press play to pick up where you left off".to_string());
            now = Some(resume_as_playback(&rp));
        }
    }
    let art = match now.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.album_art.clone()) {
        Some(url) if !url.is_empty() => s.fetch_bytes(&url).await.ok().and_then(|b| decode_art(&b)),
        _ => None,
    };
    let shuffle = now.as_ref().map(|p| p.shuffle_state).unwrap_or(false);
    let repeat = now.as_ref().map(|p| p.repeat_state.clone()).unwrap_or_else(|| "off".into());
    // Real Liked-Songs state for the playing track, so the heart in a --shot-live
    // render matches what the interactive window shows.
    let liked = match now.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.id.clone()) {
        Some(id) if !id.is_empty() => s.is_track_saved(&id).await.unwrap_or(false),
        _ => false,
    };
    let status = resume_status.unwrap_or(status);
    Snapshot {
        playlists, now, status, drill: None, art, tab: 0, search: vec![], shuffle, repeat,
        sidebar: None, downloads: None, downloader: None,
        beatport: None, settings: None, palette: false, liked, sort: default_sort(), engine: default_engine(), mini: None, ctx: None, search_drill: None,
    }
}

/// Like fetch_snapshot but rendering a mini-player layout (square | bar | nano), for
/// headless verification of each compact layout at its window size.
/// Like fetch_snapshot but with a row menu (or the confirm dialog) open, so the
/// menus can be verified headlessly. "track" drills Liked Songs first so the menu
/// is built from a real track.
/// Search, then drill the first playlist hit — renders the search tab's own
/// drill-in (`#search-drill`) the way clicking that row does.
async fn fetch_search_drill_snapshot(query: &str) -> Snapshot {
    let mut base = fetch_search_snapshot(query, "all").await;
    if let Ok(mut s) = Session::load() {
        if let Ok(res) = s.search(query).await {
            if let Some(pl) = res.playlists.first().cloned() {
                if let Ok(page) = s.playlist_tracks_page(&pl.id, 0).await {
                    base.status = if page.partial {
                        format!(
                            "Loaded {} tracks from \u{201c}{}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                            page.tracks.len(),
                            pl.name
                        )
                    } else {
                        format!("Loaded {} tracks from \u{201c}{}\u{201d}", page.tracks.len(), pl.name)
                    };
                    base.search_drill = Some((pl.name, page.tracks));
                }
            }
        }
    }
    base
}

async fn fetch_ctx_snapshot(kind: &str) -> Snapshot {
    let mut base = if kind == "track" || kind == "selection" {
        fetch_drill_snapshot(0).await
    } else {
        fetch_snapshot().await
    };
    base.ctx = Some(kind.to_string());
    base
}

async fn fetch_mini_snapshot(mode: &str) -> Snapshot {
    let mut base = fetch_snapshot().await;
    base.mini = Some(mode.to_string());
    base
}

/// Like fetch_snapshot but with the Beatport tab open on a scraped chart, for
/// headless verification. `kind` = tracks | hype | releases (genre = Overall).
async fn fetch_beatport_snapshot(kind: &str) -> Snapshot {
    let mut base = fetch_snapshot().await;
    base.tab = 2;
    if let Ok(s) = Session::load() {
        match s.beatport_chart("", kind).await {
            Ok(t) => base.beatport = Some((0, kind.to_string(), t)),
            Err(e) => base.status = format!("Beatport: {e}"),
        }
    }
    base
}

/// Like fetch_snapshot but with the command palette open (empty query).
async fn fetch_palette_snapshot() -> Snapshot {
    let mut base = fetch_snapshot().await;
    base.palette = true;
    base
}

/// Like fetch_snapshot but with the settings modal open on the real device list.
async fn fetch_settings_snapshot() -> Snapshot {
    let mut base = fetch_snapshot().await;
    if let Ok(mut s) = Session::load() {
        // Start the real engine so the shot shows what the user actually sees:
        // Lightify's own device in the list and a live engine row. Without this the
        // screenshot would "verify" a panel that never had a playback device in it.
        base.engine = start_engine_for_snapshot(&mut s).await;
        base.settings = Some(s.devices().await.unwrap_or_default());
    } else {
        base.settings = Some(vec![]);
    }
    // Same reasoning as the device list: ask the bridge rather than rendering a
    // placeholder, so the shot shows the DOWNLOADER row the user actually gets.
    base.downloader = Some(match downloader::status().await {
        Ok(st) => (st.summary(), st.spotify_ready),
        Err(e) => (e, false),
    });
    base
}

/// Bring the engine up for a headless render and wait (briefly) for it to register.
/// Returns the status line the settings panel should show.
async fn start_engine_for_snapshot(session: &mut Session) -> (String, bool) {
    let Some(bin) = engine::find_binary() else {
        return ("Not found".into(), false);
    };
    if session.ensure_fresh().await.is_err() {
        return ("Not started (no session)".into(), false);
    }
    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).unwrap_or(None);
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        return (format!("Failed \u{2014} {e}"), false);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(400)) {
            Ok(engine::Event::Ready { device_id }) => {
                let _ = adopt_engine_device(session, &device_id).await;
                return ("Running \u{00B7} Lightify".into(), true);
            }
            Ok(engine::Event::Failed { msg }) => return (format!("Error \u{2014} {msg}"), false),
            Ok(engine::Event::Exited) => return ("Not running".into(), false),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    ("Starting\u{2026}".into(), false)
}

/// Like fetch_snapshot but with a sidebar mode opened (1=queue, 2=recent, 3=downloads),
/// for headless verification of the overlay.
async fn fetch_sidebar_snapshot(mode: i32) -> Snapshot {
    let mut base = fetch_snapshot().await;
    let tracks = if let Ok(mut s) = Session::load() {
        match mode {
            1 => s.queue().await.unwrap_or_default(),
            2 => s.recently_played(50).await.unwrap_or_default(),
            _ => vec![],
        }
    } else {
        vec![]
    };
    if mode == 3 {
        // Real rows from the real bridge — a DOWNLOADS shot that rendered an empty
        // panel would "verify" nothing. An error still renders (as the empty note).
        base.downloads = Some(downloader::snapshot().await.unwrap_or_default());
    }
    base.sidebar = Some((mode, tracks));
    base
}

fn apply_snapshot(app: &MainWindow, snap: &Snapshot) {
    app.set_status_text(snap.status.clone().into());
    // No usable session: the live app shows the sign-in page (`obtain_session`), so a
    // headless render does too — `LIGHTIFY_DATA_DIR=<empty dir> --shot-live` is what
    // a fresh install sees.
    if let Some(err) = snap.status.strip_prefix("Auth error — ") {
        app.set_signin_open(true);
        app.set_signin_client_id(lightify_core::auth::saved_client_id().into());
        app.set_signin_status(
            format!("Your Spotify sign-in has expired or was revoked \u{2014} authorise again. ({err})").into(),
        );
        app.set_signin_error(true);
    } else if snap.status.starts_with("Not connected") {
        app.set_signin_open(true);
        app.set_signin_client_id(lightify_core::auth::saved_client_id().into());
        app.set_status_text("Sign in to Spotify to start".into());
    }
    // Headless renders derive the playback-presence highlight from the live context URI
    // (a snapshot has no launch history). `snap.sort` mirrors the toolbar selection.
    app.set_engine_status(snap.engine.0.clone().into());
    app.set_engine_ok(snap.engine.1);
    let (sort_mode, sort_dir) = (snap.sort.0.as_str(), snap.sort.1.as_str());
    app.set_lib_sort_mode(sort_mode.into());
    app.set_lib_sort_dir(sort_dir.into());
    let order = sort_playlist_view(&snap.playlists, sort_mode, sort_dir);
    let (active_id, playing_now) = active_source(&snap.now, &None);
    let rows = build_rows(&snap.playlists, &order, &active_id, playing_now);
    app.set_liked_current(snap.liked);
    thumbs::seed_from_disk(&snap.playlists);
    let model = slint::ModelRc::from(Rc::new(slint::VecModel::from(rows)));
    app.set_playlist_art(thumbs::art_model(&model));
    app.set_playlists(model);
    match &snap.now {
        Some(pb) => {
            let (name, artist) = pb
                .track
                .as_ref()
                .map(|t| (t.name.clone(), t.artists.clone()))
                .unwrap_or_else(|| ("\u{2014}".into(), String::new()));
            let dur = pb.duration_ms;
            let pos = pb.progress_ms.min(dur.max(1));
            app.set_track_name(name.into());
            app.set_track_artist(artist.into());
            app.set_progress(if dur > 0 { pos as f32 / dur as f32 } else { 0.0 });
            app.set_elapsed(fmt_time(pos).into());
            app.set_duration(fmt_time(dur).into());
            app.set_playing(pb.is_playing);
            app.set_volume((pb.volume_percent as f32 / 100.0).clamp(0.0, 1.0));
        }
        None => {
            app.set_track_name("\u{2014}".into());
            app.set_track_artist("Nothing playing".into());
            app.set_progress(0.0);
            app.set_elapsed("0:00".into());
            app.set_duration("0:00".into());
            app.set_playing(false);
        }
    }
    app.set_shuffle_on(snap.shuffle);
    app.set_repeat_mode(snap.repeat.clone().into());
    if let Some((rgba, w, h, glow)) = &snap.art {
        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, *w, *h);
        app.set_album_art(slint::Image::from_rgba8(buf));
        app.set_glow(*glow);
        let solid = slint::Color::from_argb_u8(255, glow.red(), glow.green(), glow.blue());
        app.set_mini_accent(solid);
        app.global::<Pal>().set_accent(solid);
    }
    if let Some((title, trks)) = &snap.drill {
        app.set_drilled(true);
        app.set_drill_title(title.clone().into());
        let cur_uri = snap
            .now
            .as_ref()
            .and_then(|p| p.track.as_ref())
            .map(|t| t.uri.clone())
            .unwrap_or_default();
        let rows: Vec<Trk> = sort_track_view(trks, sort_mode, sort_dir)
            .into_iter()
            .filter_map(|i| trks.get(i))
            .map(|t| {
                let active = !cur_uri.is_empty() && t.uri == cur_uri;
                Trk {
                    uri: t.uri.clone().into(),
                    name: t.name.clone().into(),
                    artist: t.artists.clone().into(),
                    playable: t.is_playable,
                    active,
                    playing: active && playing_now,
                    selected: false,
                }
            })
            .collect();
        app.set_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    }
    app.set_tab(snap.tab);
    if let Some((q, f)) = SHOT_SEARCH.lock().ok().and_then(|g| g.clone()) {
        app.set_search_text(q.into());
        app.set_search_filter(f.into());
    }
    if !snap.search.is_empty() {
        thumbs::seed_search_from_disk(&snap.search);
        let art: Vec<slint::Image> = snap.search.iter().map(thumbs::search_art).collect();
        app.set_search_art(slint::ModelRc::from(Rc::new(slint::VecModel::from(art))));
        app.set_search_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(snap.search.clone()))));
    }
    if let Some((mode, trks)) = &snap.sidebar {
        app.set_sidebar_mode(*mode);
        let pid = current_id(&snap.now);
        let rows = build_side_rows(trks, &pid, *mode == 2);
        app.set_sidebar_empty(
            match mode {
                1 => "No queued tracks",
                2 => "No recent tracks yet",
                _ => "",
            }
            .into(),
        );
        app.set_sidebar_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    }
    if let Some(items) = &snap.downloads {
        app.set_sidebar_empty("No downloads queued".into());
        app.set_downloads_clearable(items.iter().any(|i| i.finished()));
        app.set_sidebar_dl_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            build_dl_rows(items),
        ))));
    }
    app.set_settings_download_path(downloader::download_path().into());
    if let Some((text, ready)) = &snap.downloader {
        app.set_downloader_status(text.clone().into());
        app.set_downloader_ready(*ready);
    }
    // Beatport genres are static — always populate them for the tab.
    let genres = lightify_core::beatport_genres();
    let labels: Vec<slint::SharedString> = genres.iter().map(|(n, _)| n.clone().into()).collect();
    app.set_bp_genres(slint::ModelRc::from(Rc::new(slint::VecModel::from(labels))));
    if let Some((idx, kind, tracks)) = &snap.beatport {
        app.set_tab(2);
        app.set_bp_genre_idx(*idx as i32);
        app.set_bp_genre_label(genres.get(*idx).map(|(n, _)| n.clone()).unwrap_or_default().into());
        app.set_bp_kind(kind.clone().into());
        app.set_bp_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(build_bp_rows(tracks)))));
        app.set_bp_status(
            format!(
                "{} tracks \u{2014} {} {}",
                tracks.len(),
                genres.get(*idx).map(|(n, _)| n.as_str()).unwrap_or(""),
                bp_chart_label(kind)
            )
            .into(),
        );
    }
    app.set_account(snap.status.clone().into());
    if let Some(devs) = &snap.settings {
        app.set_settings_open(true);
        if devs.is_empty() {
            app.set_settings_device_status("No devices yet \u{2014} Lightify\u{2019}s own player is still starting.".into());
        }
        app.set_settings_devices(slint::ModelRc::from(Rc::new(slint::VecModel::from(build_device_rows(devs)))));
        // Same rows the interactive path builds, so the shot verifies the real thing.
        let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
        let outputs = engine::audio_outputs();
        let want = configured.trim();
        let missing = !want.is_empty()
            && !want.eq_ignore_ascii_case("default")
            && !outputs.iter().any(|n| n == want);
        let default_active = want.is_empty() || want.eq_ignore_ascii_case("default") || missing;
        let mut rows: Vec<DeviceRow> = vec![DeviceRow { name: "System default".into(), active: default_active }];
        for name in &outputs {
            rows.push(DeviceRow { name: name.clone().into(), active: !default_active && name == want });
        }
        app.set_settings_outputs(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        if missing {
            app.set_settings_output_note(
                format!("Configured output \u{201c}{want}\u{201d} isn\u{2019}t connected \u{2014} playing on the system default.").into(),
            );
        }
    }
    if snap.palette {
        app.set_palette_open(true);
        let playing = snap.now.as_ref().map(|p| p.is_playing).unwrap_or(false);
        let rows: Vec<CmdRow> = build_palette_base(&snap.playlists, &order, playing)
            .iter()
            .map(|c| CmdRow { title: c.title.clone().into(), hint: c.hint.clone().into() })
            .collect();
        app.set_cmdk_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    }
    if let Some((title, tracks)) = &snap.search_drill {
        app.set_tab(1);
        app.set_search_drilled(true);
        app.set_search_drill_title(title.clone().into());
        let rows: Vec<Trk> = tracks
            .iter()
            .map(|t| Trk {
                uri: t.uri.clone().into(),
                name: t.name.clone().into(),
                artist: t.artists.clone().into(),
                playable: t.is_playable,
                active: false,
                playing: false,
                selected: false,
            })
            .collect();
        app.set_search_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    }
    if let Some(kind) = &snap.ctx {
        if kind == "confirm" {
            let name = snap.playlists.first().map(|p| p.name.clone()).unwrap_or_default();
            app.set_confirm_title("DELETE PLAYLIST".into());
            app.set_confirm_message(
                format!("Delete \u{201c}{name}\u{201d} from your Spotify library?").into(),
            );
            app.set_confirm_open(true);
        } else {
            // Same builders the interactive path uses, so the shot verifies the
            // real menus. The coordinates put each menu over its own row.
            // The other three selectable surfaces. Live data can't drive these here
            // (Beatport is Cloudflare-blocked and the queue is usually empty), so the
            // rows are deterministic mocks — the selection marks, the group menus and
            // the action bars are the real code paths.
            if kind == "search" || kind == "bp" || kind == "sidebar" {
                let n = 2;
                let menu = match kind.as_str() {
                    "search" => {
                        app.set_tab(1);
                        let rows = vec![
                            SearchRow { name: "Midnight City".into(), sub: "Song \u{2022} M83".into(), kind: "track".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "Outro".into(), sub: "Song \u{2022} M83".into(), kind: "track".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "Hurry Up, We\u{2019}re Dreaming".into(), sub: "Album \u{2022} M83".into(), kind: "album".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "M83".into(), sub: "Artist".into(), kind: "artist".into(), selected: false, art: Default::default(), meta: Default::default() },
                        ];
                        let len = rows.len();
                        app.set_search_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
                        apply_search_selection(app, &(0..len).map(|i| i < n).collect::<Vec<_>>());
                        selection_menu(n)
                    }
                    "bp" => {
                        app.set_tab(2);
                        let rows: Vec<BpItem> = (1..=6)
                            .map(|i| BpItem {
                                title: format!("{i}. Chart Track {i}").into(),
                                sub: format!("Artist {i} \u{2022} Label").into(),
                                selected: false,
                            })
                            .collect();
                        let len = rows.len();
                        app.set_bp_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
                        app.set_bp_status("Beatport TOP 100 \u{2022} Overall".into());
                        apply_bp_selection(app, &(0..len).map(|i| i < n).collect::<Vec<_>>());
                        beatport_selection_menu(n)
                    }
                    _ => {
                        let rows: Vec<SideRow> = (1..=5)
                            .map(|i| SideRow {
                                name: format!("Queued Track {i}").into(),
                                artist: format!("Artist {i}").into(),
                                time: slint::SharedString::new(),
                                playing: i == 1,
                                selected: false,
                            })
                            .collect();
                        let len = rows.len();
                        app.set_sidebar_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
                        app.set_sidebar_mode(1);
                        apply_sidebar_selection(app, &(0..len).map(|i| i < n).collect::<Vec<_>>());
                        selection_menu(n)
                    }
                };
                app.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(ctx_rows(&menu)))));
                app.set_ctx_w(menu_width(&menu));
                app.set_ctx_x(if kind == "sidebar" { 740.0 } else { 96.0 });
                app.set_ctx_y(160.0);
                app.set_ctx_open(true);
                return;
            }
            // A multi-row selection + its group menu.
            if kind == "selection" {
                let n = 3;
                let rows = app.get_tracks();
                let flags: Vec<bool> = (0..rows.row_count()).map(|i| (1..=n).contains(&i)).collect();
                apply_selection(app, &flags);
                let menu = selection_menu(n);
                app.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(ctx_rows(&menu)))));
                app.set_ctx_w(menu_width(&menu));
                app.set_ctx_x(96.0);
                app.set_ctx_y(212.0);
                app.set_ctx_open(true);
                return;
            }
            let (items, x, y) = match kind.as_str() {
                "nowplaying" => (
                    snap.now
                        .as_ref()
                        .and_then(|p| p.track.as_ref())
                        .filter(|t| !t.id.is_empty())
                        .map(now_playing_menu)
                        .unwrap_or_default(),
                    470.0,
                    250.0,
                ),
                "liked" => (liked_row_menu(), 96.0, 176.0),
                "track" => (
                    snap.drill
                        .as_ref()
                        .and_then(|(_, t)| t.first())
                        .map(|t| track_row_menu(0, t))
                        .unwrap_or_default(),
                    96.0,
                    212.0,
                ),
                _ => (playlist_row_menu(1), 96.0, 222.0),
            };
            app.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(ctx_rows(&items)))));
            app.set_ctx_w(menu_width(&items));
            app.set_ctx_x(x);
            app.set_ctx_y(y);
            app.set_ctx_open(true);
        }
    }
    if let Some(mode) = &snap.mini {
        app.set_mini_mode(mode.clone().into());
    }
}

/// Render one frame of the UI to a PNG using Slint's software renderer, with no
/// window and no GPU. `snap` = Some(real data) for --shot-live, None for mock --shot.
/// Windows' "Animation effects" switch (Settings → Accessibility → Visual effects).
/// Off = the user asked for less motion, so the UI's transitions become instant.
fn reduce_motion() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
        let mut on: i32 = 1;
        let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &mut on as *mut i32 as *mut _, 0) };
        ok != 0 && on == 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Windows 11's UI face ("Segoe UI Variable", optical size Text) when its font file
/// is installed; Segoe UI otherwise (Windows 10). Checked by file rather than left to
/// the font matcher, which would fall back to an arbitrary face if the family is absent.
fn ui_font() -> &'static str {
    let fonts = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    if fonts.join("SegUIVar.ttf").is_file() {
        "Segoe UI Variable Text"
    } else {
        "Segoe UI"
    }
}

fn render_to_png(path: &str, w: u32, h: u32, snap: Option<Snapshot>) {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};

    struct ShotPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for ShotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(ShotPlatform { window: window.clone() }))
        .expect("set_platform");

    let app = MainWindow::new().expect("build UI");
    app.set_app_version(env!("CARGO_PKG_VERSION").into());
    app.set_ui_font(ui_font().into());
    app.global::<Motion>().set_reduced(reduce_motion());
    if let Some(snap) = snap.as_ref() {
        apply_snapshot(&app, snap);
    } else {
        // Sample data stands in for a signed-in session; the property's default
        // ("Connecting…") made every sample render look stuck.
        app.set_status_text("Connected".into());
    }
    window.set_size(slint::PhysicalSize::new(w, h));
    slint::platform::update_timers_and_animations();

    // 8 bits per channel, like the real window (the winit backend presents 32-bit
    // pixels). This used to render RGB565, whose 32 red/blue levels drew visible
    // rings into every gradient — the now-playing glow above all — that the live
    // app never showed, so screenshots misreported the UI.
    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w as usize);
    });

    // The window background is opaque, so every pixel ends fully covered and the
    // premultiplied channels are the final colour.
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in &buffer {
        rgb.push(px.red);
        rgb.push(px.green);
        rgb.push(px.blue);
    }

    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    let _ = app;
    downloader::shutdown();
    println!("wrote {path} ({w}x{h})");
}

#[cfg(test)]
mod tests {
    use super::{
        active_source, another_device_active, beatport_row_menu, beatport_selection_menu,
        build_dl_rows, cmp_display_text, command_match_score, context_playlist_id, download_row_menu,
        QueueMirror, PersistedQueue, queue_restore_point,
        downloader, is_subsequence, menu_width, next_sidebar_mode,
        now_playing_menu, parse_rfc3339_utc, rate_limit_backoff, scoped_selection_uris,
        search_album_menu, search_artist_menu, search_playlist_menu, search_track_menu, sel_key,
        selection_menu, sidebar_recent_selection_menu, sidebar_row_menu, sort_playlist_view,
        sort_track_view, SearchAction, SEL_BEATPORT, SEL_NONE, SEL_SEARCH, SEL_SEARCH_DRILL,
        SEL_SIDEBAR, SEL_TRACKS,
    };
    use lightify_core::{PlaybackState, Playlist, Track};

    fn pl(id: &str, name: &str) -> Playlist {
        Playlist {
            id: id.into(),
            name: name.into(),
            tracks: 0,
            owner: String::new(),
            uri: format!("spotify:playlist:{id}"),
            image: String::new(),
        }
    }

    fn trk(name: &str, artist: &str, added: Option<&str>) -> Track {
        Track {
            id: name.into(),
            name: name.into(),
            artists: artist.into(),
            artist_ids: vec![],
            album: String::new(),
            album_id: String::new(),
            duration_ms: 0,
            uri: format!("spotify:track:{name}"),
            album_art: String::new(),
            added_at: added.map(|s| s.to_string()),
            is_playable: true,
        }
    }

    #[test]
    fn display_compare_is_case_insensitive_and_numeric() {
        use std::cmp::Ordering;
        // sensitivity: 'base' — case doesn't decide the order.
        assert_eq!(cmp_display_text("apple", "Apple"), Ordering::Equal);
        assert_eq!(cmp_display_text("beta", "Alpha"), Ordering::Greater);
        // numeric: true — digit runs compare as numbers, not text.
        assert_eq!(cmp_display_text("Set 2", "Set 10"), Ordering::Less);
        assert_eq!(cmp_display_text("Set 010", "Set 10"), Ordering::Equal);
        // A prefix sorts before the longer string.
        assert_eq!(cmp_display_text("Jazz", "Jazz & Vinyl"), Ordering::Less);
    }

    #[test]
    fn playlist_sort_view_maps_display_to_source() {
        let pls = vec![pl("a", "Zeta"), pl("b", "alpha"), pl("c", "Mid 2"), pl("d", "Mid 10")];
        // "Recent" keeps the API order untouched.
        assert_eq!(sort_playlist_view(&pls, "recent", "asc"), vec![0, 1, 2, 3]);
        // A -> Z: case-insensitive, numeric-aware.
        // "Mid 2" sorts before "Mid 10" because digit runs compare numerically.
        assert_eq!(sort_playlist_view(&pls, "alpha", "asc"), vec![1, 2, 3, 0]);
        // Z -> A is the exact reverse ordering.
        assert_eq!(sort_playlist_view(&pls, "alpha", "desc"), vec![0, 3, 2, 1]);
        // Type-to-filter (D7): case-insensitive substring, applied before the sort, so
        // display indices still map to source indices.
        super::set_filter(&super::PL_FILTER, "MID");
        assert_eq!(sort_playlist_view(&pls, "recent", "asc"), vec![2, 3]);
        assert_eq!(sort_playlist_view(&pls, "alpha", "desc"), vec![3, 2]);
        super::set_filter(&super::PL_FILTER, "nothing like it");
        assert!(sort_playlist_view(&pls, "recent", "asc").is_empty());
        super::set_filter(&super::PL_FILTER, "");
        assert_eq!(sort_playlist_view(&pls, "recent", "asc"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn track_sort_view_recent_then_alpha() {
        let tracks = vec![
            trk("Bravo", "Zed", Some("2024-01-01T00:00:00Z")),
            trk("alpha", "Ann", Some("2026-05-05T00:00:00Z")),
            trk("Charlie", "Moe", None),
        ];
        // Recent = added_at newest first; missing stamps fall to the back but keep order.
        assert_eq!(sort_track_view(&tracks, "recent", "asc"), vec![1, 0, 2]);
        // Alpha = by name, case-insensitively.
        assert_eq!(sort_track_view(&tracks, "alpha", "asc"), vec![1, 0, 2]);
        assert_eq!(sort_track_view(&tracks, "alpha", "desc"), vec![2, 0, 1]);
        // Type-to-filter (D7) matches title or artist.
        super::set_filter(&super::TRK_FILTER, "moe");
        assert_eq!(sort_track_view(&tracks, "recent", "asc"), vec![2]);
        super::set_filter(&super::TRK_FILTER, "A");
        assert_eq!(sort_track_view(&tracks, "alpha", "asc"), vec![1, 0, 2]);
        super::set_filter(&super::TRK_FILTER, "");
    }

    #[test]
    fn playback_context_resolves_the_active_library_row() {
        assert_eq!(
            context_playlist_id(&Some("spotify:playlist:37i9dQ".into())).as_deref(),
            Some("37i9dQ")
        );
        // Legacy user-scoped form.
        assert_eq!(
            context_playlist_id(&Some("spotify:user:jon:playlist:abc123".into())).as_deref(),
            Some("abc123")
        );
        // Albums/artists/no context don't light up a library row.
        assert_eq!(context_playlist_id(&Some("spotify:album:xyz".into())), None);
        assert_eq!(context_playlist_id(&None), None);

        let mut pb = PlaybackState {
            is_playing: true,
            progress_ms: 0,
            duration_ms: 0,
            shuffle_state: false,
            repeat_state: "off".into(),
            volume_percent: 50,
            device_name: String::new(),
            device_id: String::new(),
            track: None,
            context_uri: Some("spotify:playlist:pl1".into()),
        };
        // The live context wins over whatever the user last launched.
        let remembered = Some("liked".to_string());
        assert_eq!(
            active_source(&Some(pb.clone()), &remembered),
            ("pl1".to_string(), true)
        );
        // Liked Songs plays as bare `uris`, so the remembered source is the only signal.
        pb.context_uri = None;
        pb.is_playing = false;
        assert_eq!(
            active_source(&Some(pb), &remembered),
            ("liked".to_string(), false)
        );
        // Nothing playing: no row is highlighted.
        assert_eq!(active_source(&None, &None), (String::new(), false));
    }


    #[test]
    fn rate_limit_parsing() {
        // Matches the exact core formatting: "Rate limited by Spotify — retry in {n}s".
        assert_eq!(rate_limit_backoff("Rate limited by Spotify \u{2014} retry in 7s"), Some(7));
        assert_eq!(rate_limit_backoff("Rate limited by Spotify \u{2014} retry in 5s"), Some(5));
        // Respects the server value for realistic waits (e.g. a multi-minute quota backoff)…
        assert_eq!(rate_limit_backoff("Rate limited by Spotify \u{2014} retry in 300s"), Some(300));
        // …but clamps to a 1h ceiling so a malformed/huge header can't freeze polling forever.
        assert_eq!(rate_limit_backoff("Rate limited by Spotify \u{2014} retry in 3989s"), Some(3600));
        assert_eq!(rate_limit_backoff("Rate limited by Spotify \u{2014} retry in 99999s"), Some(3600));
        // Non-rate-limit errors don't trigger a backoff.
        assert_eq!(rate_limit_backoff("GET me/player 500: server error"), None);
        assert_eq!(rate_limit_backoff("Playback error \u{2014} whatever"), None);
    }

    #[test]
    fn palette_matcher_ranks() {
        // Exact title beats prefix beats substring beats keyword-only.
        let s_exact = command_match_score("play", "play", "play toggle");
        let s_prefix = command_match_score("pl", "play", "play toggle");
        let s_sub = command_match_score("lay", "play", "play toggle");
        let s_kw = command_match_score("toggle", "play", "play toggle");
        assert!(s_exact > s_prefix, "{s_exact} !> {s_prefix}");
        assert!(s_prefix > s_sub, "{s_prefix} !> {s_sub}");
        assert!(s_sub > s_kw, "{s_sub} !> {s_kw}");
        // No match → 0.
        assert_eq!(command_match_score("zzz", "play", "play toggle"), 0);
        // Empty query matches everything.
        assert_eq!(command_match_score("", "anything", "anything"), 1);
    }

    #[test]
    fn subsequence_fuzzy() {
        assert!(is_subsequence("lnd", "late night drive")); // l..n..d in order
        assert!(is_subsequence("", "anything"));
        assert!(!is_subsequence("xyz", "late night drive"));
        assert!(!is_subsequence("dln", "late night drive")); // d comes after l/n → out of order
    }

    #[test]
    fn sidebar_toggle_semantics() {
        // Pressing an inactive button opens that mode.
        assert_eq!(next_sidebar_mode(0, 1), 1, "closed → open queue");
        assert_eq!(next_sidebar_mode(1, 2), 2, "queue → switch to recent");
        assert_eq!(next_sidebar_mode(2, 3), 3, "recent → switch to downloads");
        // Pressing the active button again closes the sidebar.
        assert_eq!(next_sidebar_mode(1, 1), 0, "re-press queue → close");
        assert_eq!(next_sidebar_mode(3, 3), 0, "re-press downloads → close");
    }

    #[test]
    fn rfc3339_parses_utc_seconds() {
        // 2021-01-01T00:00:00Z = 1609459200 (known epoch).
        assert_eq!(parse_rfc3339_utc("2021-01-01T00:00:00Z"), Some(1_609_459_200));
        // Fractional seconds + Z are tolerated (only the first 19 chars are read).
        assert_eq!(parse_rfc3339_utc("2021-01-01T00:00:01.234Z"), Some(1_609_459_201));
        assert_eq!(parse_rfc3339_utc("garbage"), None);
    }

    // ── Row menus ───────────────────────────────────────────────────────────
    fn menu_trk(id: &str, playable: bool) -> Track {
        Track {
            id: id.into(),
            name: id.into(),
            artists: String::new(),
            artist_ids: vec![],
            album: String::new(),
            album_id: String::new(),
            duration_ms: 0,
            uri: format!("spotify:track:{id}"),
            album_art: String::new(),
            added_at: None,
            is_playable: playable,
        }
    }

    fn labels(items: &[super::CtxItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn search_menus_match_the_original() {
        // buildSearchTrackMenu = the track menu, with "Like" only when NOT saved.
        assert_eq!(
            labels(&search_track_menu(0, "spotify:track:x", "x", true)),
            ["Play", "Add to queue", "Like", "Share", "Start station", "Download"]
        );
        assert_eq!(
            labels(&search_track_menu(0, "spotify:track:x", "x", false)),
            ["Play", "Add to queue", "Share", "Start station", "Download"]
        );
        // A track with no id offers only what works without one.
        assert_eq!(labels(&search_track_menu(0, "spotify:track:x", "", true)), ["Play", "Add to queue"]);
        assert_eq!(
            labels(&search_playlist_menu(0)),
            ["Play", "Show tracks", "Save playlist", "Download"]
        );
        assert_eq!(labels(&search_album_menu(0)), ["Play", "Show tracks", "Save album"]);
        assert_eq!(labels(&search_artist_menu(0)), ["Show tracks"]);
        // "Download" was the last dimmed row here; it runs for real now that the
        // OnTheSpot bridge is in the shell (`src/downloader.rs`).
        let menu = search_track_menu(0, "spotify:track:x", "x", false);
        let muted: Vec<&str> =
            menu.iter().filter(|i| i.muted).map(|i| i.label.as_str()).collect();
        assert!(muted.is_empty(), "unexpected dimmed rows: {muted:?}");
    }

    #[test]
    fn beatport_and_sidebar_menus_match_the_original() {
        assert_eq!(labels(&beatport_row_menu(0)), ["Play", "Add to queue", "Download"]);
        assert_eq!(
            labels(&beatport_selection_menu(4)),
            ["Play 4 selected", "Add 4 to queue", "Create playlist from selection"]
        );
        // RECENT has no "create playlist" — buildRecentSidebarSelectionMenu.
        assert_eq!(
            labels(&sidebar_recent_selection_menu(3)),
            ["Play first selected", "Add 3 to queue"]
        );
        // ...while QUEUE reuses buildSpotifySelectionMenu.
        assert_eq!(
            labels(&selection_menu(3)),
            ["Play 3 selected", "Add 3 to queue", "Create playlist from selection"]
        );
        let full = menu_trk("q1", true);
        let bare = Track { id: String::new(), uri: String::new(), ..menu_trk("q2", true) };
        assert_eq!(
            labels(&sidebar_row_menu(0, &full)),
            ["Play", "Add to queue", "Share", "Start station", "Download"]
        );
        assert_eq!(labels(&sidebar_row_menu(0, &bare)), ["Play"]);
        // The now-playing menu is buildSpotifyTrackMenu against the playing track, so
        // its rows must match a track row's exactly (Follow artist only with artist ids).
        let mut with_artist = menu_trk("np", true);
        with_artist.artist_ids = vec!["a1".into()];
        assert_eq!(
            labels(&now_playing_menu(&with_artist)),
            ["Play", "Add to queue", "Share", "Start station", "Follow artist", "Download"]
        );
        assert_eq!(
            labels(&now_playing_menu(&menu_trk("np", true))),
            ["Play", "Add to queue", "Share", "Start station", "Download"]
        );
        // Both were dimmed once: "Start station" became real when the shell learned
        // the engine's autoplay commands, and "Download" when the OnTheSpot bridge
        // landed (`src/downloader.rs`). Nothing in this menu is a placeholder now.
        let menu = sidebar_row_menu(0, &full);
        let muted: Vec<&str> =
            menu.iter().filter(|i| i.muted).map(|i| i.label.as_str()).collect();
        assert!(muted.is_empty(), "unexpected dimmed rows: {muted:?}");
    }

    fn dl(status: &str, file: &str) -> downloader::Item {
        downloader::Item {
            local_id: "x-0".into(),
            name: "Never Gonna Give You Up".into(),
            status: status.into(),
            file_path: file.into(),
            ..Default::default()
        }
    }

    #[test]
    fn download_row_menu_matches_the_original() {
        // buildDownloadItemMenu (app.js:5697): Retry only for Failed/Cancelled,
        // Cancel only while still in flight, and never both.
        assert_eq!(
            labels(&download_row_menu(&dl("Failed", ""))),
            ["Retry", "Open download folder"]
        );
        assert_eq!(
            labels(&download_row_menu(&dl("Cancelled", ""))),
            ["Retry", "Open download folder"]
        );
        assert_eq!(
            labels(&download_row_menu(&dl("Downloading", ""))),
            ["Cancel", "Open download folder"]
        );
        assert_eq!(
            labels(&download_row_menu(&dl("Waiting", ""))),
            ["Cancel", "Open download folder"]
        );
        // A finished row offers neither, and only a file really on disk is deletable.
        assert_eq!(
            labels(&download_row_menu(&dl("Unavailable", ""))),
            ["Open download folder"]
        );
        assert_eq!(
            labels(&download_row_menu(&dl("Downloaded", "C:/m/x.mp3"))),
            ["Open folder", "Delete file"]
        );
        assert_eq!(
            labels(&download_row_menu(&dl("Already Exists", "C:/m/x.mp3"))),
            ["Open folder", "Delete file"]
        );
        // "Deleted" keeps its (now stale) path but must not offer Delete again.
        assert_eq!(
            labels(&download_row_menu(&dl("Deleted", "C:/m/x.mp3"))),
            ["Open folder"]
        );
        // Only the destructive row reads as danger.
        let menu = download_row_menu(&dl("Downloaded", "C:/m/x.mp3"));
        let danger: Vec<&str> = menu.iter().filter(|i| i.danger).map(|i| i.label.as_str()).collect();
        assert_eq!(danger, ["Delete file"]);
    }

    #[test]
    fn download_rows_render_state_and_progress() {
        let rows = build_dl_rows(&[
            dl("Downloading", ""),
            dl("Downloaded", "C:/m/x.mp3"),
            dl("Unavailable", ""),
        ]);
        assert_eq!(rows.iter().map(|r| r.state).collect::<Vec<_>>(), [0, 1, 2]);
        // An item with no name still gets a label rather than an empty row.
        let blank = build_dl_rows(&[downloader::Item::default()]);
        assert_eq!(blank[0].name, "Download item");
        // Progress is clamped into the bar's 0..100 range.
        let over = build_dl_rows(&[downloader::Item { progress: 250, ..Default::default() }]);
        assert_eq!(over[0].progress, 100);
    }

    fn mirror(uris: &[&str], current: &str) -> QueueMirror {
        QueueMirror::new(
            format!("spotify:track:{current}"),
            uris.iter().map(|u| menu_trk(u, true)).collect(),
        )
    }

    #[test]
    fn stale_list_clicks_are_dropped() {
        // Surface 6 (now-playing) never has rows pushed, so this test owns it.
        const S: usize = 6;
        let a = super::list_identity(["x", "y"].into_iter());
        let v1 = super::list_pushed(S, a);
        super::list_shown(S, v1);
        // A cosmetic re-push of the same rows is not a change.
        assert_eq!(super::list_pushed(S, a), v1);
        let click = super::stamp_shown(S, super::Cmd::Drill(1));
        assert!(matches!(super::unstamp(click.clone()), super::Cmd::Drill(1)));
        // The rows change before the worker gets to the click: it must not run.
        let v2 = super::list_pushed(S, super::list_identity(["y", "x"].into_iter()));
        assert_ne!(v1, v2);
        assert!(matches!(super::unstamp(click), super::Cmd::StaleClick));
        // A menu built against the new rows still runs.
        assert!(matches!(super::unstamp(super::stamp_current(S, super::Cmd::Drill(0))), super::Cmd::Drill(0)));
    }

    #[test]
    fn queue_mirror_puts_user_queued_tracks_ahead_of_the_context() {
        let mut m = mirror(&["b", "c", "d"], "a");
        m.enqueue(menu_trk("x", true));
        m.enqueue(menu_trk("y", true));
        assert_eq!(
            m.upcoming.iter().map(|t| t.uri.as_str()).collect::<Vec<_>>(),
            ["spotify:track:x", "spotify:track:y", "spotify:track:b", "spotify:track:c", "spotify:track:d"]
        );
        // The first queued track starting drains only itself, not the context.
        assert!(m.advance("spotify:track:x"));
        assert_eq!(m.upcoming.len(), 4);
        // A later addition still goes ahead of the context, behind "y".
        m.enqueue(menu_trk("z", true));
        assert_eq!(
            m.upcoming.iter().map(|t| t.uri.as_str()).collect::<Vec<_>>(),
            ["spotify:track:y", "spotify:track:z", "spotify:track:b", "spotify:track:c", "spotify:track:d"]
        );
        // Skipping into the context clears the queued segment.
        assert!(m.advance("spotify:track:c"));
        assert_eq!(m.queued, 0);
        m.enqueue(menu_trk("w", true));
        assert_eq!(m.upcoming[0].uri, "spotify:track:w");
    }

    #[test]
    fn queue_mirror_advances_with_playback() {
        // Still on the track it was launched with: nothing moves.
        let mut m = mirror(&["b", "c", "d"], "a");
        assert!(m.advance("spotify:track:a"));
        assert_eq!(m.upcoming.len(), 3);

        // A poll that hasn't resolved a track yet must not disturb it either.
        assert!(m.advance(""));
        assert_eq!(m.upcoming.len(), 3);

        // Playback moved to the next entry: it and everything before it leave.
        assert!(m.advance("spotify:track:b"));
        assert_eq!(
            m.upcoming.iter().map(|t| t.uri.as_str()).collect::<Vec<_>>(),
            ["spotify:track:c", "spotify:track:d"]
        );

        // A jump forward (skipping) drains everything up to the new track, so the
        // panel never shows a track that already played.
        assert!(m.advance("spotify:track:d"));
        assert!(m.upcoming.is_empty());
    }

    #[test]
    fn queue_mirror_gives_up_when_playback_leaves_the_list() {
        // Starting a station / a Beatport chart / a search hit puts on something the
        // mirror has never heard of. That is the signal to drop it and fall back to
        // Spotify's own queue - it is what keeps every other "start playback" path
        // from needing its own cleanup line.
        let mut m = mirror(&["b", "c"], "a");
        assert!(!m.advance("spotify:track:zzz"));
    }

    // ── Queue persistence ───────────────────────────────────────────────────
    fn pq(current: &str, upcoming: &[&str]) -> PersistedQueue {
        PersistedQueue {
            current_uri: current.to_string(),
            upcoming: upcoming.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn queue_restore_point_on_the_exact_captured_track() {
        // Still on the track it was captured behind: the whole list applies.
        let q = pq("a", &["b", "c", "d"]);
        assert_eq!(queue_restore_point(&q, "a"), Some(&["b".to_string(), "c".to_string(), "d".to_string()][..]));
    }

    #[test]
    fn queue_restore_point_after_folding_forward() {
        // Playback had already advanced past the captured track before this ran (a
        // normal poll, or just time spent relaunching) — restore from where we are.
        let q = pq("a", &["b", "c", "d"]);
        assert_eq!(queue_restore_point(&q, "b"), Some(&["c".to_string(), "d".to_string()][..]));
        // On the very last persisted track: nothing left to restore, but that's a
        // real answer (Some(&[])), not "give up".
        assert_eq!(queue_restore_point(&q, "d"), Some(&[][..]));
    }

    #[test]
    fn queue_restore_point_gives_up_off_the_list() {
        // Something entirely different is playing now (another device since, or
        // nothing left to resume) — re-queuing a stale batch behind it would be
        // wrong, so this must say "no restore point", not guess.
        let q = pq("a", &["b", "c"]);
        assert_eq!(queue_restore_point(&q, "zzz"), None);
        // No track known yet (still waiting on the first poll) is the same "don't
        // guess" case, not "restore everything".
        assert_eq!(queue_restore_point(&q, ""), None);
    }

    #[test]
    fn menu_is_at_least_the_css_min_width_and_grows() {
        // `.ctx-menu { min-width: 156px }` — short menus sit exactly there…
        assert_eq!(menu_width(&search_artist_menu(0)), 156.0);
        // …and a long label pushes past it rather than eliding.
        assert!(menu_width(&selection_menu(3)) > 156.0);
    }

    // ── Playback-engine recovery ────────────────────────────────────────────
    fn dev(name: &str, active: bool) -> lightify_core::Device {
        lightify_core::Device {
            id: name.into(),
            name: name.into(),
            is_active: active,
            kind: "Computer".into(),
            volume_percent: 50,
        }
    }

    #[test]
    fn engine_exit_tells_a_takeover_from_a_lost_audio_device() {
        // The user started playing on their phone / the Spotify desktop app: another
        // device is active, so coming back would steal playback from them.
        assert!(another_device_active(
            &[dev("Lightify", false), dev("Jon\u{2019}s iPhone", true)],
            "Lightify"
        ));
        // Our own device being active is not a takeover.
        assert!(!another_device_active(&[dev("Lightify", true)], "Lightify"));
        // The audio-device-lost case: playback just stopped, nothing else is active.
        assert!(!another_device_active(&[dev("Lightify", false), dev("Desktop", false)], "Lightify"));
        // ...including when Spotify lists nothing at all.
        assert!(!another_device_active(&[], "Lightify"));
    }

    #[test]
    fn engine_restarts_are_budgeted_and_the_budget_refills() {
        use std::time::Instant;
        let t0 = Instant::now();
        let mut w = super::EngineWatchdog::new();
        // Three tries inside the window, then it stops asking.
        assert!(w.should_restart(t0));
        assert!(w.should_restart(t0));
        assert!(w.should_restart(t0));
        assert!(!w.should_restart(t0));
        // Past the window the budget refills, so a once-an-hour failure always recovers.
        let later = t0 + super::EngineWatchdog::WINDOW + std::time::Duration::from_secs(1);
        assert!(w.should_restart(later));
        // An engine that comes up healthy clears the run entirely.
        w.note_healthy();
        for _ in 0..super::EngineWatchdog::MAX_RESTARTS {
            assert!(w.should_restart(later));
        }
    }

    // ── Selection scoping ───────────────────────────────────────────────────
    #[test]
    fn sel_key_follows_the_sort_view_only_for_the_track_list() {
        // The drilled list is sortable, so a display row maps to its source index…
        let view = vec![4usize, 2, 7];
        assert_eq!(sel_key(SEL_TRACKS, 0, &view), Some(4));
        assert_eq!(sel_key(SEL_TRACKS, 2, &view), Some(7));
        assert_eq!(sel_key(SEL_TRACKS, 3, &view), None);
        // …the other lists are unsorted, so the row IS the key.
        for scope in [SEL_SEARCH, SEL_BEATPORT, SEL_SIDEBAR] {
            assert_eq!(sel_key(scope, 2, &view), Some(2));
        }
    }

    #[test]
    fn scoped_selection_uris_reads_the_right_surface() {
        let mk = |id: &str, playable: bool| Track {
            id: id.into(),
            name: id.into(),
            artists: String::new(),
            artist_ids: vec![],
            album: String::new(),
            album_id: String::new(),
            duration_ms: 0,
            uri: format!("spotify:track:{id}"),
            album_art: String::new(),
            added_at: None,
            is_playable: playable,
        };
        let tracks = vec![mk("a", true), mk("b", false), mk("c", true)];
        let view = vec![0usize, 1, 2];
        let actions = vec![
            SearchAction::Track { uri: "spotify:track:s1".into(), id: "s1".into() },
            SearchAction::Artist { id: "artist1".into(), name: "Artist One".into() },
            SearchAction::Track { uri: "spotify:track:s2".into(), id: "s2".into() },
        ];
        let side = vec![mk("q1", true), mk("q2", true)];
        let drill = vec![mk("d1", true), mk("d2", true)];
        let sel: std::collections::BTreeSet<usize> = [0usize, 1, 2].into_iter().collect();

        // Track list: the unplayable row is dropped for playback…
        assert_eq!(
            scoped_selection_uris(SEL_TRACKS, &sel, &view, &tracks, &actions, &side, &drill, true),
            ["spotify:track:a", "spotify:track:c"]
        );
        // …but kept when building a playlist.
        assert_eq!(
            scoped_selection_uris(SEL_TRACKS, &sel, &view, &tracks, &actions, &side, &drill, false).len(),
            3
        );
        // Search: only track hits contribute (the artist row has no uri).
        assert_eq!(
            scoped_selection_uris(SEL_SEARCH, &sel, &view, &tracks, &actions, &side, &drill, true),
            ["spotify:track:s1", "spotify:track:s2"]
        );
        // Sidebar reads its own list.
        let side_sel: std::collections::BTreeSet<usize> = [1usize].into_iter().collect();
        assert_eq!(
            scoped_selection_uris(SEL_SIDEBAR, &side_sel, &view, &tracks, &actions, &side, &drill, true),
            ["spotify:track:q2"]
        );
        // The search tab's own drill-in reads its own list.
        let drill_sel: std::collections::BTreeSet<usize> = [0usize].into_iter().collect();
        assert_eq!(
            scoped_selection_uris(SEL_SEARCH_DRILL, &drill_sel, &view, &tracks, &actions, &side, &drill, true),
            ["spotify:track:d1"]
        );
        // Beatport rows need a Spotify match first, so they never come back here.
        assert!(scoped_selection_uris(SEL_BEATPORT, &sel, &view, &tracks, &actions, &side, &drill, true).is_empty());
        assert!(scoped_selection_uris(SEL_NONE, &sel, &view, &tracks, &actions, &side, &drill, true).is_empty());
    }

    // ── Search rework ──────────────────────────────────────────────────────

    mod search {
        use super::super::{
            build_recents, build_search, dedupe_results, pick_top_result, search_match_rank, SearchAction,
        };
        use lightify_core::{Album, Artist, SearchResults, Track};

        fn t(id: &str, name: &str, artists: &str, artist_ids: &[&str]) -> Track {
            Track {
                id: id.into(),
                name: name.into(),
                artists: artists.into(),
                artist_ids: artist_ids.iter().map(|s| s.to_string()).collect(),
                album: String::new(),
                album_id: String::new(),
                duration_ms: 200_000,
                uri: format!("spotify:track:{id}"),
                album_art: String::new(),
                added_at: None,
                is_playable: true,
            }
        }
        fn a(id: &str, name: &str) -> Artist {
            Artist { id: id.into(), name: name.into(), followers: 0, uri: String::new(), image: String::new(), thumb: String::new() }
        }
        fn al(name: &str, artists: &str) -> Album {
            Album { id: name.into(), name: name.into(), artists: artists.into(), tracks: 10, uri: String::new(), image: String::new(), thumb: String::new() }
        }
        fn top(r: &SearchResults, q: &str) -> Option<(&'static str, usize)> {
            let (tracks, albums) = dedupe_results(r);
            let order = super::super::artists_by_presence(r, q);
            pick_top_result(r, &tracks, &albums, &order, q)
        }

        #[test]
        fn match_rank_orders_exact_prefix_word_contains() {
            assert_eq!(search_match_rank("Toby", "toby"), 0);
            assert_eq!(search_match_rank("Toby Keith", "toby"), 1);
            assert_eq!(search_match_rank("Best of Toby Fox", "toby"), 2);
            assert_eq!(search_match_rank("Tobytown", "oby"), 3);
            assert_eq!(search_match_rank("Robyn", "toby"), 4);
            assert_eq!(search_match_rank("anything", "  "), 4);
        }

        #[test]
        fn top_result_prefers_the_artist_on_the_top_songs() {
            // Live 2026-09-26: Spotify listed Toby Romeo first, but Toby Keith is on
            // the top songs — and an album titled exactly "Toby" must not win either.
            let r = SearchResults {
                artists: vec![a("romeo", "Toby Romeo"), a("fox", "Toby Fox"), a("keith", "Toby Keith")],
                tracks: vec![
                    t("1", "Courtesy Of The Red, White And Blue", "Toby Keith", &["keith"]),
                    t("2", "Tobey", "Eminem", &["em"]),
                    t("3", "As Good As I Once Was", "Toby Keith", &["keith"]),
                    t("4", "Death By Glamour", "Toby Fox", &["fox"]),
                ],
                albums: vec![al("Toby", "The Chi-Lites")],
                ..Default::default()
            };
            assert_eq!(top(&r, "Toby"), Some(("artist", 2)));
            // The Artists section follows the same order: Keith, Fox, then Romeo.
            assert_eq!(super::super::artists_by_presence(&r, "toby"), vec![2, 1, 0]);
        }

        #[test]
        fn top_result_prefers_a_matching_song_over_an_unknown_namesake_artist() {
            let r = SearchResults {
                artists: vec![a("band", "Midnight City"), a("m83", "M83")],
                tracks: vec![t("1", "Midnight City", "M83", &["m83"])],
                ..Default::default()
            };
            assert_eq!(top(&r, "midnight city"), Some(("track", 0)));
            // ...while the namesake still wins when no song matches.
            let r = SearchResults { artists: vec![a("band", "Midnight City")], ..Default::default() };
            assert_eq!(top(&r, "midnight city"), Some(("artist", 0)));
        }

        #[test]
        fn duplicate_songs_collapse_and_all_view_has_sections() {
            let r = SearchResults {
                artists: vec![a("em", "Eminem")],
                tracks: vec![
                    t("1", "Tobey", "Eminem, Big Sean", &["em"]),
                    t("2", "Tobey", "Eminem, Big Sean", &["em"]), // the clean cut
                    t("3", "Lose Yourself", "Eminem", &["em"]),
                ],
                ..Default::default()
            };
            let (tracks, _) = dedupe_results(&r);
            assert_eq!(tracks.len(), 2);
            let (rows, actions) = build_search(&r, "all", "eminem");
            assert_eq!(rows.len(), actions.len());
            let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
            // Eminem is the Top result, so no Artists section is left to show.
            assert_eq!(kinds, ["top", "header", "track", "track"]);
            assert_eq!(rows[0].meta.as_str(), "artist");
            assert!(matches!(&actions[1], SearchAction::Header { see_all } if see_all == "tracks"));
            assert_eq!(rows[2].meta.as_str(), "3:20"); // duration
            // A single filter is the plain (deduped) list.
            let (rows, _) = build_search(&r, "tracks", "eminem");
            assert_eq!(rows.iter().filter(|r| r.kind == "track").count(), 2);
        }

        #[test]
        fn recents_view() {
            assert!(build_recents(&[]).0.is_empty());
            let (rows, actions) = build_recents(&["toby".into(), "m83".into()]);
            assert_eq!(rows[0].kind.as_str(), "header");
            assert_eq!(rows[0].meta.as_str(), "Clear");
            assert!(matches!(actions[0], SearchAction::ClearRecents));
            assert!(matches!(&actions[2], SearchAction::Recent(q) if q == "m83"));
        }
    }

    // ── Shuffle / repeat ───────────────────────────────────────────────────

    mod options {
        use super::super::{apply_options_hold, next_repeat_mode, release_options_hold, OptionsHold, OPTIONS_HOLD};
        use lightify_core::PlaybackState;

        fn pb(shuffle: bool, repeat: &str) -> PlaybackState {
            PlaybackState {
                is_playing: true,
                progress_ms: 0,
                duration_ms: 1,
                shuffle_state: shuffle,
                repeat_state: repeat.into(),
                volume_percent: 50,
                device_name: String::new(),
                device_id: String::new(),
                track: None,
                context_uri: None,
            }
        }

        #[test]
        fn repeat_cycles_off_all_one() {
            assert_eq!(next_repeat_mode("off"), "context");
            assert_eq!(next_repeat_mode("context"), "track");
            assert_eq!(next_repeat_mode("track"), "off");
            assert_eq!(next_repeat_mode("weird"), "off");
        }

        #[test]
        fn a_lagging_poll_cannot_undo_a_fresh_choice() {
            release_options_hold();
            *OPTIONS_HOLD.lock().unwrap() = Some(OptionsHold {
                shuffle: Some(true),
                repeat: Some("track".into()),
                until: std::time::Instant::now() + std::time::Duration::from_secs(6),
            });
            // The Web API still reports the old state: the held choice wins.
            let mut p = pb(false, "off");
            apply_options_hold(&mut p);
            assert!(p.shuffle_state);
            assert_eq!(p.repeat_state, "track");
            // A poll that caught up releases each field it agrees with...
            let mut p = pb(true, "off");
            apply_options_hold(&mut p);
            assert_eq!(p.repeat_state, "track");
            assert!(OPTIONS_HOLD.lock().unwrap().as_ref().is_some_and(|h| h.shuffle.is_none()));
            let mut p = pb(true, "track");
            apply_options_hold(&mut p);
            assert!(OPTIONS_HOLD.lock().unwrap().is_none());
            // ...after which later changes (from a phone, say) come straight through.
            let mut p = pb(false, "context");
            apply_options_hold(&mut p);
            assert!(!p.shuffle_state);
            assert_eq!(p.repeat_state, "context");
            // An expired hold never overrides.
            *OPTIONS_HOLD.lock().unwrap() = Some(OptionsHold {
                shuffle: Some(true),
                repeat: None,
                until: std::time::Instant::now() - std::time::Duration::from_secs(1),
            });
            let mut p = pb(false, "off");
            apply_options_hold(&mut p);
            assert!(!p.shuffle_state);
            release_options_hold();
        }
    }

    // ── Station epoch + queue clearing ─────────────────────────────────────

    mod station_lane {
        use super::super::{
            cancel_pending_station, clear_queue_command, next_station_epoch, station_epoch_is_current,
        };
        use lightify_core::{PlaybackState, Track};

        fn pb(uri: &str, pos: u64, playing: bool) -> Option<PlaybackState> {
            Some(PlaybackState {
                is_playing: playing,
                progress_ms: pos,
                duration_ms: 200_000,
                shuffle_state: false,
                repeat_state: "off".into(),
                volume_percent: 50,
                device_name: String::new(),
                device_id: String::new(),
                track: Some(Track {
                    id: "x".into(),
                    name: "n".into(),
                    artists: "a".into(),
                    artist_ids: vec![],
                    album: String::new(),
                    album_id: String::new(),
                    duration_ms: 200_000,
                    uri: uri.into(),
                    album_art: String::new(),
                    added_at: None,
                    is_playable: true,
                }),
                context_uri: None,
            })
        }

        /// The one test that touches the process-wide epoch, so parallel tests can't race it.
        #[test]
        fn a_late_answer_for_an_abandoned_station_is_recognised() {
            // Station A is requested...
            let a = next_station_epoch();
            assert!(station_epoch_is_current(a));
            // ...the user plays something else (which cancels it) before it resolves.
            cancel_pending_station();
            assert!(!station_epoch_is_current(a), "A's answer must be dropped now");
            // Station B is requested; only B's answer counts, whichever arrives first.
            let b = next_station_epoch();
            assert!(station_epoch_is_current(b));
            assert!(!station_epoch_is_current(a));
            // A second request supersedes the first outright.
            let c = next_station_epoch();
            assert!(!station_epoch_is_current(b));
            assert!(station_epoch_is_current(c));
            // The wire default (a request without an epoch) is never the live one.
            assert!(!station_epoch_is_current(0));
        }

        #[test]
        fn clear_command_keeps_the_song_only_when_asked_and_possible() {
            let playing = pb("spotify:track:abc", 12_345, true);
            let v: serde_json::Value = serde_json::from_str(&clear_queue_command(&playing, true)).unwrap();
            assert_eq!(v["cmd"], "clearqueue");
            assert_eq!(v["keep_uri"], "spotify:track:abc");
            assert_eq!(v["position_ms"], 12_345);
            assert_eq!(v["playing"], true);
            // Paused stays paused.
            let v: serde_json::Value =
                serde_json::from_str(&clear_queue_command(&pb("spotify:track:abc", 5, false), true)).unwrap();
            assert_eq!(v["playing"], false);
            // Not asked to keep it, nothing playing, or a track with no uri: the bare reset.
            for cmd in [
                clear_queue_command(&playing, false),
                clear_queue_command(&None, true),
                clear_queue_command(&pb("", 0, true), true),
            ] {
                let v: serde_json::Value = serde_json::from_str(&cmd).unwrap();
                assert_eq!(v["cmd"], "clearqueue");
                assert!(v.get("keep_uri").is_none(), "bare reset carries no track: {cmd}");
            }
        }
    }

    #[test]
    fn beatport_start_plays_a_single_track() {
        // With two, the second becomes the play context's next track and the queued rest of
        // the chart jumps ahead of it (queue read 3, 4, 5, 6, then 2). See BP_INITIAL_MATCH.
        assert_eq!(super::BP_INITIAL_MATCH, 1);
    }
}
