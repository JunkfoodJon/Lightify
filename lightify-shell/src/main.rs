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
mod local_backdrop;
mod local_library;
mod local_player;
mod local_ui;
mod engine;
mod play_history;
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
mod window_geom;

use std::rc::Rc;
use std::time::Duration;

use lightify_core::{BeatportTrack, Device, PlaybackState, SearchResults, Session, Track};
// Every string Rust shows goes through `tr!` (LANGUAGE-PLAN.md).
use lightify_i18n::tr;
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
    /// Open a library playlist by id (the command palette lists every playlist, also
    /// ones a type-to-filter over the library is hiding).
    DrillId(String),
    /// Self-sent: read the library again (it failed at launch, a re-sign-in, a save).
    ReloadLibrary,
    /// Self-sent by the engine-adoption task once Spotify lists our new device (or
    /// gave up): what `Event::Ready` used to wait for inline, up to ~7 s.
    EngineAdopted { device_id: String, result: Result<String, String> },
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
    /// Self-sent by `bp_fetch`'s task: the chart for `genre`/`kind`, from fetch `seq`.
    BpFetched { seq: u64, genre: usize, kind: String, result: Result<Vec<BeatportTrack>, String> },
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
        /// The failing row's strike count, see `bp_retry_row`.
        strikes: u32,
    },
    /// Self-sent by `bp_match_background`: the next chart rows matched on Spotify,
    /// to append to the chart list playing on our engine (see `BpList`).
    BpMatched {
        seq_id: u64,
        tracks: Vec<Track>,
        /// Chart rows (of the matcher's source) looked up so far, found or not.
        next_index: usize,
        done: bool,
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
    /// Forget the player's own Spotify sign-in and run it again (Settings).
    StreamingSignInAgain,
    SelectOutput(usize), // pick the engine's audio output (row 0 = system default)
    /// Self-sent once a downloader sign-in has really started (the bridge reports one
    /// in progress). Picking "OnTheSpot" for it can knock our engine off; only an
    /// engine knocked off that way, inside `DL_LOGIN_RECLAIM_WINDOW`, is brought back
    /// (see `engine::Event::Exited`). It never starts a stopped engine by itself.
    DownloaderLoginStarted,
    // ── Row context menus (`.ctx-menu`) ──
    OpenContext { kind: i32, index: usize }, // kind 0 = library row, 1 = drilled track
    RunContext(usize),        // run the menu row at this index
    PlayRow(usize),           // play a whole library row (double-click / menu "Play")
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
    /// The backdrop menu's "ASCII lava screensaver".
    LavaSaver,
    // ── Track-list multi-select ──
    /// Ctrl+click toggles one row; Shift+click extends from the last clicked row.
    /// `scope` is the surface the row belongs to (see `SEL_*`); selecting in a new
    /// surface drops the old selection, like the original's per-container scopes.
    SelectClick { scope: i32, row: usize, ctrl: bool, shift: bool },
    /// Hold-to-select: a press held past the hold time starts a drag on `row`; each
    /// drag is ADDED to whatever is already selected (gaps between groups allowed).
    SelectDragBegin { scope: i32, row: usize },
    /// The drag's pointer, as a y in the list's content (the host maps it to a row).
    SelectDragTo { scope: i32, y: f32 },
    SelectDragEnd,
    ClearSelection,
    PlaySelection,
    QueueSelection,
    CreatePlaylistFromSelection,
    /// The name prompt for a new playlist: the name typed, or dismissed.
    PlaylistNamed(String),
    PlaylistNameCancelled,
    /// Everything the Local library view does (`local_ui`).
    Local(local_ui::LocalCmd),
    // ── Search result menus ──
    QueueSearchTrack(usize),
    LikeSearchTrack(usize),
    ShareSearchTrack(usize),
    PlaySearchContext(usize), // play a playlist/album hit from its first track
    QueueSearchHit(usize),    // queue a playlist/album hit's tracks, or an artist's top songs
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
    /// RECENT is open and a track just finished: re-read recently-played.
    RefreshRecentSidebar,
    // ── Sidebar menus ──
    SidebarQueueRow(usize),
    ShareSidebarTrack(usize),
    SidebarPlayFirstSelected,
    // ── Type-to-filter (D7): the visible left list's filter text ──
    ListFilter(String),
    // ── Search: Enter in the box opens the top result ──
    SearchEnter,
    /// Settings › language switched: say everything the worker put on screen again
    /// in the new language (`switch_language`).
    Relabel,
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
    let mut args: Vec<String> = std::env::args().collect();
    // `--lang <code>`: this run's language over the saved one, for renders and testing
    // (`--lang qps-ploc` in a LIGHTIFY_PSEUDO build); never saved. Taken out of `args`
    // so the modes below see their own arguments where they expect them.
    if let Some(pos) = args.iter().position(|a| a.eq_ignore_ascii_case("--lang")) {
        let code = args.drain(pos..(pos + 2).min(args.len())).nth(1).unwrap_or_default();
        // The Rust half straight away: a shot builds some of its text before the window.
        lightify_i18n::select(LANGS, &code);
        let _ = LANG_ARG.set(code);
    }
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
    if let Some(pos) = args.iter().position(|a| a == "--shot-artfull") {
        // --shot-artfull <out.png>: like --shot-live, with the cover double-clicked open
        // (filling the pane behind the controls).
        let path = args.get(pos + 1).cloned().unwrap_or_else(|| "shot-artfull.png".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_snapshot());
        SHOT_ART_FULL.store(true, std::sync::atomic::Ordering::Relaxed);
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
        // --shot-beatport <kind> <out.png> [genre index]  (kind = tracks | hype | releases)
        let kind = args.get(pos + 1).cloned().unwrap_or_else(|| "tracks".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-beatport.png".into());
        let genre = args.get(pos + 3).and_then(|g| g.parse::<usize>().ok()).unwrap_or(0);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_beatport_snapshot(&kind, genre));
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
        // --shot-search-drill <query> <out.png> [artist]  (default: first playlist hit)
        let query = args.get(pos + 1).cloned().unwrap_or_else(|| "m83".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-search-drill.png".into());
        let artist = args.get(pos + 3).is_some_and(|a| a == "artist");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let snap = rt.block_on(fetch_search_drill_snapshot(&query, artist));
        render_to_png(&path, 980, 660, Some(snap));
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-ctx") {
        // --shot-ctx <kind> <out.png>  (kind = library | liked | track | confirm | selection |
        //   nowplaying | search | search-album | search-playlist | search-artist | bp | sidebar)
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
        // `--shot-settings <png> account-problem`: the engine card as a user with a Free
        // or mismatched player sign-in sees it (this account has neither to show).
        if args.get(pos + 2).map(String::as_str) == Some("free-account") {
            SHOT_FREE_PLAN.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if args.get(pos + 2).map(String::as_str) == Some("account-problem") {
            if let Ok(mut g) = SHOT_ACCOUNT_PROBLEM.lock() {
                *g = streaming_account_problem("someone-else", "premium", "premium", "you");
            }
        }
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
    if args.iter().any(|a| a == "--selftest-bp-chart") {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(selftest_bp_chart());
        engine::shutdown();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-long-lists") {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(selftest_long_lists());
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
    if args.iter().any(|a| a == "--probe-start") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(probe_start());
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
    if let Some(pos) = args.iter().position(|a| a == "--selftest-language") {
        // --selftest-language [code]: every bundled language, or just that one.
        selftest_language(args.get(pos + 1).map(String::as_str));
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-lava") {
        selftest_lava();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-lava-fullscreen") {
        selftest_lava_fullscreen();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-local-hold") {
        selftest_local_hold();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest-hold") {
        selftest_hold();
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-nameprompt") {
        // --shot-nameprompt <left|right> <out.png>: the new-playlist name prompt open
        // over the left panel (a selection) or the queue drawer (sample data).
        let side = if args.get(pos + 1).map(|s| s.as_str()) == Some("right") { 1 } else { 0 };
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-nameprompt.png".into());
        SHOT_NAME_PROMPT.store(side, std::sync::atomic::Ordering::Relaxed);
        render_to_png(&path, 980, 660, None);
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-signin") {
        // --shot-signin <out.png> [WxH]: the sign-in page (sample data underneath).
        let path = args.get(pos + 1).cloned().unwrap_or_else(|| "shot-signin.png".into());
        let (w, h) = args
            .get(pos + 2)
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .unwrap_or((980u32, 660u32));
        SHOT_SIGNIN.store(true, std::sync::atomic::Ordering::Relaxed);
        render_to_png(&path, w, h, None);
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-update") {
        // --shot-update <downloading|available|ready> <out.png> [closed] [WxH]: Settings ›
        // updates in that state (sample data); `closed` leaves Settings shut to show the
        // footer cog asking for attention instead.
        let state = args.get(pos + 1).cloned().unwrap_or_else(|| "downloading".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-update.png".into());
        let closed = args.get(pos + 3).map(|s| s.as_str()) == Some("closed");
        // [WxH] last, after `closed` when both are given.
        let (w, h) = args
            .iter()
            .skip(pos + 3)
            .find_map(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .unwrap_or((980u32, 660u32));
        if let Ok(mut g) = SHOT_UPDATE.lock() {
            *g = if closed { format!("{state}:closed") } else { state };
        }
        render_to_png(&path, w, h, None);
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-lava") {
        // --shot-lava <native|menu|saver|settings|settings-lang|off>[@secs] <out.png> [WxH]: the
        // ASCII lava behind the Spotify now-playing pane (sample data), its right-click
        // menu, the screensaver, Settings › ascii lava, or switched off.
        let variant = args.get(pos + 1).cloned().unwrap_or_else(|| "native@4".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-lava.png".into());
        let (w, h) = args
            .get(pos + 3)
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .unwrap_or((980u32, 660u32));
        if let Ok(mut g) = SHOT_LAVA.lock() {
            *g = variant;
        }
        render_to_png(&path, w, h, None);
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-local") {
        // --shot-local <library|find|drop|carry|empty>[@secs] <out.png> [WxH]: Local
        // library mode (sample data).
        let variant = args.get(pos + 1).cloned().unwrap_or_else(|| "library".into());
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-local.png".into());
        let (w, h) = args
            .get(pos + 3)
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .unwrap_or((980u32, 660u32));
        if let Ok(mut g) = SHOT_LOCAL.lock() {
            *g = variant;
        }
        render_to_png(&path, w, h, None);
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-bpqueue") {
        // --shot-bpqueue <lining|done> <out.png>: the QUEUE panel while a Beatport chart
        // lines up (found rows, the one being looked up, the rest waiting), or once
        // it has (sample data).
        let done = args.get(pos + 1).map(|s| s.as_str()) == Some("done");
        let path = args.get(pos + 2).cloned().unwrap_or_else(|| "shot-bpqueue.png".into());
        SHOT_BP_QUEUE.store(if done { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
        render_to_png(&path, 980, 660, None);
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
    // The language, for Slint's strings and ours alike; Slint wants the window first.
    apply_language(&language_in_use());
    push_languages(&app, &language_in_use());
    cjk_font_fallback();
    #[cfg(windows)]
    single_instance::listen(app.as_weak());
    app.set_app_version(env!("CARGO_PKG_VERSION").into());
    app.set_ui_font(ui_font().into());
    app.global::<Motion>().set_reduced(reduce_motion());

    // Recall the last mini-player layout for the titlebar toggle (persisted on disk).
    app.set_mini_last(load_mini_mode().into());

    // Reopen the window as it was: its normal size and place, then maximized on top of
    // that if it was (so un-maximizing returns to the normal rect, not the default).
    // Set before the window exists, so it never flashes at the default first.
    restore_window_geometry(&app);
    track_window_geometry(&app);

    // Self-update: background check + download, "Restart" in the footer.
    install_updater(&app);

    // UI → worker channel.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    local_ui::install(&app, tx.clone());
    local_backdrop::install(&app);

    {
        let weak = app.as_weak();
        app.on_close_clicked(move || {
            if let Some(a) = weak.upgrade() {
                capture_window_geometry(&a);
            }
            let _ = slint::quit_event_loop();
        });
    }
    {
        // Resize from the edges and corners of the frameless window (winit
        // `drag_resize_window`: the OS runs the drag, so it is as smooth as a framed one).
        let weak = app.as_weak();
        app.on_start_resize(move |dir| {
            use slint::winit_030::winit::window::ResizeDirection as D;
            use slint::winit_030::WinitWindowAccessor;
            let d = match dir {
                0 => D::North,
                1 => D::South,
                2 => D::West,
                3 => D::East,
                4 => D::NorthWest,
                5 => D::NorthEast,
                6 => D::SouthWest,
                _ => D::SouthEast,
            };
            if let Some(a) = weak.upgrade() {
                a.window().with_winit_window(|w| {
                    let _ = w.drag_resize_window(d);
                });
            }
        });
    }
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
        // The main titlebar: a press starts a native window move — unless it is the
        // second press of a double-click (Windows' own time and distance limits), which
        // the UI turns into maximize / restore. The move loop eats the click events, so
        // the double-click can't come from the UI's own `double-clicked`.
        let weak = app.as_weak();
        let last: Rc<std::cell::Cell<Option<(std::time::Instant, f32, f32)>>> = Rc::default();
        app.on_titlebar_press(move |x, y| {
            let Some(a) = weak.upgrade() else { return false };
            let (limit, slop) = double_click_limits(a.window().scale_factor());
            let now = std::time::Instant::now();
            if is_double_press(last.take(), (now, x, y), limit, slop) {
                return true;
            }
            last.set(Some((now, x, y)));
            use slint::winit_030::WinitWindowAccessor;
            a.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
            false
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
        let (tx_drill, tx_play) = (tx.clone(), tx.clone());
        wire_open_row(
            &app,
            move |i| {
                let _ = tx_drill.send(stamp_shown(0, Cmd::Drill(i)));
            },
            move |j| {
                let _ = tx_play.send(stamp_shown(0, Cmd::PlayRow(j)));
            },
        );
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
        app.on_engine_sign_in_again(move || {
            let _ = tx.send(Cmd::StreamingSignInAgain);
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
        let weak = app.as_weak();
        app.on_settings_select_language(move |i| {
            let (Some(a), Some(setting)) = (weak.upgrade(), language_choices().get(i.max(0) as usize).cloned()) else {
                return;
            };
            switch_language(&a, &setting, &tx);
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
            // Local mode's RECENT is its own list; bring its "5m ago" times up to date.
            if m == 2 {
                let _ = tx.send(Cmd::Local(local_ui::LocalCmd::RecentRefresh));
            }
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
        // flag lives in the UI and is set here directly, exactly as the titlebar wordmark does.
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
        let w = app.as_weak();
        app.on_signin_copy_redirect(move || match copy_to_clipboard(lightify_core::auth::REDIRECT_URI) {
            Ok(()) => {
                if let Some(a) = w.upgrade() {
                    a.set_signin_copied(true);
                }
                let w = w.clone();
                slint::Timer::single_shot(Duration::from_millis(1500), move || {
                    if let Some(a) = w.upgrade() {
                        a.set_signin_copied(false);
                    }
                });
            }
            Err(e) => eprintln!("[lightify] copy redirect URI: {e}"),
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
                    spawn_media_key_listener(app.as_weak(), tx);
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
                    spawn_media_key_listener(app.as_weak(), tx);
                    None
                }
            };
            let sync = move |a: &MainWindow| {
                if let Some(mc) = mc.as_ref() {
                    mc.sync(a);
                }
                // The player on screen: Local's song while it is the one "now".
                taskbar::set_playing(a.get_mini_playing());
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
    spawn_media_key_listener(app.as_weak(), tx.clone());
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
        app.on_sel_drag_begin(move |scope, row| {
            let cmd = Cmd::SelectDragBegin { scope, row: row.max(0) as usize };
            let _ = tx.send(if (0..LIST_SURFACES as i32).contains(&scope) { stamp_shown(scope as usize, cmd) } else { cmd });
        });
    }
    {
        let tx = tx.clone();
        app.on_sel_drag_move(move |scope, y| {
            let cmd = Cmd::SelectDragTo { scope, y };
            let _ = tx.send(if (0..LIST_SURFACES as i32).contains(&scope) { stamp_shown(scope as usize, cmd) } else { cmd });
        });
    }
    {
        let tx = tx.clone();
        app.on_sel_drag_end(move || {
            let _ = tx.send(Cmd::SelectDragEnd);
        });
    }
    {
        let tx = tx.clone();
        app.on_name_prompt_accept(move |name| {
            let _ = tx.send(Cmd::PlaylistNamed(name.to_string()));
        });
    }
    {
        let tx = tx.clone();
        app.on_name_prompt_cancel(move || {
            let _ = tx.send(Cmd::PlaylistNameCancelled);
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
    // From here this instance can no longer bring its window forward: a launch during
    // the shutdown below waits for it to finish instead of vanishing (`claim`).
    #[cfg(windows)]
    _single.closing();
    // A quit from the tray menu skips the close paths that flush the window record;
    // whatever the last settled capture saw goes to disk here.
    window_geom::save_if_changed();
    // The worker isn't unwound either: the resume point it last saved can be up to
    // `RESUME_SAVE_EVERY` behind the music. Where playback had got to goes to disk now.
    save_exit_resume();
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
    // The Local library view: its state, and Local mode as the app last closed. Up
    // before the session, because Local needs no Spotify sign-in at all: it keeps
    // working while the sign-in page waits (`SignInWait`).
    let mut local = local_ui::LocalState::default();
    if local_ui::load_mode() {
        local_ui::handle(&mut local, local_ui::LocalCmd::ModeChanged(true), &weak, &self_tx);
    }
    let Some(mut session) = obtain_session(&weak, &mut SignInWait::new(&mut rx, &self_tx, &mut local)).await else {
        return;
    };
    set_account(&weak, connected_status(&session));
    // Signed in: drop "Connecting…"; the titlebar pill is for notices only.
    set_status(&weak, String::new());

    // Bring up our OWN Spotify Connect device before anything else. Lightify is a
    // player, not a remote: without this the Web API has no device to target and
    // every transport call dead-ends in "no active device" (which the UI used to
    // render as "open Spotify first" — the exact dependency this app exists to
    // remove). The engine reuses the shipped host's cached streaming credentials,
    // so this is silent.
    start_engine(&weak, &session, &self_tx);
    // A Spotify Free account is told, once, that the player and the downloader need
    // Premium. Nothing is blocked or changed (owner, 2026-11-05): only said.
    if let Ok(product) = session.account_product().await {
        show_plan_notes(&weak, &product);
    }

    // When to read the library again, while it hasn't loaded once this run. A launch
    // offline, rate-limited or during a Spotify 5xx used to leave the library empty (or
    // the cached copy) for the whole session.
    let mut library_retry_at: Option<tokio::time::Instant> = None;
    let mut playlists = match session.playlists().await {
        Ok(p) => {
            remember_library(&p);
            thumbs::request(&weak, &p);
            p
        }
        Err(e) => {
            set_status(&weak, tr!("Playlists error — {}", e));
            library_retry_at = Some(tokio::time::Instant::now() + LIBRARY_RETRY_EVERY);
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
    // `lightify_core::play_requests()` as of the engine's last track event: a change
    // means the next track is a load we asked for (see `restore_repeat_after_load`).
    let mut seen_play_requests = lightify_core::play_requests();
    // Guards `restore_queue` to run once per launch — the engine can come back up
    // again later (the watchdog restarting it after a lost sink), and re-queuing
    // the same persisted tracks a second time would duplicate them.
    let mut queue_restored = false;
    // The engine came up during a rate limit: the saved queue is restored after the
    // first read that works, instead of being skipped (and later overwritten).
    let mut queue_restore_pending = false;
    // The saved queue as it was at launch, for that one restore. Read now: by the time a
    // held-back restore runs, a new play has rewritten the file with its own list, and
    // restoring that queued the list's own songs a second time (BUGS-PLAN QD2).
    let mut launch_queue = load_queue();
    // When a Local song last started while Local wasn't playing (`local_player`): a
    // Spotify `Playing(true)` that arrives just after is the engine catching up, not a
    // new Spotify play, and must not stop that song.
    let mut local_started_at: Option<std::time::Instant> = None;
    // What was playing when the engine died on its own (context, track, position),
    // picked up again once the watchdog's restart registers (`Event::Ready`).
    let mut resume_after_restart: Option<(Option<String>, String, u64)> = None;
    // When it was noted (see `RESUME_AFTER_RESTART_FOR`).
    let mut resume_after_restart_at = std::time::Instant::now();
    // When a downloader sign-in last started (`Cmd::DownloaderLoginStarted`).
    let mut dl_login_at: Option<std::time::Instant> = None;

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
    // Where each single-type filter's paging stood for these results: pages land in the
    // shared results, so a filter left and come back to carries on from its own offset.
    let mut sp_paged: std::collections::HashMap<&'static str, (u32, bool)> = std::collections::HashMap::new();
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
    // Bumped by every chart fetch: a `Cmd::BpFetched` from an older one is dropped.
    let mut bp_fetch_seq: u64 = 0;
    let mut bp_tracks: Vec<BeatportTrack> = Vec::new();
    // Live Beatport chart autoplay, if a chart row started playback.
    let mut bp_autoplay: Option<BpAutoplay> = None;
    // The same, when the chart plays as one growing list on our own engine.
    let mut bp_list: Option<BpList> = None;
    // A station being resolved to follow a song played from search: (epoch, song uri).
    let mut search_station: Option<(u64, String)> = None;
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
    // What Shift+click extends from, as a selection key (`sel_key`: the track's index
    // in the track list, so a re-sort doesn't move it; the display row elsewhere).
    let mut sel_anchor: Option<usize> = None;
    // (scope, list version) the current selection was made against — see the check
    // at the bottom of the loop.
    let mut sel_version: (i32, u64) = (SEL_NONE, 0);
    // A live hold-to-select drag: what was selected before it began (each drag adds
    // to that), and the row it started on.
    let mut drag_base: Option<std::collections::BTreeSet<usize>> = None;
    let mut drag_from: usize = 0;
    let mut drag_last: usize = 0;
    // "Create playlist" waits on the name prompt: the command to finish once named.
    let mut pending_create: Option<Cmd> = None;
    // What was selected when the prompt opened (uris, rows, scope). The list under it
    // can change while the user types (a track change re-reads the QUEUE, which drops
    // its selection), and the playlist must still get what they picked.
    let mut pending_sel: Option<(Vec<String>, std::collections::BTreeSet<usize>, i32)> = None;

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
        set_status(&weak, tr!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
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
    // Re-reads left while the Web API catches up with a track the engine already
    // named (a natural track end reaches `me/player` a second or two late, and the
    // one read `Event::Track` triggers would otherwise leave the old cover up for a
    // whole poll interval).
    let mut track_catchup: u8 = 0;
    // Set when `Event::Track` updated `last` itself (no poll): the ticker then runs
    // its after-a-poll work (RECENT re-read, Beatport refill) once.
    let mut track_hook_due = false;
    // The QUEUE panel asked to re-read while a read was already out: run one more
    // when that one lands, instead of discarding it (see `Cmd::RefreshQueueSidebar`).
    let mut queue_refetch = false;
    // The track RECENT last saw playing; a change means the one before it just
    // finished and belongs at the top of the list.
    let mut recent_seen_track = String::new();
    // Spotify's last recently-played answer (other devices' plays), kept to re-merge
    // with Lightify's own history whenever a play of ours counts.
    let mut recent_spotify: Vec<Track> = Vec::new();
    // Local progress clock — see `ProgressClock`.
    let mut clock = poll_interval(PROGRESS_CLOCK);
    clock.tick().await;
    let mut progress_clock = ProgressClock::default();
    // When the clock last saved the resume point (see `RESUME_SAVE_EVERY`).
    let mut resume_saved_at = std::time::Instant::now();

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
    const QUEUE_ENGINE_QUIET: Duration = Duration::from_secs(60);
    let mut queue_ticker = poll_interval(QUEUE_POLL);
    let mut dl_ticker = poll_interval(QUEUE_POLL);
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
        // Spotify refused the refresh token (`invalid_grant`): every request from here
        // on would POST another refresh that can only fail. Back to the sign-in page,
        // which waits without polling; the engine and Local mode carry on meanwhile.
        if take_revoked_sign_in() {
            let mut wait = SignInWait::new(&mut rx, &self_tx, &mut local);
            let Some(s) = obtain_session(&weak, &mut wait).await else { break };
            session = s;
            session.remember_device(&engine_device_id);
            set_account(&weak, connected_status(&session));
            next_playback_poll = tokio::time::Instant::now();
            // The sign-in can be another account's, and the library read while the
            // old one was failing may be stale or empty.
            let _ = self_tx.send(Cmd::ReloadLibrary);
        }
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
                        set_status(&weak, tr!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
                    }
                    // Still the previous track per the Web API? Read again shortly —
                    // the cover, duration and liked state all come from this read.
                    if track_catchup > 0 && backoff_until.is_none() && !engine_track_name.is_empty() {
                        if !LAST_POLL_STALE.load(std::sync::atomic::Ordering::Relaxed) {
                            track_catchup = 0;
                        } else {
                            track_catchup -= 1;
                            poll_soon = Some(now + Duration::from_millis(600));
                        }
                    }
                    next_playback_poll = now
                        + playback_poll_every(
                            Duration::from_millis(poll_ms),
                            &last,
                            engine_owns_playback(&last, &engine_device_id),
                        );
                }
            }
            // DOWNLOADS while open: the bridge is local, so it can follow progress (and
            // a just-added download) every 1.5 s instead of once per playback poll.
            _ = dl_ticker.tick(), if sidebar_mode == 3 => {
                spawn_downloads_refresh(&weak);
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
                // The resume point keeps up with the music between reads (`save_resume`
                // itself skips a write when the position barely moved).
                if last.as_ref().is_some_and(|p| p.is_playing) && resume_saved_at.elapsed() >= RESUME_SAVE_EVERY {
                    resume_saved_at = std::time::Instant::now();
                    if let Some(pb) = progress_clock.live(&last) {
                        save_resume(&pb);
                    }
                }
                // ...and the shutdown after `app.run()` saves where it got to by then.
                note_exit_resume(&last);
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
                        set_status(&weak, tr!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
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
                        set_status(&weak, tr!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(remaining)));
                    } else {
                        backoff_until = None;
                        set_status(&weak, String::new());
                    }
                }
                // (Downloads are re-read by `dl_ticker` alone while that panel is open; a
                // second read here doubled every `/api/download_queue` request.)
                // The library never loaded this run: try again, unless Spotify is out of
                // reach or rate-limiting (then the next tick after it lifts does).
                if library_retry_at.is_some_and(|t| now >= t)
                    && backoff_until.is_none()
                    && !lightify_core::net::is_offline()
                    && lightify_core::ratelimit::background_ok()
                {
                    library_retry_at = Some(now + LIBRARY_RETRY_EVERY);
                    let _ = self_tx.send(Cmd::ReloadLibrary);
                }
                let poll_due = backoff_until.is_none() && now >= next_playback_poll;
                // An engine track change applied without a poll (`Event::Track`) still
                // owes the work below that follows a new track (RECENT, Beatport refill).
                if poll_due || (track_hook_due && backoff_until.is_none()) {
                    track_hook_due = false;
                    let polled = if poll_due {
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
                            // Held until the next try or the reconnect: it is a state,
                            // not a notice, and used to vanish after 5 s while still offline.
                            set_status_held(&weak, tr!("Offline \u{2014} reconnecting (next try in {})", fmt_backoff(net_probe.as_secs())));
                            net_probe = (net_probe * 2).min(NET_PROBE_MAX);
                            was_offline = true;
                        } else if was_offline {
                            was_offline = false;
                            net_probe = NET_PROBE_MIN;
                            set_status(&weak, String::new());
                            // Back online: a library that never loaded is read now.
                            if library_retry_at.is_some() {
                                library_retry_at = Some(now);
                            }
                        }
                        polled
                    } else {
                        None
                    };
                    match polled {
                        Some(secs) => {
                            backoff_until = Some(now + Duration::from_secs(secs));
                            set_status(&weak, tr!("Spotify rate limit \u{2014} pausing polling for {}", fmt_backoff(secs)));
                        }
                        None => {
                            // The engine came up during a rate limit: the saved queue it
                            // held back goes on now that reads work again.
                            if poll_due
                                && queue_restore_pending
                                && !queue_restored
                                && engine_owns_playback(&last, &engine_device_id)
                            {
                                queue_restore_pending = false;
                                queue_restored = true;
                                restore_queue(&weak, &mut session, &last, &mut queue_mirror, launch_queue.take()).await;
                            }
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
                                        let strikes = seq.strikes;
                                        let tx2 = self_tx.clone();
                                        let active_seq = bp_active_seq.clone();
                                        tokio::spawn(async move {
                                            let result = bp_refill_background(
                                                source, next_index, seen, seq_id, active_seq, strikes,
                                            )
                                            .await;
                                            let _ = tx2.send(Cmd::BpRefillDone {
                                                seq_id,
                                                queued: result.0,
                                                seen: result.1,
                                                next_index: result.2,
                                                completed: result.3,
                                                error: result.4,
                                                strikes: result.5,
                                            });
                                        });
                                    }
                                    Some(_) => {}
                                }
                            }
                            // QUEUE (1) has its own `queue_ticker`; DOWNLOADS (3) was
                            // refreshed above. RECENT (2) re-marks locally — and when the
                            // playing track changed, the one before it just finished:
                            // re-read the list a few seconds later (Spotify adds it to
                            // recently-played with a short lag), so it shows up without
                            // closing and reopening the panel.
                            if sidebar_mode == 2 {
                                // A play of ours just counted: it's in the list right away.
                                if play_history::take_changed() {
                                    sidebar_tracks = play_history::merged(&recent_spotify);
                                }
                                let cur = current_id(&last);
                                if cur != recent_seen_track {
                                    if !recent_seen_track.is_empty() {
                                        let tx = self_tx.clone();
                                        tokio::spawn(async move {
                                            tokio::time::sleep(Duration::from_secs(4)).await;
                                            let _ = tx.send(Cmd::RefreshRecentSidebar);
                                        });
                                    }
                                    recent_seen_track = cur;
                                }
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
                        set_status(&weak, tr!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(remaining)));
                        // Hold the query and run it the moment the limit lifts, rather
                        // than silently dropping what the user typed.
                        pending_query = Some(q);
                        search_deadline = Some(until);
                    } else {
                        let searched = match cached_search(&q) {
                            Some(hit) => Ok(hit),
                            None => {
                                let r = session.search(&q).await;
                                if let Ok(res) = &r {
                                    cache_search(&q, res);
                                }
                                r
                            }
                        };
                        set_search_loading(&weak, false);
                        match searched {
                            Ok(res) => {
                                last_query = q.clone();
                                search_results = res;
                                let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                                search_actions = actions;
                                search_rows = rows.clone();
                                sp_paged.clear();
                                reset_playlist_pager(
                                    &weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done, &sp_paged,
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
                                    let until = tokio::time::Instant::now() + Duration::from_secs(secs);
                                    backoff_until = Some(until);
                                    set_status(&weak, tr!("Spotify rate limit \u{2014} pausing for {}", fmt_backoff(secs)));
                                    // Held, as above: it runs again once the limit lifts.
                                    pending_query = Some(q);
                                    search_deadline = Some(until);
                                } else {
                                    set_status(&weak, tr!("Search \u{2014} {}", e));
                                }
                            }
                        }
                    }
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                let local_was = local_player::active();
                let Some(cmd) = route_local(unstamp(cmd), &mut local, &weak, &self_tx) else {
                    if !local_was && local_player::active() {
                        local_started_at = Some(std::time::Instant::now());
                    }
                    continue;
                };
                // "Create playlist" asks for the name first — an in-theme prompt over the
                // panel it was started from — and finishes when `PlaylistNamed` comes
                // back. With nothing to save it falls through to the usual message.
                let mut named: Option<String> = None;
                let mut named_uris: Option<Vec<String>> = None;
                let cmd = match cmd {
                    c @ (Cmd::CreatePlaylistFromSelection | Cmd::BpCreatePlaylist | Cmd::SidebarCreatePlaylist)
                        if match &c {
                            Cmd::SidebarCreatePlaylist => !sidebar_tracks.is_empty(),
                            Cmd::BpCreatePlaylist => sel_scope == SEL_BEATPORT && !selected.is_empty(),
                            _ => !selected.is_empty(),
                        } =>
                    {
                        let (side, note) = match &c {
                            Cmd::SidebarCreatePlaylist => (1, name_note_queue(sidebar_tracks.len())),
                            _ => (if sel_scope == SEL_SIDEBAR { 1 } else { 0 }, name_note_selected(selected.len())),
                        };
                        let _ = weak.upgrade_in_event_loop(move |a| {
                            a.set_name_prompt_side(side);
                            a.set_name_prompt_note(note.into());
                            a.set_name_prompt_open(true);
                        });
                        let uris = scoped_selection_uris(
                            sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, false,
                        );
                        pending_sel = Some((uris, selected.clone(), sel_scope));
                        pending_create = Some(c);
                        continue;
                    }
                    Cmd::PlaylistNamed(name) => {
                        let name = name.trim().to_string();
                        if name.is_empty() && pending_create.is_some() {
                            // Only spaces: keep the prompt up rather than closing it on
                            // nothing.
                            let _ = weak.upgrade_in_event_loop(|a| {
                                a.set_name_prompt_note(tr!("Give the playlist a name").into());
                                a.set_name_prompt_open(true);
                            });
                            continue;
                        }
                        match pending_create.take() {
                            Some(c) => {
                                named = Some(name);
                                if let Some((uris, sel, scope)) = pending_sel.take() {
                                    if selected.is_empty() && matches!(c, Cmd::BpCreatePlaylist) {
                                        // Beatport rows don't move with playback, so the
                                        // picked rows are still the picked tracks.
                                        selected = sel;
                                        sel_scope = scope;
                                    }
                                    named_uris = Some(uris);
                                }
                                c
                            }
                            None => continue,
                        }
                    }
                    Cmd::PlaylistNameCancelled => {
                        pending_create = None;
                        pending_sel = None;
                        continue;
                    }
                    other => other,
                };
                match cmd {
                    Cmd::Stamped { .. } => unreachable!("unstamp opens every stamp"),
                    Cmd::StaleClick => {
                        set_status(&weak, tr!("That list just changed \u{2014} try again"));
                    }
                    Cmd::Relabel => {
                        // The library list (Liked Songs, "N tracks"), and the open list's
                        // title when it is Liked Songs.
                        push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                        if drilled && drill_src.as_deref() == Some(LIKED_SOURCE) {
                            set_drill(&weak, true, tr!("Liked Songs"));
                        }
                        // Search: section heads, "See all", the result lines — rebuilt from
                        // what is held. Only when the rebuild is the same rows (a page
                        // still arriving would make it something else).
                        let recents = matches!(search_actions.first(), Some(SearchAction::ClearRecents));
                        let (rows, actions) = if recents {
                            build_recents(&load_recent_searches())
                        } else {
                            build_search(&search_results, &search_filter, &last_query)
                        };
                        if !search_rows.is_empty() && rows.len() == search_rows.len() {
                            drop_selection(&weak, SEL_SEARCH, &mut selected, &mut sel_anchor, &mut sel_scope);
                            search_actions = actions;
                            search_rows = rows.clone();
                            push_search(&weak, rows);
                        }
                        // QUEUE / RECENT: the "5m" times and the empty-panel line.
                        if sidebar_mode == 1 || sidebar_mode == 2 {
                            drop_selection(&weak, SEL_SIDEBAR, &mut selected, &mut sel_anchor, &mut sel_scope);
                            let empty = if sidebar_mode == 1 { tr!("No queued tracks") } else { tr!("No recent tracks yet") };
                            let rows = build_side_rows(&sidebar_tracks, &current_id(&last), sidebar_mode == 2);
                            set_sidebar(&weak, sidebar_mode, rows, empty);
                        } else if sidebar_mode == 3 {
                            spawn_downloads_refresh(&weak);
                        }
                        // Settings, open while the language changed: its Rust-made lines.
                        if devices_open(&weak) {
                            set_account(&weak, connected_status(&session));
                            push_outputs(&weak);
                            spawn_downloader_status(&weak);
                            push_devices(&weak, build_device_rows(&devices));
                        }
                        relabel_settings_lines(&weak);
                        // An open prompt: the name prompt's note, the delete confirmation.
                        let note = match (&pending_create, &pending_sel) {
                            (Some(Cmd::SidebarCreatePlaylist), _) => Some(name_note_queue(sidebar_tracks.len())),
                            (Some(_), Some((_, sel, _))) => Some(name_note_selected(sel.len())),
                            _ => None,
                        };
                        let confirm = match &pending_confirm {
                            Some(Cmd::DeletePlaylist(_, name)) => {
                                Some(tr!("Delete \u{201c}{}\u{201d} from your Spotify library?", name))
                            }
                            _ => None,
                        };
                        let _ = weak.upgrade_in_event_loop(move |a| {
                            if let (Some(n), true) = (note, a.get_name_prompt_open()) {
                                a.set_name_prompt_note(n.into());
                            }
                            if let (Some(m), true) = (confirm, a.get_confirm_open()) {
                                a.set_confirm_title(tr!("DELETE PLAYLIST").into());
                                a.set_confirm_message(m.into());
                            }
                        });
                        local_ui::handle(&mut local, local_ui::LocalCmd::Relabel, &weak, &self_tx);
                    }
                    // Only meaningful on the sign-in page (`obtain_session`).
                    Cmd::SignInAuthorise(_) | Cmd::SignInRetry(_) | Cmd::SignInOpenDashboard => {}
                    Cmd::TogglePlay if last.is_none() && resume_offered().is_some() => {
                        // Play on the resume offer: that track, in its context, from
                        // where it stopped, on our own device.
                        if let Some(rp) = take_resume_offer() {
                            set_status(&weak, tr!("Resuming {}\u{2026}", rp.name));
                            // On our own engine when it is up: a local load, not the Web
                            // API's round trip through Spotify to this same device.
                            let played = match rp.context_uri.as_deref() {
                                Some(c) if engine_play_context(c, &rp.track_uri, rp.position_ms, &last, &engine_device_id) => Ok(()),
                                None => play_track_list(
                                    &mut session, std::slice::from_ref(&rp.track_uri), 0, rp.position_ms, &last, &engine_device_id,
                                )
                                .await
                                .map(|_| ()),
                                ctx => session.play_at(ctx, &rp.track_uri, rp.position_ms).await,
                            };
                            match played {
                                // The offer stays up, so Play can try again.
                                Err(e) => {
                                    put_back_resume_offer(rp);
                                    set_status(&weak, tr!("Resume \u{2014} {}", e));
                                }
                                // Playing (the read below shows it): "Resuming…" is done.
                                Ok(()) => set_status(&weak, String::new()),
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            // The launch's saved queue was held for this (see `Event::Ready`).
                            if !queue_restored && last.is_some() && !engine_device_id.is_empty() {
                                queue_restored = true;
                                restore_queue(&weak, &mut session, &last, &mut queue_mirror, launch_queue.take()).await;
                            }
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
                            if let Err(e) = r { set_status(&weak, tr!("Playback — {}", e)); }
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
                            // Skipping forward out of repeat-one leaves repeat-all, as in
                            // Spotify: librespot clears repeat-one on a skip (repeat-all
                            // stays on underneath, see the engine's `Repeat`) and doesn't
                            // report it. Only *next* does that — "previous" keeps
                            // repeat-one, so this can't be inferred from a track change.
                            if last.as_ref().is_some_and(|p| p.repeat_state == "track") {
                                show_options(&weak, &mut last, None, Some("context".to_string()));
                                set_repeat_choice("context");
                            }
                        } else {
                            if let Err(e) = session.next().await {
                                if is_player_restriction(&e) {
                                    // Spotify said "not allowed" but our device can still
                                    // advance itself; don't surface a raw 403 for that.
                                    engine::send(&serde_json::json!({ "cmd": "next" }).to_string());
                                } else {
                                    set_status(&weak, tr!("Next \u{2014} {}", e));
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
                                    set_status(&weak, tr!("Prev \u{2014} {}", e));
                                }
                            }
                            settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        }
                    }
                    Cmd::DrillId(id) => {
                        // From the palette, which lists every playlist: a filter typed over
                        // the library may hide this one, so it goes first (as `Drill` drops
                        // it anyway), then the row is the playlist's place in the full list.
                        if !filter_of(&PL_FILTER).is_empty() {
                            clear_list_filters(&weak);
                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                            push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                        }
                        match pl_view.iter().position(|&k| playlists.get(k).is_some_and(|p| p.id == id)) {
                            Some(row) => {
                                let _ = self_tx.send(Cmd::Drill(row + 1));
                            }
                            None => set_status(&weak, tr!("That list just changed \u{2014} try again")),
                        }
                    }
                    Cmd::ReloadLibrary => match session.playlists().await {
                        Ok(p) => {
                            library_retry_at = None;
                            playlists = p;
                            remember_library(&playlists);
                            thumbs::request(&weak, &playlists);
                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                            push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                        }
                        // Still out of reach: the retry timer (if the library never
                        // loaded) tries again; a loaded library just stays as it is.
                        Err(e) => eprintln!("[lightify] library reload: {e}"),
                    },
                    Cmd::Drill(i) => {
                        drop_track_selection(&weak, &mut selected, &mut sel_anchor, &mut sel_scope, &trk_view);
                        // `i` is the *display* row index, so map it back through the sort view.
                        let src = if i == 0 { None } else { pl_view.get(i - 1).copied() };
                        // ...then drop a filter typed over the library (mapped first: the
                        // index is into the filtered view). It doesn't follow into a playlist.
                        if !filter_of(&PL_FILTER).is_empty() {
                            clear_list_filters(&weak);
                            pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                            push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                        }
                        // A track filter (the palette drills from inside a playlist) goes
                        // too, pill and all: the data alone left "x matches" on screen.
                        if !filter_of(&TRK_FILTER).is_empty() {
                            clear_list_filters(&weak);
                        }
                        if i == 0 {
                            // Liked Songs (saved tracks, no context).
                            drilled = true;
                            drill_src = Some(LIKED_SOURCE.to_string());
                            set_drill(&weak, true, tr!("Liked Songs"));
                            set_status(&weak, tr!("Loading Liked Songs\u{2026}"));
                            push_tracks(&weak, &[], &[]);
                            push_sort(&weak, &trk_mode, &trk_dir);
                            tracks.clear();
                            context = None;
                            // Fetched while the first page loads, so a click on any row
                            // doesn't first wait for the whole list (see `prepare_context`).
                            if let Some(ctx) = liked_context(&session) {
                                prepare_context(&ctx);
                            }
                            pager = TrackPager::start(PagerSource::Liked, &tr!("Liked Songs"));
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
                            set_status(&weak, tr!("Loading {}\u{2026}", name));
                            push_tracks(&weak, &[], &[]);
                            push_sort(&weak, &trk_mode, &trk_dir);
                            tracks.clear();
                            context = if uri.is_empty() { None } else { Some(uri) };
                            if let Some(ctx) = &context {
                                prepare_context(ctx);
                            }
                            pager = TrackPager::start(PagerSource::Playlist(id.clone()), &name);
                            load_next_track_page(
                                &weak, &mut session, &mut pager, &mut tracks, &mut trk_view,
                                &trk_mode, &trk_dir, &presence,
                            )
                            .await;
                        }
                    }
                    Cmd::Back => {
                        drop_track_selection(&weak, &mut selected, &mut sel_anchor, &mut sel_scope, &trk_view);
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
                            set_status(&weak, tr!("That track is no longer in the list"));
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
                            // Warm the cover while the play request is out, so the
                            // refresh after it usually finds it ready.
                            prefetch_art(&t.album_art, &last_art);
                            // No context (Liked Songs): the whole list as displayed,
                            // from this row — on our own engine, with no length cap.
                            // Liked Songs in its own (newest-first) order is a real
                            // context the engine resolves whole — every saved track
                            // follows, not just the pages scrolled into view so far.
                            // Sorted A–Z or filtered, it's the list as shown instead.
                            let liked_ctx = (pager.source == PagerSource::Liked
                                && trk_mode != "alpha"
                                && filter_of(&TRK_FILTER).is_empty())
                                .then(|| liked_context(&session))
                                .flatten();
                            // A context plays in its own order, whatever order is on screen.
                            let mut context_play = true;
                            let r = if let Some(ctx) = context.clone() {
                                // On our engine when it's up, like a list play: it resolves
                                // the context itself (no Web-API call, and no repeat to
                                // restore afterwards — the load carries it).
                                if engine_play_context(&ctx, &t.uri, 0, &last, &engine_device_id) {
                                    Ok(true)
                                } else {
                                    session.play_context(&ctx, &t.uri).await.map(|()| true)
                                }
                            } else if liked_ctx.as_deref().is_some_and(|c| {
                                engine_play_context(c, &t.uri, 0, &last, &engine_device_id)
                            }) {
                                Ok(true)
                            } else {
                                context_play = false;
                                let uris: Vec<String> = trk_view
                                    .iter()
                                    .filter_map(|&k| tracks.get(k))
                                    .map(|x| x.uri.clone())
                                    .collect();
                                play_track_list(&mut session, &uris, i, 0, &last, &engine_device_id).await
                            };
                            // Up-next is everything BELOW this row as displayed. A
                            // context play continues in the playlist's own order, which
                            // is not the sorted order on screen, so the panel has to
                            // mirror what the user is looking at.
                            if let Ok(whole) = r {
                                // A Web-API fallback only sent `WEB_API_URIS_MAX` uris, so
                                // only those can come next — listing the rest of a long
                                // Liked Songs list here would show tracks that never play.
                                let upcoming: Vec<Track> = trk_view[i + 1..]
                                    .iter()
                                    .filter_map(|&k| tracks.get(k))
                                    .take(if whole { usize::MAX } else { WEB_API_URIS_MAX - 1 })
                                    .cloned()
                                    .collect();
                                let previous = current_uri(&last);
                                // Shuffled, the display order isn't what plays next —
                                // for a context play as much as a list play; Spotify's
                                // own queue (fetched by the panel) is.
                                let shuffled = last.as_ref().is_some_and(|p| p.shuffle_state);
                                // A context continues in the playlist's own order: the rows
                                // below this one are what comes next only when the list is
                                // shown in that order (no re-sort, no filter). Otherwise the
                                // panel reads Spotify's real queue.
                                let in_order = !context_play || view_is_source_order(&trk_view);
                                queue_mirror = (!shuffled && in_order).then(|| {
                                    QueueMirror::new(t.uri.clone(), upcoming)
                                        .keeping_queued(queue_mirror.as_ref())
                                        .replacing(&previous)
                                });
                                // A queue fetch dispatched before this play must not land
                                // on top of the mirror it just replaced.
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                                // Show it now rather than on the panel's next tick.
                                if sidebar_mode == 1 {
                                    if let Some(m) = queue_mirror.as_ref() {
                                        sidebar_tracks = m.upcoming.clone();
                                        set_sidebar(
                                            &weak, 1, build_side_rows(&sidebar_tracks, &t.id, false),
                                            tr!("No queued tracks"),
                                        );
                                    } else {
                                        spawn_delayed_queue_refresh(&self_tx);
                                    }
                                }
                            }
                            // As for a whole playlist (`PlayRow`): only a play that started.
                            match r {
                                Ok(_) => launched_src = drill_src.clone(),
                                Err(e) => set_status(&weak, tr!("Play — {}", e)),
                            }
                            refresh_soon(&mut poll_soon);
                        }
                    }
                    Cmd::Search(q) => {
                        let q = q.trim().to_string();
                        if q.is_empty() {
                            pending_query = None;
                            search_deadline = None;
                            search_results = SearchResults::default();
                            search_actions.clear();
                            // Nothing to page under the recent searches: a scroll to the
                            // bottom must not ask Spotify for `q=` (empty).
                            sp_paged.clear();
                            sp_done = true;
                            let _ = weak.upgrade_in_event_loop(|app| app.set_search_more(false));
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
                            // Long enough that a word typed at normal speed is one
                            // search, not one per pause between letters.
                            search_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_millis(450));
                        }
                    }
                    Cmd::SearchFilter(f) => {
                        search_filter = f;
                        // No query yet: the list is the recent searches, under any chip
                        // (filtering "no results" left it blank until something was typed).
                        let (rows, actions) = if last_query.is_empty() {
                            build_recents(&load_recent_searches())
                        } else {
                            build_search(&search_results, &search_filter, &last_query)
                        };
                        search_actions = actions;
                        search_rows = rows.clone();
                        reset_playlist_pager(
                            &weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done, &sp_paged,
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
                        if let Some(action) = action {
                            match action {
                                SearchAction::Header { .. } | SearchAction::Recent(_) | SearchAction::ClearRecents => {}
                                SearchAction::Track { uri, .. } => {
                                    // Only a play ends a chart lining up; opening a
                                    // playlist, album or artist below just browses.
                                    bp_autoplay = None;
                                    bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                                    end_autoplay_authority(
                                        &weak, &mut session, &mut station_active, &mut station_queued,
                                        &mut bp_autoplay, &mut bp_queued,
                                        &bp_active_seq, &last, &engine_device_id,
                                    )
                                    .await;
                                    // A one-off track has no library source behind it.
                                    launched_src = None;
                                    // Songs queued by hand still play next (a load spares
                                    // the user queue); only without any does a station
                                    // follow, so the music doesn't just stop after one song.
                                    let hand_queued = queue_mirror.as_ref().is_some_and(|m| m.queued > 0);
                                    // The panel follows this song, not the list before it: the
                                    // old mirror showed that list's remainder as up next
                                    // (BUGS-PLAN QD7). Spotify's read fills it until the
                                    // station lands.
                                    queue_mirror = None;
                                    sidebar_generation += 1;
                                    persist_mirror(&queue_mirror);
                                    match play_track_list(&mut session, std::slice::from_ref(&uri), 0, 0, &last, &engine_device_id).await {
                                        Ok(true) if !hand_queued => {
                                            // Resolved in the background and appended
                                            // behind the song (`Event::StationTracks`): the
                                            // song starts at once, nothing it plays into
                                            // is interrupted, and no queue requests are made.
                                            let epoch = next_station_epoch();
                                            engine::send(
                                                &serde_json::json!({ "cmd": "stationtracks", "context_uri": uri, "seq": epoch })
                                                    .to_string(),
                                            );
                                            search_station = Some((epoch, uri.clone()));
                                        }
                                        Ok(_) => {}
                                        Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
                                    }
                                    refresh_soon(&mut poll_soon);
                                }
                                // Both stay in the search tab (`#search-drill`), which
                                // is what the original does — the library list is a
                                // different container and must not be disturbed.
                                SearchAction::Playlist { id, uri, name } => {
                                    s_tracks.clear();
                                    s_context = if uri.is_empty() { None } else { Some(uri) };
                                    if let Some(ctx) = &s_context {
                                        prepare_context(ctx);
                                    }
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
                                    if let Some(ctx) = &s_context {
                                        prepare_context(ctx);
                                    }
                                    set_search_drill(&weak, true, name);
                                    // Albums are short and the API serves them whole, so
                                    // there is nothing to page here.
                                    s_pager = TrackPager::default();
                                    match session.album_tracks(&id).await {
                                        Ok(t) => {
                                            s_tracks = t;
                                            push_search_tracks(&weak, &s_tracks);
                                            set_status(&weak, String::new());
                                        }
                                        Err(e) => set_status(&weak, tr!("Album \u{2014} {}", e)),
                                    }
                                }
                                SearchAction::Artist { id, name } => {
                                    s_tracks.clear();
                                    // No real context behind a synthesized artist
                                    // track list — playing from it falls back to a
                                    // plain uri list, same as Liked Songs.
                                    s_context = None;
                                    set_search_drill(&weak, true, name.clone());
                                    // The artist's discography, a page of releases at a
                                    // time as the list scrolls (`artists/{id}/top-tracks`
                                    // 403s for this client id, and an artist-name track
                                    // search mostly returns other artists).
                                    s_pager = TrackPager::start(PagerSource::Artist(id), &name);
                                    load_next_search_page(
                                        &weak, &mut session, &mut s_pager, &mut s_tracks,
                                    )
                                    .await;
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
                        let cur = load_shuffle(&last);
                        let want = !cur;
                        show_options(&weak, &mut last, Some(want), None);
                        let result = if last.is_none() {
                            // Nothing loaded: nothing to tell. The next play carries it.
                            set_shuffle_choice(want);
                            Ok(())
                        } else if engine_owns_playback(&last, &engine_device_id) {
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
                                set_status(&weak, tr!("Shuffle \u{2014} {}", e));
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
                            // own device is told directly, as the host does. `from` lets
                            // it touch only the flag that changes.
                            engine::send(&serde_json::json!({ "cmd": "repeat", "mode": next, "from": cur }).to_string());
                            Ok(())
                        } else {
                            session.set_repeat(next).await
                        };
                        match result {
                            Ok(()) => {
                                set_repeat_choice(next);
                                poll_soon = Some(tokio::time::Instant::now() + Duration::from_millis(2500));
                            }
                            Err(e) => {
                                release_options_hold();
                                show_options(&weak, &mut last, None, Some(cur));
                                release_options_hold();
                                set_status(&weak, tr!("Repeat \u{2014} {}", e));
                            }
                        }
                    }
                    Cmd::AddQueue => {
                        if let Some(t) = last.as_ref().and_then(|p| p.track.clone()) {
                            let uri = t.uri.clone();
                            match queue_one(&mut session, &uri, engine_owns_playback(&last, &engine_device_id)).await {
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
                                    set_status(&weak, tr!("Added to queue"));
                                }
                                Err(e) => set_status(&weak, tr!("Queue — {}", e)),
                            }
                            if sidebar_mode == 1 {
                                refresh_sidebar(&weak, &self_tx, sidebar_mode, &mut sidebar_tracks, &last, &mut queue_mirror, &mut sidebar_generation).await;
                            }
                        }
                    }
                    Cmd::SearchEnter => 'enter: {
                        // Enter in the search box: open the first real result — the Top
                        // result in the All view. (Runs a still-pending query first.)
                        if let Some(q) = pending_query.take() {
                            // As the debounce does: through a rate limit, the query is
                            // held for when it lifts, and nothing stale is opened.
                            if let Some(until) = backoff_until.filter(|u| *u > tokio::time::Instant::now()) {
                                let remaining = (until - tokio::time::Instant::now()).as_secs() + 1;
                                set_status(&weak, tr!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(remaining)));
                                pending_query = Some(q);
                                search_deadline = Some(until);
                                break 'enter;
                            }
                            search_deadline = None;
                            let searched = match cached_search(&q) {
                                Some(hit) => Ok(hit),
                                None => {
                                    let r = session.search(&q).await;
                                    if let Ok(res) = &r {
                                        cache_search(&q, res);
                                    }
                                    r
                                }
                            };
                            set_search_loading(&weak, false);
                            match searched {
                                Ok(res) => {
                                    last_query = q.clone();
                                    search_results = res;
                                    let (rows, actions) = build_search(&search_results, &search_filter, &last_query);
                                    search_actions = actions;
                                    search_rows = rows.clone();
                                    sp_paged.clear();
                                    reset_playlist_pager(&weak, &search_filter, &search_rows, &mut sp_offset, &mut sp_done, &sp_paged);
                                    // New results replace a drilled-into hit of the old query.
                                    exit_search_drill(
                                        &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                                        &mut s_tracks, &mut s_context, &mut s_pager,
                                    );
                                    drop_selection(&weak, SEL_SEARCH, &mut selected, &mut sel_anchor, &mut sel_scope);
                                    push_search(&weak, rows);
                                }
                                // The rows on screen belong to the previous query: opening
                                // one would play something the user didn't ask for. Keep
                                // what they typed; Enter again (or the limit lifting) runs it.
                                Err(e) => {
                                    if let Some(secs) = rate_limit_backoff(&e) {
                                        let until = tokio::time::Instant::now() + Duration::from_secs(secs);
                                        backoff_until = Some(until);
                                        search_deadline = Some(until);
                                        set_status(&weak, tr!("Spotify rate limit \u{2014} pausing for {}", fmt_backoff(secs)));
                                    } else {
                                        set_status(&weak, tr!("Search \u{2014} {}", e));
                                    }
                                    pending_query = Some(q);
                                    break 'enter;
                                }
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
                        drop_track_selection(&weak, &mut selected, &mut sel_anchor, &mut sel_scope, &trk_view);
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
                        let note = if drilled { filter_loaded_note(&pager, tracks.len()) } else { String::new() };
                        let _ = weak.upgrade_in_event_loop(move |app| {
                            app.set_list_filter_count(shown);
                            app.set_list_filter_note(note.into());
                        });
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
                                remark_tracks(&weak, sel_scope, &selected, &trk_view);
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
                            remark_tracks(&weak, sel_scope, &selected, &trk_view);
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
                            set_status(&weak, tr!("No track playing"));
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
                                        if target { tr!("Saved to Liked Songs") } else { tr!("Removed from Liked Songs") },
                                    );
                                }
                                Err(e) => {
                                    push_liked(&weak, liked.on, false);
                                    set_status(&weak, tr!("Like — {}", e));
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
                                    set_status(&weak, tr!("Seek — {}", e));
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
                            set_status(&weak, tr!("Volume \u{2014} {}", e));
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
                        // Another panel's rows must not stand in while this one loads
                        // (an open panel's refresh keeps its rows up until the answer).
                        if target != sidebar_mode {
                            sidebar_tracks.clear();
                        }
                        if target == 2 {
                            recent_seen_track = current_id(&last);
                        }
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
                                    // Lightify's own plays show at once; Spotify's (other
                                    // devices) merge in when they arrive.
                                    sidebar_generation += 1;
                                    sidebar_tracks = play_history::merged(&recent_spotify);
                                    let pid = current_id(&last);
                                    set_sidebar(&weak, 2, build_side_rows(&sidebar_tracks, &pid, true), tr!("Loading recent\u{2026}"));
                                    spawn_sidebar_fetch(&self_tx, 2, sidebar_generation);
                                }
                                _ => {
                                    // Downloads: rows come from the bridge, not Spotify, so
                                    // this pushes an empty track model and fills the separate
                                    // `sidebar-dl-rows` asynchronously.
                                    sidebar_tracks.clear();
                                    set_sidebar(&weak, 3, vec![], tr!("Loading downloads\u{2026}"));
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
                        // exactly like `PlayRow` does.
                        let target = if row == 0 {
                            downloader::spotify_url("liked", "").ok().map(|u| (u, tr!("Liked Songs")))
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
                            None => set_status(&weak, tr!("Nothing to download on that row")),
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
                            None => set_status(&weak, tr!("Nothing to download on that row")),
                        }
                    }
                    Cmd::DownloadBeatportRow(row) => {
                        // A Beatport row is a Beatport track: resolve it to a Spotify
                        // track first, the same scored match `BpPlay`/`BpQueue` use.
                        if let Some(bt) = bp_tracks.get(row).cloned() {
                            set_status(&weak, tr!("Matching \u{201c}{}\u{201d} on Spotify\u{2026}", bt.name));
                            match session.beatport_match(&bt.name, &bt.artists).await {
                                Ok(Some(t)) => match downloader::spotify_url("track", &t.id) {
                                    Ok(url) => spawn_enqueue_download(&weak, &self_tx, url, t.name.clone()),
                                    Err(e) => set_status(&weak, e),
                                },
                                Ok(None) => set_status(&weak, tr!("No Spotify match to download for {}", bt.name)),
                                Err(e) => set_status(&weak, tr!("Beatport \u{2014} {}", e)),
                            }
                        }
                    }
                    Cmd::RefreshDownloads => {
                        // `ensureDownloadsSidebar` (app.js:5429): open the panel on
                        // DOWNLOADS if it isn't already, otherwise just re-read.
                        if sidebar_mode != 3 {
                            sidebar_mode = 3;
                            sidebar_tracks.clear();
                            set_sidebar(&weak, 3, vec![], tr!("Loading downloads\u{2026}"));
                        }
                        spawn_downloads_refresh(&weak);
                    }
                    Cmd::ClearDownloads => {
                        spawn_download_action(
                            &weak,
                            true,
                            tr!("Cleared finished downloads"),
                            downloader::clear_finished(),
                        );
                    }
                    Cmd::DownloadRetry(id) => {
                        if downloader::last_snapshot().iter().any(|i| i.local_id == id) {
                            spawn_download_action(
                                &weak,
                                true,
                                tr!("Download queued for retry"),
                                async move { downloader::retry(&id).await },
                            );
                        } else {
                            set_status(&weak, tr!("That download is no longer listed"));
                        }
                    }
                    Cmd::DownloadCancel(id) => {
                        if downloader::last_snapshot().iter().any(|i| i.local_id == id) {
                            spawn_download_action(
                                &weak,
                                true,
                                tr!("Download cancelled"),
                                async move { downloader::cancel(&id).await },
                            );
                        } else {
                            set_status(&weak, tr!("That download is no longer listed"));
                        }
                    }
                    Cmd::DownloadDelete(id) => {
                        if downloader::last_snapshot().iter().any(|i| i.local_id == id) {
                            spawn_download_action(
                                &weak,
                                true,
                                tr!("Deleted downloaded file"),
                                async move { downloader::delete(&id).await },
                            );
                        } else {
                            set_status(&weak, tr!("That download is no longer listed"));
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
                        push_downloader_status(&weak, tr!("Waiting\u{2026}"), false, true);
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
                                            tr!("Signing the downloader in to Spotify \u{2014} if a Spotify page opens in your browser, click Agree."),
                                        );
                                    }
                                    st.login_in_progress
                                }
                                Err(e) => {
                                    push_downloader_status(&weak2, e.clone(), false, false);
                                    set_status(&weak2, tr!("Downloader setup failed \u{2014} {}", e));
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
                            // (or its 150s timeout) actually resolves. Only for a sign-in
                            // that really started: there is nothing to reclaim from
                            // otherwise, and an engine that was already stopped (the user
                            // moved to the phone) must stay stopped.
                            if login_in_progress {
                                let _ = tx2.send(Cmd::DownloaderLoginStarted);
                            }
                            // The bridge resolves this itself in the background
                            // (connected, "already exists", or — now that the vendored
                            // login wait has a real deadline — a timeout) with nothing
                            // pushing that change back to this app on its own. Without
                            // polling here, the "Select OnTheSpot..." text above stays
                            // frozen on screen forever regardless of what actually
                            // happened, since nothing else re-asks the bridge unless
                            // the user closes and reopens Settings.
                            // One poller at a time: a second click on Connect while the
                            // first sign-in waits used to start a second one, each
                            // re-asking the bridge every 4 s.
                            let _poller = if login_in_progress { DownloaderLoginPoller::claim() } else { None };
                            if _poller.is_some() {
                                let deadline = tokio::time::Instant::now() + Duration::from_secs(170);
                                let mut failed_polls = 0;
                                while tokio::time::Instant::now() < deadline {
                                    tokio::time::sleep(Duration::from_secs(4)).await;
                                    match downloader::status().await {
                                        Ok(polled) => {
                                            failed_polls = 0;
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
                                        // Show the failure: stopping silently left "Waiting…"
                                        // on screen with the button busy. One slow answer is
                                        // retried; three in a row and the bridge is gone.
                                        Err(e) => {
                                            failed_polls += 1;
                                            push_downloader_status(&weak2, e, false, failed_polls < 3);
                                            if failed_polls >= 3 {
                                                break;
                                            }
                                        }
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
                                    set_status(&weak2, tr!("Download path saved"));
                                }
                                Err(e) => set_status(&weak2, tr!("Failed to set download path \u{2014} {}", e)),
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
                                tr!("Lightify isn\u{2019}t the active Spotify player right now \u{2014} \
                                 Settings \u{2192} Restart, then try again."),
                            );
                        } else if last_is_stale(&last, &engine_track_name) {
                            // `clearqueue` reloads `last`'s track at `last`'s position; if
                            // that's the *previous* song, playback would jump back to it.
                            set_status(&weak, stale_track_note());
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
                            set_status(&weak, tr!("Clearing the queue\u{2026}"));
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
                                        set_status(&weak, tr!("Cleared queue"));
                                    } else {
                                        // Still tracked, so the next play or station retries.
                                        set_status(&weak, lightify_i18n::trn!(
                                            "Cleared the queue, but {n} queued tracks are still there",
                                            "Cleared the queue, but {n} queued tracks are still there",
                                            left.len()
                                        ));
                                    }
                                    if sidebar_mode == 1 {
                                        let pid = current_id(&last);
                                        sidebar_tracks = t;
                                        set_sidebar(&weak, 1,
                                            build_side_rows(&sidebar_tracks, &pid, false),
                                            tr!("No queued tracks"));
                                    }
                                }
                                None => {
                                    // Never heard back: keep the bookkeeping rather than guess.
                                    set_status(&weak, tr!("Couldn\u{2019}t confirm the queue was cleared \u{2014} try again"));
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
                        // The refresh exists because Spotify's queue changed, so a
                        // mirror copied from Spotify earlier is the thing that's stale.
                        if queue_mirror.as_ref().is_some_and(|m| m.from_spotify) {
                            queue_mirror = None;
                        }
                        if sidebar_mode == 1 && sidebar_fetches_in_flight() > 0 {
                            // Don't start a second read that makes the first one stale
                            // (a chart lining up sends these every second or so, and
                            // the panel then never showed ANY answer): run one more
                            // when the current read lands (`SidebarFetched`).
                            queue_refetch = true;
                        } else {
                            refresh_sidebar(&weak, &self_tx, sidebar_mode, &mut sidebar_tracks, &last, &mut queue_mirror, &mut sidebar_generation).await;
                        }
                    }
                    Cmd::RefreshRecentSidebar => {
                        // A track just finished while RECENT is open: re-read it in the
                        // background, keeping the rows on screen until the answer lands.
                        if sidebar_mode == 2 {
                            sidebar_generation += 1;
                            spawn_sidebar_fetch(&self_tx, 2, sidebar_generation);
                        }
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
                                // (Except a top-up of a mirror that was itself copied
                                // from Spotify: that answer is the newer window.)
                                Ok(_) if mode == 1 && queue_mirror.as_ref().is_some_and(|m| !m.from_spotify) => {}
                                Ok(t) => {
                                    if mode == 1 {
                                        let same = t.len() == sidebar_tracks.len()
                                            && t.iter().zip(&sidebar_tracks).all(|(a, b)| a.uri == b.uri);
                                        queue_quiet = if same { (queue_quiet + 1).min(3) } else { 0 };
                                        // While our engine plays, the queue only moves on a
                                        // track change (re-read at once: `queue_seen_track`)
                                        // or a write of ours (which schedules its own read);
                                        // an unchanged answer can then wait a minute, not
                                        // 1.5–12 s. Remote edits from a phone show within it.
                                        let settled = if same && engine_owns_playback(&last, &engine_device_id) {
                                            QUEUE_ENGINE_QUIET
                                        } else {
                                            QUEUE_POLL * (1u32 << queue_quiet)
                                        };
                                        queue_next_at = tokio::time::Instant::now() + settled;
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
                                            // Keep the hand-queued count across a top-up,
                                            // so additions still land ahead of the context
                                            // — also when the mirror that knew it is gone.
                                            let queued = queue_mirror
                                                .as_ref()
                                                .map_or(0, |m| m.queued)
                                                .max(queued_by_hand_head(&t))
                                                .min(t.len());
                                            let mut m = QueueMirror::from_spotify(playing_uri.clone(), t.clone());
                                            m.queued = queued;
                                            m.refill_for = playing_uri;
                                            queue_mirror = Some(m);
                                            persist_mirror(&queue_mirror);
                                        }
                                    }
                                    // RECENT: Spotify's list only has other devices' plays
                                    // (play_history.rs) — shown merged with Lightify's own.
                                    sidebar_tracks = if mode == 2 {
                                        recent_spotify = t;
                                        play_history::merged(&recent_spotify)
                                    } else {
                                        t
                                    };
                                    let empty = if mode == 1 { tr!("No queued tracks") } else { tr!("No recent tracks yet") };
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
                                // RECENT still has Lightify's own plays when Spotify's
                                // list won't load: show those, and say what failed.
                                Err(e) if mode == 2 && !play_history::merged(&[]).is_empty() => {
                                    sidebar_tracks = play_history::merged(&[]);
                                    set_sidebar(&weak, 2, build_side_rows(&sidebar_tracks, &pid, true), tr!("No recent tracks yet"));
                                    set_status(&weak, tr!("Recent \u{2014} other devices\u{2019} plays didn\u{2019}t load: {}", e));
                                }
                                Err(e) => {
                                    if mode == 1 {
                                        // A read that keeps failing backs off as an
                                        // unchanged one does (1.5 → 3 → 6 → 12 s), rather
                                        // than going out again on every 1.5 s tick.
                                        queue_quiet = (queue_quiet + 1).min(3);
                                        queue_next_at = tokio::time::Instant::now() + QUEUE_POLL * (1u32 << queue_quiet);
                                    }
                                    sidebar_tracks.clear();
                                    let what = if mode == 1 { tr!("Queue \u{2014} {}", e) } else { tr!("Recent \u{2014} {}", e) };
                                    set_sidebar(&weak, mode, vec![], what);
                                }
                            }
                        }
                        // A refresh asked for while this read was out: run it now.
                        if queue_refetch && sidebar_fetches_in_flight() == 0 {
                            queue_refetch = false;
                            if sidebar_mode == 1 {
                                let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                            }
                        }
                    }
                    Cmd::RemoveQueuedTrack(i) => {
                        // Neither the Web API nor upstream librespot can remove one item
                        // from the middle of a queue; the vendored Spirc can
                        // (`removefromqueue`), on our own device only.
                        if sidebar_mode == 1 && !engine_owns_playback(&last, &engine_device_id) {
                            // Same reasoning as the Clear button: this whole mechanism is
                            // spirc-only (no Web-API "remove from queue" exists at all), so
                            // if Lightify isn't actually the active device this would
                            // silently do nothing to the real queue while still claiming
                            // the track was removed.
                            set_status(
                                &weak,
                                tr!("Lightify isn\u{2019}t the active Spotify player right now \u{2014} \
                                 Settings \u{2192} Restart, then try again."),
                            );
                        } else if sidebar_mode == 1 && last_is_stale(&last, &engine_track_name) {
                            set_status(&weak, stale_track_note());
                        } else if sidebar_mode == 1 {
                            if let Some(removed) = sidebar_tracks.get(i).cloned() {
                                // One local command to the engine (the vendored
                                // `Spirc::remove_from_queue`): only that row goes; the rest of
                                // the queue and the playlist/album it came from stay as they
                                // were. The old way cleared the device queue and re-added at
                                // most 20 of the rest as single tracks, which dropped the
                                // context — playback stopped ~20 songs later.
                                let nth = sidebar_tracks[..i].iter().filter(|t| t.uri == removed.uri).count();
                                engine::send(
                                    &serde_json::json!({ "cmd": "removefromqueue", "uri": removed.uri, "nth": nth }).to_string(),
                                );
                                sidebar_tracks.remove(i);
                                if let Some(m) = queue_mirror.as_mut() {
                                    if i < m.upcoming.len() {
                                        m.upcoming.remove(i);
                                        if i < m.queued {
                                            m.queued -= 1;
                                        }
                                    }
                                    if m.upcoming.is_empty() {
                                        queue_mirror = None;
                                    }
                                }
                                // A lane's bookkeeping only loses the uri once no copy of it
                                // is left in the queue.
                                if !sidebar_tracks.iter().any(|t| t.uri == removed.uri) {
                                    station_queued.retain(|u| *u != removed.uri);
                                    bp_queued.retain(|u| *u != removed.uri);
                                }
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                                let pid = current_id(&last);
                                set_sidebar(&weak, 1, build_side_rows(&sidebar_tracks, &pid, false), tr!("No queued tracks"));
                                spawn_delayed_queue_refresh(&self_tx);
                                set_status(&weak, tr!("Removed \u{201c}{}\u{201d} from the queue", removed.name));
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
                        if let Some(t) = sidebar_tracks.get(i).cloned() {
                            if !t.uri.is_empty() {
                                // RECENT starts something new; QUEUE is navigation inside
                                // the queue itself, where a reset would destroy what is
                                // being used. Only once there is something to play: a row
                                // with nothing to play keeps a station or chart going.
                                if sidebar_mode != 1 {
                                    end_autoplay_authority(
                                        &weak, &mut session, &mut station_active, &mut station_queued,
                                        &mut bp_autoplay, &mut bp_queued,
                                        &bp_active_seq, &last, &engine_device_id,
                                    )
                                    .await;
                                }
                                launched_src = None;
                                // In QUEUE, jump *into* the queue: play the clicked track and
                                // everything after it. Playing it alone replaced the whole
                                // context, so the rest of the list on screen never played.
                                let mut rest: Vec<Track> = if sidebar_mode == 1 {
                                    sidebar_tracks[i + 1..]
                                        .iter()
                                        .filter(|x| !x.uri.is_empty())
                                        .cloned()
                                        .collect()
                                } else {
                                    Vec::new()
                                };
                                // A playlist/album track in QUEUE is a position in that
                                // context: jump there inside it, so the whole rest of the
                                // playlist still follows. As a bare uri list it replaced
                                // the context with the ~20 rows the panel happened to
                                // show, and playback stopped once they ran out. (Hand-
                                // queued, station and Beatport rows aren't part of the
                                // context, so they keep the uri-list jump.)
                                let ctx = last
                                    .as_ref()
                                    .and_then(|p| p.context_uri.clone())
                                    .filter(|c| {
                                        c.starts_with("spotify:playlist:") || c.starts_with("spotify:album:") || is_liked_context(c)
                                    });
                                // A row Spotify lists ahead of the context's own tracks is
                                // in its user queue, not in the playlist: a context jump to
                                // it found no such track and started from track 1.
                                let hand_head = match queue_mirror.as_ref() {
                                    Some(m) => m.queued,
                                    None if sidebar_mode == 1 => queued_by_hand_head(&sidebar_tracks),
                                    None => 0,
                                };
                                let hand_queued = i < hand_head
                                    || station_queued.contains(&t.uri)
                                    || bp_queued.contains(&t.uri);
                                let previous = current_uri(&last);
                                // A window onto Spotify's queue stays one after the jump,
                                // so it keeps topping itself up (`QueueMirror::from_spotify`).
                                let mut keep_spotify = false;
                                // A hand-queued row while our engine plays: skip forward to
                                // it inside the queue — the rows queued ahead of it go (as in
                                // Spotify), and the playlist still follows. Re-loading it as a
                                // uri list put Spotify's ~20-row window in the playlist's
                                // place, and playback stopped once that ran out.
                                // A row of a plain list (a chart, a station, a re-sorted list)
                                // while the panel shows Spotify's ~20-row read: skip to it
                                // inside the list, which stays whole. Re-played as a list,
                                // those rows replaced the rest of it (BUGS-PLAN QD8).
                                let list_row = ctx.is_none() && queue_mirror.as_ref().map_or(true, |m| m.from_spotify);
                                let skip_to = sidebar_mode == 1
                                    && (i < hand_head || list_row)
                                    && engine_owns_playback(&last, &engine_device_id)
                                    && !last_is_stale(&last, &engine_track_name);
                                let r = match ctx {
                                    _ if skip_to => {
                                        if i < hand_head {
                                            for skipped in &sidebar_tracks[..i] {
                                                // Each is at the head of the queue by the time it goes.
                                                engine::send(
                                                    &serde_json::json!({ "cmd": "removefromqueue", "uri": skipped.uri, "nth": 0 }).to_string(),
                                                );
                                            }
                                            engine::send(&serde_json::json!({ "cmd": "next" }).to_string());
                                        } else {
                                            // Rows before it are skipped unloaded (no audio key each).
                                            engine::send(&serde_json::json!({ "cmd": "skipto", "uri": t.uri }).to_string());
                                        }
                                        sidebar_tracks.drain(..i);
                                        if let Some(m) = queue_mirror.as_mut() {
                                            let n = i.min(m.upcoming.len());
                                            m.upcoming.drain(..n);
                                            m.queued = m.queued.saturating_sub(n);
                                        }
                                        rest.clear();
                                        Ok(true)
                                    }
                                    Some(ctx) if sidebar_mode == 1 && !station_active && !hand_queued => {
                                        keep_spotify = queue_mirror.as_ref().is_some_and(|m| m.from_spotify);
                                        if is_liked_context(&ctx) {
                                            // Only our engine can start Liked Songs as a context.
                                            if engine_load_context(&ctx, &t.uri, 0, &last, &engine_device_id, true) {
                                                Ok(true)
                                            } else {
                                                Err(tr!("Playback engine isn\u{2019}t running"))
                                            }
                                        } else if engine_load_context(&ctx, &t.uri, 0, &last, &engine_device_id, true) {
                                            // The list playing, jumped into: rows removed from
                                            // QUEUE stay gone (QD5).
                                            Ok(true)
                                        } else {
                                            session.play_context(&ctx, &t.uri).await.map(|()| true)
                                        }
                                    }
                                    _ => {
                                        let mut uris = vec![t.uri.clone()];
                                        uris.extend(rest.iter().map(|x| x.uri.clone()));
                                        let r = play_track_list(&mut session, &uris, 0, 0, &last, &engine_device_id).await;
                                        // A chart lining up on the engine appends after its
                                        // first track, which this new list no longer holds:
                                        // the clicked row is the new anchor, or every later
                                        // match is dropped and the chart stops growing.
                                        if let (Ok(true), true, Some(list)) = (&r, sidebar_mode == 1, bp_list.as_mut()) {
                                            if bp_active_seq.load(std::sync::atomic::Ordering::SeqCst) == list.id {
                                                list.anchor = t.uri.clone();
                                            }
                                        }
                                        r
                                    }
                                };
                                if skip_to {
                                    // The mirror (if any) was folded above; the track change
                                    // advances it onto the clicked row.
                                    persist_mirror(&queue_mirror);
                                    let pid = current_id(&last);
                                    set_sidebar(&weak, 1, build_side_rows(&sidebar_tracks, &pid, false), tr!("No queued tracks"));
                                } else if sidebar_mode == 1 {
                                    if let Ok(whole) = r {
                                        if !whole {
                                            rest.truncate(WEB_API_URIS_MAX - 1);
                                        }
                                        // The panel now lists exactly what was sent — unless
                                        // shuffle is on, where Spotify picks the order and
                                        // only its own queue (fetched by the panel) is right.
                                        let m = if keep_spotify {
                                            QueueMirror::from_spotify(t.uri.clone(), rest)
                                        } else {
                                            QueueMirror::new(t.uri.clone(), rest)
                                        };
                                        let shuffled = last.as_ref().is_some_and(|p| p.shuffle_state);
                                        queue_mirror = (!shuffled).then(|| m.replacing(&previous));
                                        sidebar_generation += 1;
                                        persist_mirror(&queue_mirror);
                                    }
                                }
                                if let Err(e) = r {
                                    set_status(&weak, tr!("Play — {}", e));
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
                            bp_fetch(&weak, &self_tx, &bp_genres, bp_idx, &bp_kind, &mut bp_tracks, &mut bp_fetch_seq);
                        }
                    }
                    Cmd::BpSelectGenre(i) => {
                        if let Some((label, _slug)) = bp_genres.get(i) {
                            bp_idx = i;
                            bp_loaded = true;
                            set_bp_genre_selected(&weak, i, label.clone());
                            drop_selection(&weak, SEL_BEATPORT, &mut selected, &mut sel_anchor, &mut sel_scope);
                            bp_fetch(&weak, &self_tx, &bp_genres, bp_idx, &bp_kind, &mut bp_tracks, &mut bp_fetch_seq);
                        }
                    }
                    Cmd::BpSetMode(kind) => {
                        bp_kind = kind;
                        bp_loaded = true;
                        set_bp_kind(&weak, bp_kind.clone());
                        drop_selection(&weak, SEL_BEATPORT, &mut selected, &mut sel_anchor, &mut sel_scope);
                        bp_fetch(&weak, &self_tx, &bp_genres, bp_idx, &bp_kind, &mut bp_tracks, &mut bp_fetch_seq);
                    }
                    Cmd::BpPlay(i) => 'bp_play: {
                        // Ports `playBeatportSequenceFromIndex` (`app.js:3868`): play
                        // FROM this row and keep queueing the rest of the chart behind
                        // it, rather than playing the one matched track and stopping.
                        if i < bp_tracks.len() {
                            if let Some(left) = lightify_core::ratelimit::blocked_for() {
                                // Every lookup would fail at once; what plays now stays.
                                set_bp_status(&weak, tr!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(left.as_secs() + 1)));
                                break 'bp_play;
                            }
                            let label = bp_tracks[i].name.clone();
                            set_bp_status(&weak, tr!("Matching \u{201c}{}\u{201d} on Spotify\u{2026}", label));

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
                                strikes: 0,
                            };
                            let mut first: Vec<String> = Vec::new();
                            let mut first_tracks: Vec<Track> = Vec::new();
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
                            let mut failed: Option<String> = None;
                            while first.len() < BP_INITIAL_MATCH && seq.next_index < seq.source.len() {
                                let is_clicked_row = seq.next_index == i;
                                let bt = seq.source[seq.next_index].clone();
                                seq.next_index += 1;
                                match session.beatport_match_with(&bt.name, &bt.artists, &bt.isrc).await {
                                    Ok((Some(t), _)) => {
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
                                        first.push(t.uri.clone());
                                        first_tracks.push(t);
                                    }
                                    Ok((None, _)) => continue,
                                    Err(e) => {
                                        failed = Some(e);
                                        break;
                                    }
                                }
                            }

                            if first.is_empty() {
                                // A failed search isn't "no match" (lightify-core answers
                                // `Ok(None)` only for a real miss).
                                set_bp_status(
                                    &weak,
                                    match failed {
                                        Some(e) => tr!("Match \u{2014} {}", e),
                                        None => tr!("No Spotify match for \u{201c}{}\u{201d}", label),
                                    },
                                );
                                // Nothing to play: the running chart or station carries on.
                                break 'bp_play;
                            }
                            // Something to play: only now does what was playing give way.
                            end_autoplay_authority(
                                &weak, &mut session, &mut station_active, &mut station_queued,
                                &mut bp_autoplay, &mut bp_queued,
                                &bp_active_seq, &last, &engine_device_id,
                            )
                            .await;
                            bp_autoplay = None;
                            bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                            if engine::running() && !engine_device_id.is_empty() {
                                // The whole chart as ONE list on our own engine: the first
                                // match plays now, the rest is matched in the background
                                // and appended in chart order (`BpMatched`). The old
                                // queue-ahead lane topped up a handful at a time from the
                                // playback poll, and dropped the chart for good the moment
                                // that poll didn't recognise the playing track — it
                                // stopped after the first 4–5.
                                launched_src = None;
                                // The QUEUE panel is built here, not read back from
                                // Spotify: we know exactly what goes on the list and in
                                // what order, and Spotify's queue read only ever returns
                                // ~20 of it. The rest of the chart shows as placeholders
                                // until it is found (`BpPanel`).
                                let previous = current_uri(&last);
                                // Shuffled, the engine plays the list in its own order: no
                                // mirror then (as on every other play path), the panel
                                // reads Spotify's queue instead of showing chart order.
                                let shuffled = last.as_ref().is_some_and(|p| p.shuffle_state);
                                queue_mirror = (!shuffled).then(|| {
                                    QueueMirror::new(first[0].clone(), first_tracks[1..].to_vec())
                                        .keeping_queued(queue_mirror.as_ref())
                                        .replacing(&previous)
                                });
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                                match play_track_list(&mut session, &first, 0, 0, &last, &engine_device_id).await {
                                    Err(e) => set_bp_status(&weak, tr!("Play \u{2014} {}", e)),
                                    Ok(_) => {
                                        let left = seq.source.len().saturating_sub(seq.next_index);
                                        // Whole sentences, with and without the note that
                                        // the clicked row was swapped for the next one.
                                        set_bp_status(
                                            &weak,
                                            match (clicked_matched, left > 0) {
                                                (true, true) => lightify_i18n::trn!(
                                                    "Playing \u{2014} lining up the rest of the chart ({n} to go)",
                                                    "Playing \u{2014} lining up the rest of the chart ({n} to go)",
                                                    left
                                                ),
                                                (true, false) => tr!("Playing {} \u{2014} {}", first_name, first_artists),
                                                (false, true) => lightify_i18n::trn!(
                                                    "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing \u{2014} lining up the rest of the chart ({n} to go)",
                                                    "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing \u{2014} lining up the rest of the chart ({n} to go)",
                                                    left, label, first_name
                                                ),
                                                (false, false) => tr!(
                                                    "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing {1} \u{2014} {2}",
                                                    label, first_name, first_artists
                                                ),
                                            },
                                        );
                                        bp_active_seq.store(seq.id, std::sync::atomic::Ordering::SeqCst);
                                        let total = seq.source.len().saturating_sub(i);
                                        bp_list = Some(BpList {
                                            id: seq.id,
                                            anchor: first[0].clone(),
                                            lined_up: first.len(),
                                            total,
                                            next_index: seq.next_index,
                                        });
                                        set_bp_panel(Some(BpPanel {
                                            seq_id: seq.id,
                                            active: bp_active_seq.clone(),
                                            title: bp_chart_title(&bp_genres, bp_idx, &bp_kind),
                                            found: first.len(),
                                            total,
                                            waiting: bp_waiting(&seq.source, seq.next_index),
                                            next_index: seq.next_index,
                                            start: i,
                                            done: left == 0,
                                            missed: (seq.next_index - i).saturating_sub(first.len()),
                                        }));
                                        if sidebar_mode == 1 {
                                            if let Some(m) = queue_mirror.as_ref() {
                                                sidebar_tracks = m.upcoming.clone();
                                                let pid = first_tracks[0].id.clone();
                                                set_sidebar(&weak, 1, build_side_rows(&sidebar_tracks, &pid, false), tr!("No queued tracks"));
                                            } else {
                                                spawn_delayed_queue_refresh(&self_tx);
                                            }
                                        }
                                        if left > 0 {
                                            let tx2 = self_tx.clone();
                                            let active_seq = bp_active_seq.clone();
                                            let (source, next_index, seen, seq_id) =
                                                (seq.source.clone(), seq.next_index, seq.seen.clone(), seq.id);
                                            tokio::spawn(bp_match_background(source, next_index, seen, seq_id, active_seq, tx2));
                                        }
                                    }
                                }
                                refresh_soon(&mut poll_soon);
                            } else {
                                launched_src = None;
                                // Not the old list's mirror (QD7).
                                queue_mirror = None;
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                                if let Err(e) = session.play_uris(&first).await {
                                    set_bp_status(&weak, tr!("Play \u{2014} {}", e));
                                } else {
                                    set_bp_status(
                                        &weak,
                                        tr!("Playing {} \u{2014} {}", first_name, first_artists),
                                    );
                                    settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                    // One refill now so the queue isn't empty the moment
                                    // they open it; the poll tick tops it up from here.
                                    seq.completed = seq.next_index >= seq.source.len();
                                    // The same background refill the poll tick uses: awaited here,
                                    // the first fill held up every other command while it matched
                                    // (BUGS-PLAN DM3-05). Its result comes back as BpRefillDone.
                                    if !seq.completed && !seq.refill_pending {
                                        seq.refill_pending = true;
                                        let seq_id = seq.id;
                                        let source = seq.source.clone();
                                        let next_index = seq.next_index;
                                        let seen = seq.seen.clone();
                                        let strikes = seq.strikes;
                                        let tx2 = self_tx.clone();
                                        let active_seq = bp_active_seq.clone();
                                        tokio::spawn(async move {
                                            let result = bp_refill_background(source, next_index, seen, seq_id, active_seq, strikes).await;
                                            let _ = tx2.send(Cmd::BpRefillDone {
                                                seq_id,
                                                queued: result.0,
                                                seen: result.1,
                                                next_index: result.2,
                                                completed: result.3,
                                                error: result.4,
                                                strikes: result.5,
                                            });
                                        });
                                    }
                                    let left = seq.source.len().saturating_sub(seq.next_index);
                                    // `loaded_ids` also holds the track that is playing.
                                    let ahead = seq.loaded_ids.len().saturating_sub(1);
                                    // The substitution note survives into this final
                                    // message rather than only the transient one above,
                                    // since this is the one the user actually ends up
                                    // reading (the "queued, left to match" update lands
                                    // moments later and would otherwise erase it).
                                    set_bp_status(
                                        &weak,
                                        // The count that picks the plural form: what is
                                        // left to match, else what was queued.
                                        match (clicked_matched, left > 0) {
                                            (true, true) => lightify_i18n::trn!(
                                                "Playing \u{2014} {0} queued, {n} left to match",
                                                "Playing \u{2014} {0} queued, {n} left to match",
                                                left, ahead
                                            ),
                                            (true, false) => lightify_i18n::trn!("Playing \u{2014} {n} queued", "Playing \u{2014} {n} queued", ahead),
                                            (false, true) => lightify_i18n::trn!(
                                                "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing \u{2014} {2} queued, {n} left to match",
                                                "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing \u{2014} {2} queued, {n} left to match",
                                                left, label, first_name, ahead
                                            ),
                                            (false, false) => lightify_i18n::trn!(
                                                "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing \u{2014} {n} queued",
                                                "\u{201c}{0}\u{201d} isn\u{2019}t on Spotify \u{2014} playing \u{201c}{1}\u{201d} instead. Playing \u{2014} {n} queued",
                                                ahead, label, first_name
                                            ),
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
                    Cmd::BpFetched { seq, genre, kind, result } => {
                        // Only the latest fetch, and only while its genre and chart are
                        // still the ones picked: a slow reply must not replace a newer one.
                        if seq == bp_fetch_seq && genre == bp_idx && kind == bp_kind {
                            let label = bp_genres.get(genre).map(|(l, _)| l.clone()).unwrap_or_default();
                            let chart = bp_chart_label(&kind);
                            match result {
                                Ok(t) => {
                                    push_bp_rows(&weak, build_bp_rows(&t));
                                    let n = t.len();
                                    bp_tracks = t;
                                    set_bp_status(&weak, lightify_i18n::trn!("{n} tracks \u{2014} {} {}", "{n} tracks \u{2014} {} {}", n, label, chart));
                                }
                                Err(e) => {
                                    bp_tracks.clear();
                                    push_bp_rows(&weak, vec![]);
                                    set_bp_status(&weak, e);
                                }
                            }
                        }
                    }
                    Cmd::BpOpenExternal => {
                        let slug = bp_genres.get(bp_idx).map(|(_, s)| s.clone()).unwrap_or_default();
                        match lightify_core::beatport::chart_url(&slug, &bp_kind) {
                            Ok(url) => {
                                if open_url(&url) {
                                    set_bp_status(&weak, tr!("Opened the chart in your browser"));
                                } else {
                                    set_bp_status(&weak, tr!("Open this in your browser: {}", url));
                                }
                            }
                            Err(e) => set_bp_status(&weak, e),
                        }
                    }
                    Cmd::BpMatched { seq_id, tracks: found, next_index, done, error } => {
                        let current = bp_list.as_ref().is_some_and(|l| l.id == seq_id)
                            && bp_active_seq.load(std::sync::atomic::Ordering::SeqCst) == seq_id;
                        if let (true, Some(list)) = (current, bp_list.as_mut()) {
                            list.next_index = list.next_index.max(next_index);
                            if !found.is_empty() {
                                list.lined_up += found.len();
                                let uris: Vec<String> = found.iter().map(|t| t.uri.clone()).collect();
                                engine::send(
                                    &serde_json::json!({ "cmd": "appendtracks", "anchor": list.anchor, "uris": uris })
                                        .to_string(),
                                );
                                // The panel follows the list we built, not Spotify's
                                // ~20-item read of it: the new tracks go on the end of
                                // the mirror (appends land after everything else).
                                if let Some(m) = queue_mirror.as_mut().filter(|m| !m.from_spotify) {
                                    m.upcoming.extend(found.iter().cloned());
                                }
                            }
                            let (n, total) = (list.lined_up, list.total);
                            bp_panel_progress(seq_id, n, list.next_index, done);
                            if sidebar_mode == 1 {
                                if let Some(m) = queue_mirror.as_ref().filter(|m| !m.from_spotify) {
                                    sidebar_tracks = m.upcoming.clone();
                                    persist_mirror(&queue_mirror);
                                    let pid = current_id(&last);
                                    set_sidebar(&weak, 1, build_side_rows(&sidebar_tracks, &pid, false), tr!("No queued tracks"));
                                }
                            }
                            match (&error, done) {
                                (_, true) => {
                                    set_bp_status(
                                        &weak,
                                        lightify_i18n::trn!(
                                            "Playing the chart \u{2014} {} of {n} tracks found on Spotify",
                                            "Playing the chart \u{2014} {} of {n} tracks found on Spotify",
                                            total, n
                                        ),
                                    );
                                    bp_list = None;
                                }
                                (Some(e), false) => set_bp_status(&weak, tr!("Lining up the chart \u{2014} {} so far; retrying ({})", n, e)),
                                (None, false) => set_bp_status(&weak, tr!("Playing the chart \u{2014} {} lined up\u{2026}", n)),
                            }
                        }
                    }
                    Cmd::BpRefillDone { seq_id, queued, seen, next_index, completed, error, strikes } => {
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
                                seq.strikes = strikes;
                                for (_uri, id) in &queued {
                                    if !id.is_empty() {
                                        seq.loaded_ids.push(id.clone());
                                    }
                                }
                                if let Some(e) = error {
                                    set_status(&weak, tr!("Beatport queue refill \u{2014} {}", e));
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
                    // Settings closed since this was sent: nothing to fill, and never
                    // reopened from here (the gear and the palette open it themselves).
                    Cmd::OpenSettings | Cmd::RefreshDevices if !devices_open(&weak) => {}
                    Cmd::OpenSettings | Cmd::RefreshDevices => {
                        set_account(&weak, connected_status(&session));
                        // Re-read the live process state rather than trusting the last
                        // event: an engine that died silently must not still read "Running".
                        if !engine::running() {
                            push_engine_status(&weak, || tr!("Not running"), false);
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
                        push_downloader_status(&weak, tr!("Checking\u{2026}"), false, false);
                        spawn_downloader_status(&weak);
                        set_device_status(&weak, || tr!("Loading\u{2026}"));
                        push_devices(&weak, vec![]);
                        match session.devices().await {
                            Ok(d) => {
                                if d.is_empty() {
                                    set_device_status(&weak, || tr!("No devices yet \u{2014} Lightify\u{2019}s own player is still starting."));
                                }
                                push_devices(&weak, build_device_rows(&d));
                                devices = d;
                            }
                            Err(e) => {
                                devices.clear();
                                let e2 = e.clone();
                                set_device_status(&weak, move || tr!("Devices \u{2014} {}", e2));
                                push_devices(&weak, vec![]);
                            }
                        }
                    }
                    Cmd::SelectDevice(i) => {
                        if let Some(d) = devices.get(i).cloned() {
                            let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                            match session.transfer_playback(&d.id, playing).await {
                                Ok(()) => {
                                    set_status(&weak, tr!("Playing on {}", d.name));
                                    note_settings_open(false);
                                    let _ = weak.upgrade_in_event_loop(|app| app.set_settings_open(false));
                                    settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                }
                                Err(e) => set_status(&weak, tr!("Transfer — {}", e)),
                            }
                        }
                    }
                    Cmd::CollapseLeft => {
                        let _ = weak.upgrade_in_event_loop(|app| app.set_left_collapsed(!app.get_left_collapsed()));
                    }
                    Cmd::PaletteOpen => {
                        let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                        palette_actions = apply_palette_filter(&weak, "", &playlists, &playlist_order(&playlists, &lib_mode, &lib_dir, ""), playing);
                    }
                    Cmd::PaletteQuery(q) => {
                        let playing = last.as_ref().map(|p| p.is_playing).unwrap_or(false);
                        palette_actions = apply_palette_filter(&weak, &q, &playlists, &playlist_order(&playlists, &lib_mode, &lib_dir, ""), playing);
                    }
                    Cmd::PaletteRun(idx) => {
                        if let Some(cmd) = palette_actions.get(idx).cloned() {
                            match &cmd {
                                // A playlist (or Liked Songs) opens in the Library tab's
                                // list: from another tab, or with the panel collapsed,
                                // nothing visible happened.
                                Cmd::Drill(_) => {
                                    set_tab(&weak, 0);
                                    let _ = weak.upgrade_in_event_loop(|app| app.set_left_collapsed(false));
                                }
                                // Opened here, as the gear opens it on the UI side: the
                                // worker no longer opens Settings itself (a queued
                                // `OpenSettings` reopened a panel closed meanwhile).
                                Cmd::OpenSettings => {
                                    note_settings_open(true);
                                    let _ = weak.upgrade_in_event_loop(|app| app.set_settings_open(true));
                                }
                                _ => {}
                            }
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
                            8 => local_ui::row_menu(&local, index),
                            9 => local_ui::node_menu(&local, index),
                            10 => local_ui::queue_menu(&local, index),
                            11 => vec![CtxItem::new(tr!("ASCII lava screensaver"), Cmd::LavaSaver)],
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
                            match queue_one(&mut session, &uri, engine_owns_playback(&last, &engine_device_id)).await {
                                Ok(()) => {
                                    mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[&search_results.tracks]);
                                    if sidebar_mode == 1 {
                                        let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                    }
                                    set_status(&weak, tr!("Added to queue"));
                                }
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    Cmd::LikeSearchTrack(i) => {
                        if let Some(SearchAction::Track { id, .. }) = search_actions.get(i) {
                            match session.set_track_saved(id, true).await {
                                Ok(()) => set_status(&weak, tr!("Saved to Liked Songs")),
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
                        let uri = match search_actions.get(i) {
                            Some(SearchAction::Playlist { uri, .. }) => uri.clone(),
                            Some(SearchAction::Album { uri, .. }) => uri.clone(),
                            _ => String::new(),
                        };
                        if uri.is_empty() {
                            // Nothing to play: a station or chart keeps going.
                            set_status(&weak, tr!("Nothing to play here"));
                        } else {
                            end_autoplay_authority(
                                &weak, &mut session, &mut station_active, &mut station_queued,
                                &mut bp_autoplay, &mut bp_queued,
                                &bp_active_seq, &last, &engine_device_id,
                            )
                            .await;
                            launched_src = None;
                            match play_context_from_start(&mut session, &uri, &last, &engine_device_id).await {
                                // The old QUEUE mirror is the list this play replaced.
                                Ok(()) => forget_mirror(&mut queue_mirror, &mut sidebar_generation, sidebar_mode, &self_tx),
                                Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
                            }
                            refresh_soon(&mut poll_soon);
                        }
                    }
                    Cmd::QueueSearchHit(i) => 'queue_hit: {
                        let Some(action) = search_actions.get(i).cloned() else { break 'queue_hit };
                        set_status(&weak, tr!("Getting tracks\u{2026}"));
                        let (name, picked, total) = match search_hit_tracks(&mut session, &action).await {
                            Ok(r) => r,
                            Err(e) => {
                                set_status(&weak, tr!("Add to queue \u{2014} {}", e));
                                break 'queue_hit;
                            }
                        };
                        if picked.is_empty() {
                            set_status(&weak, tr!("Nothing playable in \u{201c}{}\u{201d}", name));
                            break 'queue_hit;
                        }
                        // Appended after anything already queued, in the hit's own order:
                        // one request per track, stopping at the first failure so a
                        // mid-run rate limit reports what actually landed.
                        let uris: Vec<String> = picked.iter().map(|t| t.uri.clone()).collect();
                        let mut queued = 0usize;
                        let mut failed: Option<String> = None;
                        for uri in &uris {
                            match queue_one(&mut session, uri, engine_owns_playback(&last, &engine_device_id)).await {
                                Ok(()) => {
                                    queued += 1;
                                    if queued % 5 == 0 && queued < uris.len() {
                                        set_status(&weak, tr!("Adding to queue\u{2026} {}/{}", queued, uris.len()));
                                    }
                                }
                                Err(e) => {
                                    failed = Some(e);
                                    break;
                                }
                            }
                        }
                        if queued > 0 {
                            mirror_queued(&mut queue_mirror, &mut sidebar_generation, &uris[..queued], &[&picked]);
                            last_bulk_queue_write = Some(tokio::time::Instant::now());
                            spawn_delayed_queue_refresh(&self_tx);
                        }
                        match failed {
                            Some(e) if queued == 0 => set_status(&weak, tr!("Add to queue \u{2014} {}", e)),
                            Some(e) => set_status(
                                &weak,
                                lightify_i18n::trn!(
                                    "Added {n} track to queue, then stopped \u{2014} {0}",
                                    "Added {n} tracks to queue, then stopped \u{2014} {0}",
                                    queued, e
                                ),
                            ),
                            None if total > queued && queued == QUEUE_HIT_MAX => set_status(
                                &weak,
                                lightify_i18n::trn!(
                                    "Added the first {0} of {n} tracks from \u{201c}{1}\u{201d} to queue",
                                    "Added the first {0} of {n} tracks from \u{201c}{1}\u{201d} to queue",
                                    total, queued, name
                                ),
                            ),
                            None => set_status(
                                &weak,
                                lightify_i18n::trn!(
                                    "Added {n} track from \u{201c}{0}\u{201d} to queue",
                                    "Added {n} tracks from \u{201c}{0}\u{201d} to queue",
                                    queued, name
                                ),
                            ),
                        }
                    }
                    Cmd::SaveSearchItem(i) => {
                        let r = match search_actions.get(i) {
                            Some(SearchAction::Playlist { id, .. }) => {
                                Some((session.follow_playlist(id).await, tr!("Playlist saved")))
                            }
                            Some(SearchAction::Album { id, .. }) => {
                                Some((session.save_album(id).await, tr!("Album saved")))
                            }
                            _ => None,
                        };
                        if let Some((res, ok)) = r {
                            match res {
                                Ok(()) => {
                                    set_status(&weak, ok);
                                    // A saved playlist is a new library row: show it.
                                    if matches!(search_actions.get(i), Some(SearchAction::Playlist { .. })) {
                                        let _ = self_tx.send(Cmd::ReloadLibrary);
                                    }
                                }
                                Err(e) => set_status(&weak, e),
                            }
                        }
                    }
                    // ── Beatport menus ─────────────────────────────────────────
                    Cmd::BpQueue(i) => {
                        if let Some(bt) = bp_tracks.get(i).cloned() {
                            match session.beatport_match(&bt.name, &bt.artists).await {
                                Ok(Some(t)) => match queue_one(&mut session, &t.uri, engine_owns_playback(&last, &engine_device_id)).await {
                                    Ok(()) => {
                                        set_status(&weak, tr!("Added to queue: {}", t.name));
                                        let uri = t.uri.clone();
                                        mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[std::slice::from_ref(&t)]);
                                        if sidebar_mode == 1 {
                                            let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                        }
                                    }
                                    Err(e) => set_status(&weak, e),
                                },
                                Ok(None) => set_status(&weak, tr!("No Spotify match for {}", bt.name)),
                                Err(e) => set_status(&weak, tr!("Beatport \u{2014} {}", e)),
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
                        set_status(&weak, tr!("No Beatport tracks selected"));
                    }
                    Cmd::BpPlaySelection | Cmd::BpQueueSelection | Cmd::BpCreatePlaylist => {
                        // Beatport rows are Beatport tracks: each has to be resolved to
                        // a Spotify track first (the host's scored match).
                        let picked: Vec<BeatportTrack> = bp_tracks
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| selected.contains(i))
                            .map(|(_, t)| t.clone())
                            .collect();
                        if picked.is_empty() {
                            set_status(&weak, tr!("No Beatport tracks selected"));
                        } else if let Some(left) = lightify_core::ratelimit::blocked_for() {
                            // Every row would fail at once and count as unmatched.
                            set_status(&weak, tr!("Spotify rate limit \u{2014} resuming in {}", fmt_backoff(left.as_secs() + 1)));
                        } else {
                            set_status(&weak, lightify_i18n::trn!("Matching {n} Beatport tracks\u{2026}", "Matching {n} Beatport tracks\u{2026}", picked.len()));
                            let (bp_matched, missed, failed) = bp_match_rows(&mut session, &picked).await;
                            let uris: Vec<String> = bp_matched.iter().map(|t| t.uri.clone()).collect();
                            if let Some(e) = failed {
                                // A failed search isn't a miss: a part-matched selection
                                // would be played, queued or saved as if it were whole.
                                set_status(&weak, tr!("Couldn\u{2019}t match the selection \u{2014} {}", e));
                            } else if uris.is_empty() {
                                set_status(&weak, tr!("No Spotify matches for the selection"));
                            } else {
                                // Each message whole, with and without the unmatched count.
                                match cmd {
                                    Cmd::BpPlaySelection => {
                                        // A selection is its own fixed list, not the running
                                        // chart. Queueing or saving one leaves the chart alone.
                                        bp_autoplay = None;
                                        bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                                        end_autoplay_authority(
                                            &weak, &mut session, &mut station_active, &mut station_queued,
                                            &mut bp_autoplay, &mut bp_queued,
                                            &bp_active_seq, &last, &engine_device_id,
                                        )
                                        .await;
                                        launched_src = None;
                                        // Not the old list's mirror (QD7).
                                        queue_mirror = None;
                                        sidebar_generation += 1;
                                        persist_mirror(&queue_mirror);
                                        let n = uris.len();
                                        if let Err(e) = play_track_list(&mut session, &uris, 0, 0, &last, &engine_device_id).await {
                                            set_status(&weak, tr!("Play \u{2014} {}", e));
                                        } else {
                                            set_status(
                                                &weak,
                                                if missed > 0 {
                                                    lightify_i18n::trn!(
                                                        "Playing {n} selected ({} unmatched)",
                                                        "Playing {n} selected ({} unmatched)",
                                                        n, missed
                                                    )
                                                } else {
                                                    lightify_i18n::trn!("Playing {n} selected", "Playing {n} selected", n)
                                                },
                                            );
                                        }
                                        settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                                    }
                                    Cmd::BpQueueSelection => {
                                        let mut queued = 0usize;
                                        let mut failed: Option<String> = None;
                                        for uri in &uris {
                                            match queue_one(&mut session, uri, engine_owns_playback(&last, &engine_device_id)).await {
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
                                            Some(e) => set_status(
                                                &weak,
                                                lightify_i18n::trn!(
                                                    "Added {n} to queue, then stopped \u{2014} {}",
                                                    "Added {n} to queue, then stopped \u{2014} {}",
                                                    queued, e
                                                ),
                                            ),
                                            None if missed > 0 => set_status(
                                                &weak,
                                                lightify_i18n::trn!(
                                                    "Added {n} to queue ({} unmatched)",
                                                    "Added {n} to queue ({} unmatched)",
                                                    queued, missed
                                                ),
                                            ),
                                            None => set_status(&weak, lightify_i18n::trn!("Added {n} to queue", "Added {n} to queue", queued)),
                                        }
                                    }
                                    _ => {
                                        let name = named.clone().unwrap_or_else(|| timestamped_playlist_name(&tr!("BP Selection")));
                                        match session.create_playlist(&name, &uris).await {
                                            Ok(pl) => {
                                                set_status(
                                                    &weak,
                                                    if missed > 0 {
                                                        lightify_i18n::trn!(
                                                            "Created: {} ({n} unmatched)",
                                                            "Created: {} ({n} unmatched)",
                                                            missed, pl.name
                                                        )
                                                    } else {
                                                        tr!("Created: {}", pl.name)
                                                    },
                                                );
                                                if let Ok(p) = session.playlists().await {
                                                    playlists = p;
                                                    remember_library(&playlists);
                                                    thumbs::request(&weak, &playlists);
                                                    pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                                    push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                                                }
                                            }
                                            Err(e) => set_status(&weak, tr!("Create playlist \u{2014} {}", e)),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // ── Sidebar menus ──────────────────────────────────────────
                    Cmd::SidebarQueueRow(i) => {
                        if let Some(uri) = sidebar_tracks.get(i).map(|t| t.uri.clone()) {
                            match queue_one(&mut session, &uri, engine_owns_playback(&last, &engine_device_id)).await {
                                Ok(()) => {
                                    let pool = sidebar_tracks.clone();
                                    mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[&pool]);
                                    if sidebar_mode == 1 {
                                        let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                    }
                                    set_status(&weak, tr!("Added to queue"));
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
                            set_status(&weak, tr!("Queue is empty"));
                        } else {
                            let name = named.clone().unwrap_or_else(|| timestamped_playlist_name(&tr!("Queue")));
                            match session.create_playlist(&name, &uris).await {
                                Ok(pl) => {
                                    set_status(&weak, tr!("Created: {}", pl.name));
                                    if let Ok(p) = session.playlists().await {
                                        playlists = p;
                                        remember_library(&playlists);
                                        thumbs::request(&weak, &playlists);
                                        pl_view = sort_playlist_view(&playlists, &lib_mode, &lib_dir);
                                        push_rows(&weak, build_rows(&playlists, &pl_view, &presence.source, presence.playing));
                                    }
                                }
                                Err(e) => set_status(&weak, tr!("Failed: {}", e)),
                            }
                        }
                    }
                    Cmd::SidebarPlayFirstSelected => {
                        let first = sidebar_tracks
                            .iter()
                            .enumerate()
                            .find(|(i, _)| selected.contains(i))
                            .map(|(_, t)| t.uri.clone());
                        match first {
                            Some(uri) if !uri.is_empty() => {
                                // Only now: with nothing to play, a station or chart must
                                // keep going.
                                end_autoplay_authority(
                                    &weak, &mut session, &mut station_active, &mut station_queued,
                                    &mut bp_autoplay, &mut bp_queued,
                                    &bp_active_seq, &last, &engine_device_id,
                                )
                                .await;
                                launched_src = None;
                                match play_track_list(&mut session, &[uri], 0, 0, &last, &engine_device_id).await {
                                    Ok(_) => forget_mirror(&mut queue_mirror, &mut sidebar_generation, sidebar_mode, &self_tx),
                                    Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
                                }
                                settle_refresh(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                            }
                            _ => set_status(&weak, tr!("Nothing selected")),
                        }
                    }
                    Cmd::SearchBack => {
                        exit_search_drill(
                            &weak, &mut selected, &mut sel_anchor, &mut sel_scope,
                            &mut s_tracks, &mut s_context, &mut s_pager,
                        );
                        set_status(&weak, String::new());
                    }
                    Cmd::LoadMoreSearchResults => {
                        // Every single-type filter pages (the original paged PLAYLISTS
                        // only): 10 more of that type per scroll to the bottom. No
                        // `loading` flag is needed: the worker handles one command at a
                        // time, and the UI closes its own gate the moment it fires.
                        let kind = search_kind(&search_filter);
                        if sp_done || kind.is_empty() || last_query.is_empty() {
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
                                    sp_paged.insert(kind, (sp_offset, sp_done));
                                    // A page only adds rows below: a selection made above
                                    // still names the same rows, so it stays (with its marks,
                                    // see `push_search`) instead of being dropped as a change.
                                    let appended = rows_extend(&search_rows, &rows);
                                    search_actions = actions;
                                    search_rows = rows.clone();
                                    push_search(&weak, rows);
                                    if appended && sel_scope == SEL_SEARCH && sel_version.0 == SEL_SEARCH {
                                        sel_version = (SEL_SEARCH, list_version(SEL_SEARCH as usize));
                                    }
                                }
                                Err(e) => {
                                    sp_done = true;
                                    set_status(&weak, tr!("Search \u{2014} {}", e));
                                }
                            }
                            let more = !sp_done;
                            let _ = weak.upgrade_in_event_loop(move |app| app.set_search_more(more));
                        }
                    }
                    Cmd::LoadMoreSearchTracks => {
                        let before: Vec<String> = s_tracks.iter().map(|t| t.uri.clone()).collect();
                        load_next_search_page(&weak, &mut session, &mut s_pager, &mut s_tracks).await;
                        // A page appends below: the selection still names the same rows, so
                        // it stays, marks and all (`push_search_tracks` carries them over).
                        let after: Vec<String> = s_tracks.iter().map(|t| t.uri.clone()).collect();
                        if rows_extend(&before, &after) && sel_scope == SEL_SEARCH_DRILL && sel_version.0 == SEL_SEARCH_DRILL {
                            sel_version = (SEL_SEARCH_DRILL, list_version(SEL_SEARCH_DRILL as usize));
                        }
                    }
                    Cmd::PlaySearchDrillTrack(i) => {
                        if let Some(t) = s_tracks.get(i).cloned() {
                            end_autoplay_authority(
                                &weak, &mut session, &mut station_active, &mut station_queued,
                                &mut bp_autoplay, &mut bp_queued,
                                &bp_active_seq, &last, &engine_device_id,
                            )
                            .await;
                            prefetch_art(&t.album_art, &last_art);
                            let r = if let Some(ctx) = s_context.clone() {
                                if engine_play_context(&ctx, &t.uri, 0, &last, &engine_device_id) {
                                    Ok(())
                                } else {
                                    session.play_context(&ctx, &t.uri).await
                                }
                            } else {
                                let uris: Vec<String> = s_tracks.iter().map(|x| x.uri.clone()).collect();
                                play_track_list(&mut session, &uris, i, 0, &last, &engine_device_id).await.map(|_| ())
                            };
                            // A search hit isn't a library row, so nothing to highlight.
                            launched_src = None;
                            match r {
                                // The old mirror is the list this play replaced.
                                Ok(()) => forget_mirror(&mut queue_mirror, &mut sidebar_generation, sidebar_mode, &self_tx),
                                Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
                            }
                            refresh_soon(&mut poll_soon);
                        }
                    }
                    Cmd::QueueSearchDrillTrack(i) => {
                        if let Some(uri) = s_tracks.get(i).map(|t| t.uri.clone()) {
                            match queue_one(&mut session, &uri, engine_owns_playback(&last, &engine_device_id)).await {
                                Ok(()) => {
                                    mirror_queued(&mut queue_mirror, &mut sidebar_generation, &[uri], &[&s_tracks]);
                                    if sidebar_mode == 1 {
                                        let _ = self_tx.send(Cmd::RefreshQueueSidebar);
                                    }
                                    set_status(&weak, tr!("Added to queue"));
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
                        // Filtering while it pages: the count and how much it covers move on.
                        if drilled && !filter_of(&TRK_FILTER).is_empty() {
                            let (shown, note) = (trk_view.len() as i32, filter_loaded_note(&pager, tracks.len()));
                            let _ = weak.upgrade_in_event_loop(move |app| {
                                app.set_list_filter_count(shown);
                                app.set_list_filter_note(note.into());
                            });
                        }
                        // The page re-sorted and rebuilt every row; the selection is
                        // keyed to tracks and still stands, so its marks go back on.
                        remark_tracks(&weak, sel_scope, &selected, &trk_view);
                    }
                    Cmd::PlayCurrentTrack => {
                        if let Some(uri) =
                            last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone())
                        {
                            end_autoplay_authority(
                                &weak, &mut session, &mut station_active, &mut station_queued,
                                &mut bp_autoplay, &mut bp_queued,
                                &bp_active_seq, &last, &engine_device_id,
                            )
                            .await;
                            // The original plays it as a bare uri, with no context.
                            launched_src = None;
                            match play_track_list(&mut session, &[uri], 0, 0, &last, &engine_device_id).await {
                                // No context any more: what the mirror listed won't follow.
                                Ok(_) => forget_mirror(&mut queue_mirror, &mut sidebar_generation, sidebar_mode, &self_tx),
                                Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
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
                    Cmd::LavaSaver => {
                        let _ = weak.upgrade_in_event_loop(|a| local_backdrop::start_saver(&a));
                    }
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
                                // Extend from the last clicked row, like any list UI. The
                                // anchor is a key (a track, in the track list), so a re-sort
                                // in between extends from the same song; the first Shift+click
                                // becomes the anchor for the next.
                                let from = anchor_row(scope, *sel_anchor.get_or_insert(key), &trk_view).unwrap_or(row);
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
                                sel_anchor = Some(key);
                            }
                            push_selection(&weak, scope, &selected, &trk_view);
                        }
                    }
                    Cmd::SelectDragBegin { scope, row } => {
                        // A drag in another list starts that list's selection afresh;
                        // in the same list it ADDS to what is already selected.
                        if scope != sel_scope {
                            let old = sel_scope;
                            selected.clear();
                            sel_anchor = None;
                            if old != SEL_NONE {
                                push_selection(&weak, old, &selected, &trk_view);
                            }
                            sel_scope = scope;
                        }
                        let base = selected.clone();
                        let pickable = |r: usize| match scope {
                            SEL_SEARCH => matches!(search_actions.get(r), Some(SearchAction::Track { .. })),
                            SEL_BEATPORT => r < bp_tracks.len(),
                            SEL_SIDEBAR => r < sidebar_tracks.len(),
                            SEL_SEARCH_DRILL => r < s_tracks.len(),
                            _ => r < trk_view.len(),
                        };
                        selected = drag_selection(scope, &base, row, row, &trk_view, pickable);
                        drag_base = Some(base);
                        drag_from = row;
                        drag_last = row;
                        push_selection(&weak, scope, &selected, &trk_view);
                    }
                    Cmd::SelectDragTo { scope, y } if scope == sel_scope && drag_base.is_some() => {
                        let len = match scope {
                            SEL_SEARCH => search_actions.len(),
                            SEL_BEATPORT => bp_tracks.len(),
                            SEL_SIDEBAR => sidebar_tracks.len(),
                            SEL_SEARCH_DRILL => s_tracks.len(),
                            _ => trk_view.len(),
                        };
                        let row = drag_row(scope, y, len);
                        if row != drag_last {
                            drag_last = row;
                            let pickable = |r: usize| match scope {
                                SEL_SEARCH => matches!(search_actions.get(r), Some(SearchAction::Track { .. })),
                                _ => r < len,
                            };
                            let base = drag_base.clone().unwrap_or_default();
                            selected = drag_selection(scope, &base, drag_from, row, &trk_view, pickable);
                            push_selection(&weak, scope, &selected, &trk_view);
                        }
                    }
                    Cmd::SelectDragTo { .. } => {}
                    Cmd::SelectDragEnd => {
                        if drag_base.take().is_some() {
                            // Shift+click then extends from where the drag ended.
                            sel_anchor = sel_key(sel_scope, drag_last, &trk_view);
                        }
                    }
                    // Handled before this match (they never reach it).
                    Cmd::PlaylistNamed(_) | Cmd::PlaylistNameCancelled | Cmd::Local(_) => {}
                    Cmd::ClearSelection => {
                        drag_base = None;
                        sel_anchor = None;
                        // Even with nothing selected here: the UI only sends this when
                        // it believes something is, so its count must be reset or every
                        // later plain click would land here again instead of playing.
                        let scope = sel_scope;
                        selected.clear();
                        sel_scope = SEL_NONE;
                        push_selection(&weak, scope, &selected, &trk_view);
                    }
                    Cmd::PlaySelection => {
                        // The host filters unplayable rows and de-dupes before playing.
                        let uris = scoped_selection_uris(
                            sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, true,
                        );
                        if uris.is_empty() {
                            // Nothing to play: a station or chart keeps going.
                            set_status(&weak, tr!("Selection has no playable tracks"));
                        } else {
                            bp_autoplay = None;
                            bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                            end_autoplay_authority(
                                &weak, &mut session, &mut station_active, &mut station_queued,
                                &mut bp_autoplay, &mut bp_queued,
                                &bp_active_seq, &last, &engine_device_id,
                            )
                            .await;
                            let n = uris.len();
                            let played = play_track_list(&mut session, &uris, 0, 0, &last, &engine_device_id).await;
                            if played.is_ok() {
                                // The old mirror is the list this play replaced; folding it
                                // forward to the first selected song kept it on screen.
                                forget_mirror(&mut queue_mirror, &mut sidebar_generation, sidebar_mode, &self_tx);
                            }
                            match played {
                                Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
                                Ok(false) if n > WEB_API_URIS_MAX => set_status(
                                    &weak,
                                    lightify_i18n::trn!(
                                        "Playing the first {0} of {n} selected",
                                        "Playing the first {0} of {n} selected",
                                        n, WEB_API_URIS_MAX
                                    ),
                                ),
                                Ok(_) => set_status(&weak, lightify_i18n::trn!("Playing {n} selected", "Playing {n} selected", n)),
                            }
                            launched_src = None;
                            refresh_soon(&mut poll_soon);
                        }
                    }
                    Cmd::QueueSelection => {
                        let uris = scoped_selection_uris(
                            sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, true,
                        );
                        if uris.is_empty() {
                            set_status(&weak, tr!("Selection has no playable tracks"));
                        } else {
                            let mut queued = 0usize;
                            let mut failed: Option<String> = None;
                            for uri in &uris {
                                match queue_one(&mut session, uri, engine_owns_playback(&last, &engine_device_id)).await {
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
                                Some(e) => set_status(
                                    &weak,
                                    lightify_i18n::trn!(
                                        "Added {n} to queue, then stopped \u{2014} {}",
                                        "Added {n} to queue, then stopped \u{2014} {}",
                                        queued, e
                                    ),
                                ),
                                None => set_status(&weak, lightify_i18n::trn!("Added {n} to queue", "Added {n} to queue", queued)),
                            }
                        }
                    }
                    Cmd::CreatePlaylistFromSelection => {
                        // Unlike Play/Queue this keeps unplayable rows: the host only
                        // drops them for playback, not for a saved playlist. Named from
                        // the prompt: what was selected when it opened.
                        let uris = named_uris.take().unwrap_or_else(|| {
                            scoped_selection_uris(
                                sel_scope, &selected, &trk_view, &tracks, &search_actions, &sidebar_tracks, &s_tracks, false,
                            )
                        });
                        if uris.is_empty() {
                            set_status(&weak, tr!("Selection has no playlist tracks"));
                        } else {
                            let name = named.clone().unwrap_or_else(|| timestamped_playlist_name(&tr!("Selection")));
                            match session.create_playlist(&name, &uris).await {
                                Ok(pl) => {
                                    set_status(&weak, tr!("Created: {}", pl.name));
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
                                Err(e) => set_status(&weak, tr!("Create playlist \u{2014} {}", e)),
                            }
                        }
                    }

                    Cmd::PlayRow(i) => {
                        // Liked Songs has no "play the whole thing" in the original
                        // (its row carries no dblclick and its menu has no Play), so
                        // row 0 deliberately does nothing — a station or chart included.
                        if let Some((id, uri)) = pl_view
                            .get(i.wrapping_sub(1))
                            .and_then(|&k| playlists.get(k))
                            .map(|p| (p.id.clone(), p.uri.clone()))
                        {
                            if uri.is_empty() {
                                set_status(&weak, tr!("Play \u{2014} playlist has no URI"));
                            } else {
                                bp_autoplay = None;
                                bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                                end_autoplay_authority(
                                    &weak, &mut session, &mut station_active, &mut station_queued,
                                    &mut bp_autoplay, &mut bp_queued,
                                    &bp_active_seq, &last, &engine_device_id,
                                )
                                .await;
                                // The highlight moves only once the play is accepted: a
                                // failed one left this row marked while the old music played.
                                match play_context_from_start(&mut session, &uri, &last, &engine_device_id).await {
                                    Ok(()) => {
                                        launched_src = Some(id);
                                        // The old QUEUE mirror is the list this play replaced.
                                        forget_mirror(&mut queue_mirror, &mut sidebar_generation, sidebar_mode, &self_tx);
                                    }
                                    Err(e) => set_status(&weak, tr!("Play \u{2014} {}", e)),
                                }
                                refresh_soon(&mut poll_soon);
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
                            let msg = tr!("Delete \u{201c}{}\u{201d} from your Spotify library?", name);
                            let _ = weak.upgrade_in_event_loop(move |app| {
                                app.set_confirm_title(tr!("DELETE PLAYLIST").into());
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
                                        Err(e) => set_status(&weak, tr!("Playlists \u{2014} {}", e)),
                                    }
                                    set_status(&weak, tr!("Deleted playlist: {}", name));
                                }
                                Err(e) => set_status(&weak, tr!("Failed to delete playlist: {}", e)),
                            }
                        }
                    }
                    Cmd::QueueTrack { row, uri } => {
                        let i = resolve_track_row(&trk_view, &tracks, row, &uri);
                        if let Some(t) = i.and_then(|i| trk_view.get(i)).and_then(|&k| tracks.get(k)).cloned() {
                            match queue_one(&mut session, &t.uri, engine_owns_playback(&last, &engine_device_id)).await {
                                Ok(()) => {
                                    if let Some(m) = queue_mirror.as_mut() {
                                        m.enqueue(t);
                                    }
                                    persist_mirror(&queue_mirror);
                                    // An open QUEUE shows it within a moment: with no mirror
                                    // to hold it, the next read could otherwise be a minute off
                                    // (an unchanged queue on our engine is read once a minute).
                                    if sidebar_mode == 1 {
                                        spawn_delayed_queue_refresh(&self_tx);
                                    }
                                    set_status(&weak, tr!("Added to queue"));
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
                        // Through the search box's own path (the debounce, now): held
                        // through a rate limit and run when it lifts, answered from the
                        // search cache when it can be. A query sent during a limit used
                        // to be dropped, and one sent after it ended was refused until the
                        // next tick noticed.
                        pending_query = Some(q);
                        set_search_loading(&weak, true);
                        search_deadline = Some(tokio::time::Instant::now());
                    }
                    // The engine's device is listed (or Spotify didn't list it in time):
                    // the rest of what a new engine needs. Only for the engine that is
                    // current — a restart since then has its own.
                    Cmd::EngineAdopted { device_id, result } if device_id == engine_device_id => {
                        match &result {
                            Ok(name) => set_status(&weak, tr!("Playing through {}", name)),
                            Err(e) => set_status(&weak, tr!("Playback device \u{2014} {}", e)),
                        }
                        // The crash-resume and the saved queue go on either way: our engine
                        // plays them itself, so neither a slow listing nor a rate limit on
                        // the read below is a reason to lose them.
                        let limited =
                            refresh_playback(&weak, &mut session, &mut last, &mut last_art, &mut liked).await;
                        if let Some(secs) = limited {
                            backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
                        }
                        if let Some((ctx, track_uri, position_ms)) = resume_after_restart
                            .take()
                            .filter(|_| restart_resume_due(resume_after_restart_at.elapsed()))
                        {
                            // Back after an engine restart: the context when there
                            // is one, else the track plus the list it was in. The
                            // new engine starts empty, so a station's songs (or a
                            // list's) must be handed back, or playback stops when
                            // this song ends with the old queue still on screen.
                            let up_next = restart_up_next(&track_uri, queue_mirror.as_ref(), load_queue().as_ref());
                            let from_list = |up_next: &[String]| {
                                std::iter::once(track_uri.clone()).chain(up_next.iter().cloned()).collect::<Vec<_>>()
                            };
                            let r = match ctx.as_deref() {
                                // A station runs on top of whatever it started from:
                                // its own songs come next, not the context's.
                                _ if station_active && !up_next.is_empty() => {
                                    play_track_list(&mut session, &from_list(&up_next), 0, position_ms, &last, &device_id)
                                        .await
                                        .map(|_| ())
                                }
                                // Rate-limited, or not listed by the Web API yet: the engine
                                // loads the list itself, no Web API needed.
                                Some(c) if (c.starts_with("spotify:playlist:") || c.starts_with("spotify:album:"))
                                    && (limited.is_some() || result.is_err())
                                    && engine_play_context(c, &track_uri, position_ms, &last, &device_id) =>
                                {
                                    Ok(())
                                }
                                Some(c) if c.starts_with("spotify:playlist:") || c.starts_with("spotify:album:") => {
                                    session.play_at(Some(c), &track_uri, position_ms).await
                                }
                                Some(c) if is_liked_context(c)
                                    && engine_play_context(c, &track_uri, position_ms, &last, &device_id) =>
                                {
                                    Ok(())
                                }
                                _ => play_track_list(&mut session, &from_list(&up_next), 0, position_ms, &last, &device_id)
                                    .await
                                    .map(|_| ()),
                            };
                            if r.is_ok() {
                                // A chart lining up appends after its list's first song; the
                                // list was played again from the song that was playing, so
                                // that is its first now. Left on the old one, every later
                                // match was refused and the chart stopped growing (QD9).
                                if let Some(list) = bp_list.as_mut() {
                                    if bp_active_seq.load(std::sync::atomic::Ordering::SeqCst) == list.id {
                                        list.anchor = track_uri.clone();
                                    }
                                }
                                // The new engine's radio flag starts off: a station
                                // has to keep topping itself up past its last song.
                                if station_active {
                                    engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
                                }
                                // What the panel showed was the old engine's queue.
                                if queue_mirror.as_ref().is_none_or(|m| m.current_uri != track_uri) {
                                    queue_mirror = None;
                                    sidebar_generation += 1;
                                    spawn_delayed_queue_refresh(&self_tx);
                                }
                            }
                            match r {
                                Ok(()) => set_status(&weak, tr!("Playback engine restarted \u{2014} resumed")),
                                Err(e) => set_status(&weak, tr!("Resume \u{2014} {}", e)),
                            }
                            refresh_soon(&mut poll_soon);
                        } else if !queue_restored && (limited.is_some() || result.is_err()) {
                            // Restored after the first read that works with our engine as
                            // the player (the ticker), not skipped for this launch and
                            // later overwritten.
                            queue_restore_pending = true;
                        } else if !queue_restored && last.is_some() {
                            if last.as_ref().is_some_and(|p| p.device_id == device_id) {
                                queue_restored = true;
                                restore_queue(&weak, &mut session, &last, &mut queue_mirror, launch_queue.take()).await;
                            } else {
                                // Another device is playing (launch left it alone, Q2): the
                                // saved queue isn't for its music. Held for when ours plays.
                                queue_restore_pending = true;
                            }
                        }
                        // (Nothing playing at all — a resume offer, say: the queue waits
                        // for that play, `Cmd::TogglePlay`.)
                        if devices_open(&weak) {
                            if let Ok(d) = session.devices().await {
                                push_devices(&weak, build_device_rows(&d));
                                devices = d;
                            }
                        }
                    }
                    Cmd::EngineAdopted { .. } => {}
                    Cmd::Engine(ev) => match ev {
                        engine::Event::OutputChanged { device } => {
                            // Only announce actual moves, not the first open at startup -
                            // "Audio output -> Speakers" on every launch is noise.
                            if !system_default_device.is_empty() && system_default_device != device {
                                set_status(&weak, tr!("Audio output \u{2192} {}", device));
                            }
                            system_default_device = device;
                            push_outputs(&weak);
                        }
                        engine::Event::Ready { device_id } => {
                            // Healthy only once it has stayed up a while (`EngineWatchdog`):
                            // an output that dies right after opening must still use up the
                            // restart budget, not refill it on every Ready.
                            engine_watchdog.note_ready(std::time::Instant::now());
                            push_engine_status(&weak, || tr!("Running \u{00B7} Lightify"), true);
                            // Record it regardless of whether the adoption below succeeds —
                            // the id is valid the moment spirc reports it, and
                            // `engine_owns_playback` needs it to recognise OUR device even if
                            // the Web API hasn't listed it yet. Every later player call is
                            // aimed at it straight away, too.
                            engine_device_id = device_id.clone();
                            if let Ok(mut d) = ENGINE_DEVICE_ID.lock() {
                                *d = device_id.clone();
                            }
                            session.remember_device(&device_id);
                            // Waiting for `me/player/devices` to list it takes up to ~7 s;
                            // that wait runs beside the command loop now, not inside it
                            // (Play/Pause/Next used to sit behind it). `EngineAdopted` picks up
                            // from here.
                            let tx = self_tx.clone();
                            tokio::spawn(async move {
                                let result = match Session::load() {
                                    Ok(mut s) => adopt_engine_device(&mut s, &device_id).await,
                                    Err(e) => Err(e),
                                };
                                let _ = tx.send(Cmd::EngineAdopted { device_id, result });
                            });
                        }
                        // The engine knows about play/pause and seeks the instant they
                        // happen; trusting it beats waiting up to a poll interval.
                        engine::Event::Playing { playing } => {
                            // A song loaded paused (a resume offer) goes into RECENT now.
                            play_history::engine_playing(playing);
                            show_new_plays(&weak, sidebar_mode, &mut sidebar_tracks, &recent_spotify, &current_id(&last));
                            // Spotify started: Local gives way — unless a Local song started
                            // a moment ago, which paused the engine; a `Playing(true)` now is
                            // the engine still catching up with the play before that.
                            if playing && spotify_play_stops_local(local_started_at) {
                                local_ui::spotify_started(&mut local, &weak);
                            }
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
                            // Paused: that is where a later launch should offer to resume.
                            if was_playing && !playing {
                                if let Some(pb) = progress_clock.live(&last) {
                                    save_resume(&pb);
                                }
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
                        // Authoritative for our own device — except for the in-between
                        // steps of a change the user just made (see
                        // `accept_device_options`).
                        engine::Event::Options { shuffle, repeat } => {
                            let (shuffle, repeat) = accept_device_options(shuffle, repeat);
                            if let Some(r) = repeat.as_deref() {
                                set_repeat_choice(r);
                            }
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
                        engine::Event::Track { name, artists, id, uri, artist_ids, album, duration_ms, cover } => {
                            // (Skipping out of repeat-one is mirrored by `Cmd::Next`, not
                            // here: a track change can also be "previous", which keeps
                            // repeat-one, or a new load, which resets repeat entirely.)
                            //
                            // Did this track arrive because we asked Spotify to play
                            // something? Then it's a load, and the load reset repeat.
                            let requests = lightify_core::play_requests();
                            let loaded = requests != seen_play_requests
                                && lightify_core::last_play_request_age().is_some_and(|a| a < PLAY_LOAD_WINDOW);
                            seen_play_requests = requests;
                            engine_track_name = name.clone();
                            // Ahead of the title, which is what makes the media flyout
                            // look: this way it picks up both together.
                            #[cfg(windows)]
                            media_controls::set_art_url(&cover);
                            if !name.is_empty() {
                                let (n, a) = (name.clone(), artists.clone());
                                let _ = weak.upgrade_in_event_loop(move |app| {
                                    app.set_track_name(n.into());
                                    app.set_track_artist(a.into());
                                });
                            }
                            let engine_track = (!id.is_empty()).then(|| Track {
                                id: id.clone(),
                                name: name.clone(),
                                artists: artists.clone(),
                                artist_ids,
                                album,
                                album_id: String::new(),
                                duration_ms,
                                uri,
                                album_art: cover,
                                added_at: None,
                                is_playable: true,
                            });
                            set_engine_now(&engine_device_id, engine_track.clone());
                            // RECENT: the track before this one is over — counted here,
                            // from our engine's own events (play_history.rs).
                            if let Some(t) = engine_track.clone() {
                                play_history::engine_track(t);
                                show_new_plays(&weak, sidebar_mode, &mut sidebar_tracks, &recent_spotify, &id);
                            }
                            let engine_loaded = take_engine_load();
                            let owned = engine_owns_playback(&last, &engine_device_id);
                            if let (true, false, Some(t), Some(mut pb)) = (owned, loaded, engine_track, last.clone()) {
                                // The next song of what we are already playing: everything
                                // the screen needs came with the event, so no Web-API read
                                // (this used to cost 2–6 `me/player` reads per song). The
                                // context, shuffle and repeat are unchanged by a track end.
                                pb.track = Some(t);
                                pb.duration_ms = duration_ms;
                                pb.progress_ms = 0;
                                save_resume(&pb);
                                show_track_extras(&weak, &mut session, &pb, &mut last_art, &mut liked).await;
                                last = Some(pb);
                                track_hook_due = true;
                                if engine_loaded && backoff_until.is_none() {
                                    // Something new was loaded: one read for its context.
                                    poll_soon = Some(tokio::time::Instant::now() + Duration::from_millis(2500));
                                }
                            } else if backoff_until.is_none() {
                                if loaded {
                                    restore_repeat_after_load(&weak, &mut session, &mut last, &engine_device_id, &name).await;
                                }
                                // On the loop's timer, not inline: the play that caused
                                // this already scheduled one, and the two now coalesce
                                // into a single read instead of two blocking ones.
                                refresh_soon(&mut poll_soon);
                                // ...and keep re-reading briefly until the Web API
                                // reports this track (5 × 600 ms, then the normal poll).
                                track_catchup = 5;
                            }
                        }
                        engine::Event::Account { username, account_type, catalogue } => {
                            let problem = streaming_account_problem(&username, &account_type, &catalogue, session.user_id());
                            if let Some(p) = problem.as_deref() {
                                set_status(&weak, p.to_string());
                                let p2 = p.to_string();
                                push_engine_status(&weak, move || p2.clone(), false);
                            }
                            let text = problem.unwrap_or_default();
                            let _ = weak.upgrade_in_event_loop(move |app| app.set_engine_account_problem(text.into()));
                        }
                        engine::Event::Inactive => {
                            // Another device took over: the engine's track is no longer
                            // the truth, and the screen should follow the new device.
                            set_engine_now("", None);
                            // A chart lining up on our engine can't grow any more (the
                            // engine drops appends while inactive): stop matching it, which
                            // spends the shared Web-API quota, and drop its mirror, which
                            // would list songs that never play here (BUGS-PLAN QD10).
                            if bp_list.take().is_some() {
                                bp_autoplay = None;
                                bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                                queue_mirror = None;
                                sidebar_generation += 1;
                                persist_mirror(&queue_mirror);
                            }
                            play_history::engine_stopped();
                            show_new_plays(&weak, sidebar_mode, &mut sidebar_tracks, &recent_spotify, "");
                            poll_soon = Some(tokio::time::Instant::now() + Duration::from_millis(1500));
                        }
                        engine::Event::NeedAuth { msg } => {
                            let msg = if msg.trim().is_empty() {
                                tr!("Finish the Spotify streaming sign-in in your browser")
                            } else {
                                msg
                            };
                            set_status(&weak, msg);
                        }
                        // The engine is fine and the music goes on: nothing to mark as
                        // stopped, and no "Error" on the engine's line in Settings.
                        engine::Event::Notice { msg } => set_status(&weak, msg),
                        engine::Event::Failed { msg } => {
                            play_history::engine_stopped();
                            show_new_plays(&weak, sidebar_mode, &mut sidebar_tracks, &recent_spotify, "");
                            let m = msg.clone();
                            push_engine_status(&weak, move || tr!("Error \u{2014} {}", m), false);
                            set_status(&weak, tr!("Playback engine \u{2014} {}", msg));
                        }
                        // An answer to a station the user has since abandoned (see
                        // `STATION_EPOCH`): queueing it now would put the OLD station's
                        // tracks in front of whatever they moved on to.
                        engine::Event::StationTracks { seq, .. } if !station_epoch_is_current(seq) => {
                            set_status(&weak, tr!("Ignored a station that was no longer wanted"));
                        }
                        engine::Event::StationTracks { uris, seq }
                            if search_station.as_ref().is_some_and(|(s, _)| *s == seq) =>
                        {
                            let (_, anchor) = search_station.take().unwrap_or_default();
                            let mut seen = std::collections::HashSet::from([anchor.clone()]);
                            let picked: Vec<String> = uris.into_iter().filter(|u| seen.insert(u.clone())).collect();
                            if !picked.is_empty() {
                                let n = picked.len();
                                // Appended to the song's own list, so it is just "what
                                // comes next": Clear, Next and a new play all treat it
                                // like any list, and there is no radio lane to end.
                                engine::send(
                                    &serde_json::json!({ "cmd": "appendtracks", "anchor": anchor, "uris": picked }).to_string(),
                                );
                                set_status(
                                    &weak,
                                    lightify_i18n::trn!(
                                        "Station \u{2014} {n} similar tracks after this one",
                                        "Station \u{2014} {n} similar tracks after this one",
                                        n
                                    ),
                                );
                                if sidebar_mode == 1 {
                                    queue_mirror = None;
                                    spawn_delayed_queue_refresh(&self_tx);
                                }
                            }
                        }
                        engine::Event::StationTracks { uris, .. } => {
                            // Drop the seed itself and anything already queued-worthy
                            // duplicate, then either make the station what follows the
                            // current song or — if nothing is playing — start it outright.
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
                                .collect();
                            // A song paused on our own engine is kept too: the station goes
                            // behind it, as for a playing one (it used to be dropped for the
                            // station's first track). Nothing starts playing by itself.
                            let paused_here = !current.is_empty()
                                && last.as_ref().is_some_and(|p| !p.is_playing)
                                && engine_owns_playback(&last, &engine_device_id);
                            if picked.is_empty() {
                                set_status(&weak, tr!("Station had no tracks to queue"));
                            } else if last.as_ref().is_some_and(|p| p.is_playing) || paused_here {
                                // The station now owns the queue lane.
                                station_active = true;
                                engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
                                // The station REPLACES what follows the song, the way playing
                                // a playlist replaces it. It used to be put in the user
                                // queue instead, which plays ahead of the old list but
                                // leaves all of it behind: the queue read as the station's
                                // first track or so, then the rest of whatever played before.
                                // Hand-queued songs stay ahead of it, as on any play.
                                let result = if engine_owns_playback(&last, &engine_device_id) {
                                    // Local, and the song keeps playing untouched.
                                    engine::send(&serde_json::json!({ "cmd": "replacenext", "uris": picked }).to_string());
                                    Ok(())
                                } else {
                                    // Another device: the Web API has no "replace what's
                                    // next", so the song is played again as the head of the
                                    // station's list, from where it was.
                                    let position_ms = last.as_ref().map(|p| p.progress_ms).unwrap_or(0);
                                    let list: Vec<String> = std::iter::once(current.clone())
                                        .filter(|u| !u.is_empty())
                                        .chain(picked.iter().cloned())
                                        .take(WEB_API_URIS_MAX)
                                        .collect();
                                    match session.play_uris(&list).await {
                                        Ok(()) if position_ms > 0 => session.seek(position_ms).await,
                                        other => other,
                                    }
                                };
                                match result {
                                    Err(e) => set_status(&weak, tr!("Station \u{2014} {}", e)),
                                    Ok(()) => {
                                        launched_src = None;
                                        set_status(
                                            &weak,
                                            lightify_i18n::trn!(
                                                "Station queued \u{2014} {n} tracks after this one",
                                                "Station queued \u{2014} {n} tracks after this one",
                                                picked.len()
                                            ),
                                        );
                                        // The old mirror is the list the station replaced. No
                                        // `Track`s for the station yet (the resolve only
                                        // returns uris), so the panel reads Spotify's queue
                                        // once the device has published it.
                                        let hand: Vec<String> = queue_mirror
                                            .as_ref()
                                            .map(|m| m.upcoming[..m.queued.min(m.upcoming.len())].iter().map(|t| t.uri.clone()).collect())
                                            .unwrap_or_default();
                                        let upcoming: Vec<String> = hand.iter().chain(picked.iter()).cloned().collect();
                                        save_queue_marked(&current, &upcoming, Some(hand.len()));
                                        queue_mirror = None;
                                        sidebar_generation += 1;
                                        if sidebar_mode == 1 {
                                            sidebar_tracks.clear();
                                            set_sidebar(&weak, 1, vec![], tr!("Loading queue\u{2026}"));
                                        }
                                        spawn_delayed_queue_refresh(&self_tx);
                                    }
                                }
                            } else {
                                // Nothing playing: this is the one case where a station
                                // should take over, because there is nothing to disturb.
                                station_active = true;
                                engine::send(&serde_json::json!({ "cmd": "autoplay", "enabled": true }).to_string());
                                launched_src = None;
                                if let Err(e) = play_track_list(&mut session, &picked, 0, 0, &last, &engine_device_id).await {
                                    set_status(&weak, tr!("Station \u{2014} {}", e));
                                } else {
                                    set_status(
                                        &weak,
                                        lightify_i18n::trn!("Station started \u{2014} {n} tracks", "Station started \u{2014} {n} tracks", picked.len()),
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
                            push_engine_status(&weak, || tr!("Not running"), false);
                            set_engine_now("", None);
                            // What our device was playing, before marking it stopped.
                            let playing_here = last
                                .as_ref()
                                .filter(|p| p.is_playing && !engine_device_id.is_empty() && p.device_id == engine_device_id)
                                .and_then(|p| {
                                    p.track.as_ref().map(|t| (p.context_uri.clone(), t.uri.clone(), p.progress_ms))
                                });
                            if let Some(pb) = last.as_mut() {
                                pb.is_playing = false;
                            }
                            let _ = weak.upgrade_in_event_loop(|app| app.set_playing(false));
                            // Ask who owns playback now — that is what separates "the
                            // user moved to another device" from "our audio sink died".
                            let (moved, to_downloader) = match session.devices().await {
                                Ok(devs) => (another_device_active(&devs, &engine_device_id), downloader_active(&devs)),
                                // No answer: assume nothing took over, since the sink
                                // failure is the case worth recovering from.
                                Err(_) => (false, false),
                            };
                            // That engine's device is gone: until the next one registers,
                            // player commands take the Web API (and say so if it fails)
                            // instead of going to an engine that drops them.
                            forget_engine_device(&mut engine_device_id);
                            let login_recent = dl_login_at.is_some_and(|t| t.elapsed() < DL_LOGIN_RECLAIM_WINDOW);
                            if moved && to_downloader && login_recent {
                                // Our own downloader sign-in took the device ("OnTheSpot"
                                // picked in Spotify's list): routine, so take it back. A
                                // phone or another app taking over is left alone (below).
                                dl_login_at = None;
                                set_status(&weak, tr!("Reclaiming playback after downloader sign-in\u{2026}"));
                                push_engine_status(&weak, || tr!("Restarting\u{2026}"), false);
                                start_engine_after(&weak, &session, &self_tx, Duration::from_millis(1500));
                            } else if moved {
                                set_status(
                                    &weak,
                                    tr!("Playback moved to another Spotify device \u{2014} Lightify\u{2019}s player stopped. Settings \u{2192} Restart to play here again."),
                                );
                            } else if engine_watchdog.should_restart(std::time::Instant::now()) {
                                // Our device was playing: carry on where it was once the
                                // new engine is up, rather than leaving it silent.
                                if playing_here.is_some() {
                                    resume_after_restart = playing_here;
                                    resume_after_restart_at = std::time::Instant::now();
                                }
                                set_status(
                                    &weak,
                                    tr!("Playback engine stopped (audio device lost?) \u{2014} restarting\u{2026}"),
                                );
                                push_engine_status(&weak, || tr!("Restarting\u{2026}"), false);
                                // A beat, so a hard-failing engine cannot spin, and the
                                // OS has a moment to settle its new default device. Waited
                                // out beside the command loop, not in it.
                                start_engine_after(&weak, &session, &self_tx, Duration::from_secs(2));
                            } else {
                                set_status(
                                    &weak,
                                    tr!("Playback engine keeps stopping \u{2014} check the audio output in Settings, then Restart"),
                                );
                            }
                        }
                    },
                    Cmd::SelectOutput(i) => {
                        // Row 0 is "System default" (stored as an empty string, the same
                        // convention the shipped host's config uses).
                        let shown = SHOWN_OUTPUTS.lock().ok().and_then(|g| g.get(i.wrapping_sub(1)).cloned());
                        let chosen = if i == 0 {
                            String::new()
                        } else if let Some(name) = shown {
                            name
                        } else {
                            // Not a row of the list on screen: say so and redraw it,
                            // rather than silently saving "System default".
                            set_status(&weak, tr!("That output list changed \u{2014} pick again"));
                            push_outputs(&weak);
                            continue;
                        };
                        let dir = lightify_core::config::data_dir();
                        match lightify_core::config::update_config(&dir, |cfg| cfg.audio_output = chosen.clone()) {
                            Ok(()) => {
                                let label = if chosen.is_empty() { tr!("the system default") } else { chosen.clone() };
                                set_status(&weak, tr!("Audio output \u{2192} {}", label));
                                // Live, no restart: the engine's output thread moves to it
                                // on its next pass. An engine that is not running picks
                                // the saved choice up when it next starts.
                                if engine::running() {
                                    engine::send(
                                        &serde_json::json!({ "cmd": "output", "device": chosen }).to_string(),
                                    );
                                } else {
                                    // Local playback may hold an output of its own
                                    // meanwhile: move it too. The engine is not started
                                    // for this — a stopped engine is usually one the user
                                    // left for another device, and starting it takes
                                    // playback back (`adopt_engine_device` transfers).
                                    audio_output::set_target(Some(chosen.clone()));
                                }
                                push_outputs(&weak);
                            }
                            Err(e) => set_status(&weak, tr!("Settings \u{2014} {}", e)),
                        }
                    }
                    Cmd::StreamingSignInAgain => {
                        // The player's credentials go, so the new engine asks for the
                        // one-time browser sign-in again; nothing else is touched.
                        // Like `RestartEngine`: the old engine's play state goes with it,
                        // or the UI kept showing "playing" on a dead engine.
                        let ours = engine_owns_playback(&last, &engine_device_id);
                        // Retired now; stopped by `start_engine`, beside the command loop.
                        engine::retire();
                        set_engine_now("", None);
                        forget_engine_device(&mut engine_device_id);
                        if ours {
                            if let Some(pb) = last.as_mut() {
                                pb.is_playing = false;
                            }
                            let _ = weak.upgrade_in_event_loop(|app| app.set_playing(false));
                        }
                        let dir = engine::cache_dir();
                        let _ = std::fs::remove_file(dir.join("credentials.json"));
                        let _ = std::fs::remove_file(dir.join(".lightify_stream_oauth_v1"));
                        let _ = weak.upgrade_in_event_loop(|app| app.set_engine_account_problem("".into()));
                        set_status(&weak, tr!("Sign in to Spotify in your browser with your Premium account"));
                        push_engine_status(&weak, || tr!("Restarting\u{2026}"), false);
                        start_engine(&weak, &session, &self_tx);
                    }
                    Cmd::RestartEngine => {
                        push_engine_status(&weak, || tr!("Restarting\u{2026}"), false);
                        // `shutdown` retires the old engine's generation, so its `Exited`
                        // never arrives: what that handler clears, clear here — the old
                        // engine's track and play state must not outlive it.
                        let ours = engine_owns_playback(&last, &engine_device_id);
                        // Retired at once (its events stop counting); `start_engine` stops it
                        // and starts the new one beside the command loop, not in it.
                        engine::retire();
                        set_engine_now("", None);
                        forget_engine_device(&mut engine_device_id);
                        if ours {
                            if let Some(pb) = last.as_mut() {
                                pb.is_playing = false;
                            }
                            let _ = weak.upgrade_in_event_loop(|app| app.set_playing(false));
                        }
                        start_engine(&weak, &session, &self_tx);
                    }
                    // Only noted: the engine is taken back in `Event::Exited`, and only if
                    // this sign-in's "OnTheSpot" is what knocked it off. An engine that
                    // was already stopped (the user moved to the phone) stays stopped.
                    Cmd::DownloaderLoginStarted => dl_login_at = Some(std::time::Instant::now()),
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

/// Long lists play whole: a uri list goes to the engine uncapped (it was cut to 50, so
/// Liked Songs stopped after 50), Liked Songs starts as its real context in the order
/// the list shows, and repeat survives the load. Against the real account; same
/// conditions as `--selftest-queue-order` (Lightify closed, a few quiet seconds).
async fn selftest_long_lists() {
    println!("long lists: uncapped track lists + Liked Songs as a context");
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
    let mut liked: Vec<String> = Vec::new();
    for offset in [0, 50, 100] {
        if let Ok(p) = session.saved_tracks_page(offset).await {
            liked.extend(p.tracks.into_iter().map(|t| t.uri).filter(|u| !u.is_empty()));
        }
    }
    if liked.len() < 150 {
        return println!("  need 150 liked songs (have {})\nFAIL", liked.len());
    }
    let mut fails: Vec<String> = Vec::new();
    let mut check = |cond: bool, what: &str| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };
    let none: Option<PlaybackState> = None;
    // Wait for the device to report `want` as the playing track.
    async fn playing(session: &mut Session, want: &str) -> Option<PlaybackState> {
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(700)).await;
            if let Ok(Some(pb)) = session.playback().await {
                if pb.track.as_ref().is_some_and(|t| t.uri == want) {
                    return Some(pb);
                }
            }
        }
        None
    }

    // 1. 150 uris, started at #70: plays #70, #71.. follow.
    let ok = play_track_list(&mut session, &liked, 70, 0, &none, &device).await;
    check(ok == Ok(true), "a uri list goes to the engine, not the capped Web API");
    check(playing(&mut session, &liked[70]).await.is_some(), "starts at the clicked row (#70 of 150)");
    let q = queue_uris(&mut session).await;
    check(q.first() == Some(&liked[71]), "the next row follows it");

    // 2. Near the end: everything to the end of the list is still there, past
    // where the old cap of 50 would have cut it off.
    let _ = play_track_list(&mut session, &liked, 140, 0, &none, &device).await;
    check(playing(&mut session, &liked[140]).await.is_some(), "starts at #140 of 150");
    let q = queue_uris(&mut session).await;
    check(q == liked[141..150].to_vec(), "up-next is exactly #141..#149");

    // 3. Liked Songs as its context, from row #60, with repeat carried by the load.
    let ctx = format!("spotify:user:{}:collection", session.user_id());
    let with_repeat = Some(PlaybackState { repeat_state: "context".into(), ..resume_as_playback(&ResumePoint {
        track_uri: String::new(), context_uri: None, position_ms: 0, duration_ms: 0,
        name: String::new(), artists: String::new(), album_art: String::new(),
    }) });
    check(engine_play_context(&ctx, &liked[60], 0, &with_repeat, &device), "Liked Songs context goes to the engine");
    let pb = playing(&mut session, &liked[60]).await;
    check(pb.is_some(), "Liked Songs context starts at the clicked row (#60)");
    check(
        pb.as_ref().and_then(|p| p.context_uri.as_deref()).is_some_and(is_liked_context),
        "playback reports the Liked Songs context",
    );
    check(pb.as_ref().is_some_and(|p| p.repeat_state == "context"), "repeat survives the load");
    let q = queue_uris(&mut session).await;
    check(
        q.len() >= 10 && q[..10] == liked[61..71],
        "the context continues in the list's own (newest-first) order",
    );

    engine::send(&serde_json::json!({ "cmd": "repeat", "mode": "off", "from": "context" }).to_string());
    let _ = session.pause().await;
    if let Some(v) = volume_before {
        let _ = session.set_volume(v).await;
    }
    if fails.is_empty() {
        println!("PASS: long lists play whole");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// A Beatport chart plays as one list that keeps growing in chart order (it used to
/// stop after the first 4–5), a song played from search gets its station appended
/// behind it, and a station started mid-list replaces the rest of it. Real chart (Overall top 100, first 15 rows), real account; same
/// conditions as `--selftest-long-lists`.
async fn selftest_bp_chart() {
    println!("beatport chart + search station: lists that grow while they play");
    println!("{:-<72}", "");
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => return println!("  session: {e}\nFAIL"),
    };
    if let Err(e) = session.ensure_fresh().await {
        return println!("  session: {e}\nFAIL");
    }
    let chart = match session.beatport_chart("", "tracks").await {
        Ok(t) if t.len() >= 15 => t.into_iter().take(15).collect::<Vec<_>>(),
        Ok(t) => return println!("  chart: only {} rows\nFAIL", t.len()),
        Err(e) => return println!("  chart: {e}\nFAIL"),
    };
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
    // Songs queued by hand outlive every play and every transfer (as on Spotify), so a
    // song an earlier run queued (step 4 below) came back ahead of this run's lists and
    // failed its order checks. Nothing plays yet: a hard reset clears them.
    engine::send(&serde_json::json!({ "cmd": "clearqueue" }).to_string());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut fails: Vec<String> = Vec::new();
    let mut check = |cond: bool, what: &str| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };
    let none: Option<PlaybackState> = None;

    // 1. Chart: the first match plays, the rest is matched and appended in order.
    let mut first_at = 0;
    let mut first = None;
    while first.is_none() && first_at < chart.len() {
        first = session.beatport_match(&chart[first_at].name, &chart[first_at].artists).await.ok().flatten();
        first_at += 1;
    }
    let Some(first) = first else { return println!("  no chart row matched\nFAIL") };
    let ok = play_track_list(&mut session, std::slice::from_ref(&first.uri), 0, 0, &none, &device).await;
    check(ok == Ok(true), "the chart's first match plays on our engine");
    let active = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(7));
    let (ctx, mut crx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    let seen = std::collections::HashSet::from([first.uri.clone()]);
    tokio::spawn(bp_match_background(chart.clone(), first_at, seen, 7, active.clone(), ctx));
    let mut appended: Vec<String> = Vec::new();
    let until = tokio::time::Instant::now() + Duration::from_secs(90);
    while tokio::time::Instant::now() < until {
        match tokio::time::timeout(Duration::from_secs(5), crx.recv()).await {
            Ok(Some(Cmd::BpMatched { tracks, done, error, .. })) => {
                if let Some(e) = error {
                    println!("  (matcher retrying: {e})");
                }
                let uris: Vec<String> = tracks.into_iter().map(|t| t.uri).collect();
                if !uris.is_empty() {
                    engine::send(
                        &serde_json::json!({ "cmd": "appendtracks", "anchor": first.uri, "uris": uris }).to_string(),
                    );
                    appended.extend(uris);
                }
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    println!("  matched {} more of the {} chart rows after the first", appended.len(), chart.len() - first_at);
    check(appended.len() >= 6, "well past the old 4–5 tracks");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let q = queue_uris_settled(&mut session).await;
    let want: Vec<String> = appended.iter().take(q.len()).cloned().collect();
    if q.is_empty() || q != want {
        let pb = session.playback().await.ok().flatten();
        println!(
            "      web api sees: device={:?} track={:?} (first match {})",
            pb.as_ref().map(|p| p.device_id == device),
            pb.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()),
            first.uri
        );
        println!("      up-next ({}): {:?}\n      chart  ({}): {:?}", q.len(), q, appended.len(), appended);
    }
    check(!q.is_empty() && q == want, "up-next is the rest of the chart, in chart order");
    // Abandoning the chart stops the matcher.
    active.store(0, std::sync::atomic::Ordering::SeqCst);

    // 2. An append meant for a list that's gone is dropped.
    let other = appended.first().cloned().unwrap_or_default();
    let _ = play_track_list(&mut session, std::slice::from_ref(&other), 0, 0, &none, &device).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    engine::send(&serde_json::json!({ "cmd": "appendtracks", "anchor": first.uri, "uris": [first.uri] }).to_string());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    check(queue_uris(&mut session).await.is_empty(), "a stale append doesn't land in the new list");

    // 3. Search song: station appended behind it.
    let seed = other;
    let _ = play_track_list(&mut session, std::slice::from_ref(&seed), 0, 0, &none, &device).await;
    let station = resolve_station(&rx, &seed);
    let picked: Vec<String> = station.into_iter().filter(|u| *u != seed).collect();
    println!("  station resolved {} tracks", picked.len());
    engine::send(&serde_json::json!({ "cmd": "appendtracks", "anchor": seed, "uris": picked }).to_string());
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let pb = session.playback().await.ok().flatten();
    check(
        pb.as_ref().and_then(|p| p.track.as_ref()).is_some_and(|t| t.uri == seed),
        "the song keeps playing while its station is added",
    );
    let q = queue_uris_settled(&mut session).await;
    if q.len() < 10 || q[..10] != picked[..10.min(picked.len())] {
        println!("      up-next ({}): {:?}\n      station ({}): {:?}", q.len(), &q[..q.len().min(12)], picked.len(), &picked[..picked.len().min(12)]);
    }
    check(
        q.len() >= 10 && q[..10] == picked[..10.min(picked.len())],
        "the station follows the song, in order",
    );

    // 4. Start station in the middle of a list: the station replaces the rest of the
    // list (it used to be queued ahead of it, leaving the whole old list behind), the
    // song keeps playing, and a hand-queued song stays first.
    let list: Vec<String> = appended.iter().take(8).cloned().collect();
    if list.len() < 2 {
        return println!("  need a list of 2+ matched chart tracks\nFAIL");
    }
    let _ = play_track_list(&mut session, &list, 0, 0, &none, &device).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let hand = seed.clone();
    engine::send(&serde_json::json!({ "cmd": "addtoqueue", "uris": [hand] }).to_string());
    let station: Vec<String> = resolve_station(&rx, &list[0])
        .into_iter()
        .filter(|u| *u != list[0] && *u != hand)
        .collect();
    let before = session.playback().await.ok().flatten();
    engine::send(&serde_json::json!({ "cmd": "replacenext", "uris": station }).to_string());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let after = session.playback().await.ok().flatten();
    check(
        after.as_ref().and_then(|p| p.track.as_ref()).is_some_and(|t| t.uri == list[0])
            && after.as_ref().map(|p| p.progress_ms) >= before.as_ref().map(|p| p.progress_ms),
        "the song keeps playing, not restarted, while its station replaces the list",
    );
    let q = queue_uris_settled(&mut session).await;
    let old_left = list[1..].iter().filter(|u| !station.contains(u) && q.contains(u)).count();
    check(old_left == 0, &format!("none of the old list is left after the station ({old_left} left)"));
    check(q.first() == Some(&hand), "the hand-queued song still plays first");
    let n = 8.min(station.len());
    check(q.len() > n && q[1..=n] == station[..n], "then the station, in order");

    // Leave the account's queue as it was: the song this test queued by hand goes.
    engine::send(&serde_json::json!({ "cmd": "removefromqueue", "uri": hand }).to_string());
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = session.pause().await;
    if let Some(v) = volume_before {
        let _ = session.set_volume(v).await;
    }
    if fails.is_empty() {
        println!("PASS: charts, search songs and stations keep playing");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
    }
}

/// Every uri in the device's up-next, in order.
async fn queue_uris(session: &mut Session) -> Vec<String> {
    session.queue().await.unwrap_or_default().into_iter().map(|t| t.uri).collect()
}

/// `queue_uris` for an up-next that should NOT be empty: Spotify publishes the engine's
/// new list a few seconds late, and an early read answers with an empty queue (seen
/// live 2026-10-31, at random on either check of `--selftest-bp-chart`).
async fn queue_uris_settled(session: &mut Session) -> Vec<String> {
    for _ in 0..8 {
        let q = queue_uris(session).await;
        if !q.is_empty() {
            return q;
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
    Vec::new()
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
    // Any other full context uri (a playlist, `spotify:user:<id>:collection`, ...)
    // is resolved as-is instead of as a station seed.
    let raw = seed.starts_with("spotify:") && !seed.starts_with("spotify:track:");
    let seed = if seed.starts_with("spotify:") {
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
    // `liked` = this account's Liked Songs context.
    let (raw, seed) = if seed == "spotify:track:liked" {
        (true, format!("spotify:user:{}:collection", session.user_id()))
    } else {
        (raw, seed)
    };
    println!("  resolving: {seed}");
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
                    &serde_json::json!({ "cmd": "stationtracks", "context_uri": seed, "raw": raw }).to_string(),
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

/// How long a click takes to become sound, on our own engine: the same `loadtracks` /
/// `loadcontext` commands a row click sends, timed from the send to the engine's
/// track event and to its first `Playing`. The engine log carries millisecond stamps,
/// so the steps in between (activate, resolve, load) are printed after each case.
/// Live: plays a few seconds of four tracks from the user's own library.
async fn probe_start() {
    println!("start-latency probe (click to sound, on our own engine)");
    println!("{:-<72}", "");
    let Some(bin) = engine::find_binary() else {
        println!("FAIL engine binary not found");
        return;
    };
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => return println!("FAIL session: {e}"),
    };
    if let Err(e) = session.ensure_fresh().await {
        return println!("FAIL auth: {e}");
    }
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let output = engine::resolve_output(&configured).unwrap_or(None);
    let (tx, rx) = std::sync::mpsc::channel::<engine::Event>();
    if let Err(e) = engine::start(&bin, session.access_token(), output.as_deref(), move |ev| {
        let _ = tx.send(ev);
    }) {
        return println!("FAIL spawn: {e}");
    }
    let started = std::time::Instant::now();
    let mut device_id = String::new();
    while started.elapsed() < Duration::from_secs(45) && device_id.is_empty() {
        if let Ok(engine::Event::Ready { device_id: id }) = rx.recv_timeout(Duration::from_millis(200)) {
            device_id = id;
        }
    }
    if device_id.is_empty() {
        return println!("FAIL the engine never registered");
    }
    println!("  engine ready in {} ms", started.elapsed().as_millis());
    if let Err(e) = adopt_engine_device(&mut session, &device_id).await {
        return println!("FAIL adopt: {e}");
    }

    let liked = session.saved_tracks(4).await.unwrap_or_default();
    if liked.len() < 4 {
        return println!("FAIL need 4 Liked Songs to test with");
    }
    // A playlist this token can read (dev mode refuses some), for a second context.
    let mut playlist = None;
    for p in session.playlists().await.unwrap_or_default().into_iter().filter(|p| p.tracks >= 5).take(6) {
        if let Ok(t) = session.playlist_tracks(&p.id, 3).await {
            if let Some(row) = t.get(1) {
                playlist = Some((p, row.uri.clone()));
                break;
            }
        }
    }
    let one = |uri: &str| serde_json::json!({ "cmd": "loadtracks", "uris": [uri], "index": 0 });
    let ctx_load = |ctx: &str, uri: &str| serde_json::json!({ "cmd": "loadcontext", "context_uri": ctx, "track_uri": uri });
    // (label, context to prepare first, the load)
    let mut cases: Vec<(String, Option<String>, serde_json::Value)> = vec![
        ("one track, first play".into(), None, one(&liked[0].uri)),
        ("one track, warm".into(), None, one(&liked[1].uri)),
    ];
    if let Some(ctx) = liked_context(&session) {
        cases.push(("Liked Songs row".into(), None, ctx_load(&ctx, &liked[2].uri)));
        cases.push(("one track, leaving the list".into(), None, one(&liked[0].uri)));
        cases.push(("Liked Songs row, list prepared".into(), Some(ctx.clone()), ctx_load(&ctx, &liked[3].uri)));
    }
    if let Some((p, row)) = &playlist {
        cases.push((format!("playlist row ({} tracks)", p.tracks), None, ctx_load(&p.uri, row)));
        cases.push(("one track, leaving the list".into(), None, one(&liked[1].uri)));
        cases.push((format!("playlist row ({} tracks), list prepared", p.tracks), Some(p.uri.clone()), ctx_load(&p.uri, row)));
    }

    let log_path = lightify_core::config::data_dir().join("lightify-shell-audio.log");
    let mut rows = Vec::new();
    for (label, prepare, cmd) in &cases {
        let log_from = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
        if let Some(ctx) = prepare {
            // What opening the list does: fetch it while the user looks at it.
            prepare_context(ctx);
            tokio::time::sleep(Duration::from_millis(2500)).await;
        }
        while rx.try_recv().is_ok() {}
        let t = std::time::Instant::now();
        engine::send(&cmd.to_string());
        let (mut track_at, mut playing_at) = (None, None);
        while t.elapsed() < Duration::from_secs(20) && playing_at.is_none() {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(engine::Event::Track { .. }) if track_at.is_none() => track_at = Some(t.elapsed()),
                Ok(engine::Event::Playing { playing: true }) => playing_at = Some(t.elapsed()),
                _ => {}
            }
        }
        let ms = |d: Option<Duration>| d.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "never".into());
        println!("  {label}: track event {}, playing {}", ms(track_at), ms(playing_at));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let log = std::fs::read(&log_path).unwrap_or_default();
        let new = String::from_utf8_lossy(log.get(log_from as usize..).unwrap_or_default()).into_owned();
        for line in new.lines() {
            println!("      {line}");
        }
        rows.push((label.clone(), playing_at));
    }
    // The Web API's play, for comparison: the route several clicks took before.
    while rx.try_recv().is_ok() {}
    let t = std::time::Instant::now();
    let web = session.play_uris(std::slice::from_ref(&liked[2].uri)).await;
    let mut playing_at = None;
    while web.is_ok() && t.elapsed() < Duration::from_secs(20) && playing_at.is_none() {
        if let Ok(engine::Event::Playing { playing: true }) = rx.recv_timeout(Duration::from_millis(5)) {
            playing_at = Some(t.elapsed());
        }
    }
    match (web, playing_at) {
        (Err(e), _) => println!("  one track via the Web API: failed ({e})"),
        (Ok(()), Some(d)) => println!("  one track via the Web API: playing {} ms", d.as_millis()),
        (Ok(()), None) => println!("  one track via the Web API: never started"),
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    engine::send(r#"{"cmd":"pause"}"#);
    println!("{:-<72}", "");
    let all = rows.iter().all(|(_, p)| p.is_some());
    println!("{}", if all { "DONE \u{2014} every case started" } else { "FAIL \u{2014} a case never started" });
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
    let mut pager = TrackPager::start(PagerSource::Liked, &tr!("Liked Songs"));
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
    app.set_drill_title(tr!("Liked Songs").into());
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
    const ART: (f32, f32) = (634.0, 207.0);
    // Centre of the play/pause button with the drawer open, which narrows the pane
    // (288..680); measured from a render at this size.
    const PLAY_DRAWER: (f32, f32) = (484.0, 457.0);

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
    // Panels jump straight to their end state (no frames advance between clicks here).
    app.global::<Motion>().set_slide(0);

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
    click(PLAY_DRAWER);
    check(*plays.borrow() == 1, "the play button still toggles playback", &mut fails);
    check(
        *closes.borrow() == 2,
        "...and clicking it does NOT dismiss the sidebar",
        &mut fails,
    );

    // 6. Double-click with nothing left to dismiss toggles the expanded cover.
    // (`pane-double-click` ignores a double-click within 700ms of a dismiss, timed by
    // `animation-tick()`, which only moves when timers update.)
    std::thread::sleep(std::time::Duration::from_millis(800));
    slint::platform::update_timers_and_animations();
    app.set_sidebar_mode(0);
    app.set_left_collapsed(true);
    app.set_art_full(false);
    click(BG);
    click(BG);
    check(app.get_art_full(), "a double-click on the bare pane expands the cover", &mut fails);
    click(BG);
    click(BG);
    check(!app.get_art_full(), "...and another one shrinks it again", &mut fails);

    // 7. Sidebars first: a double-click whose clicks closed a panel leaves the cover.
    app.set_art_full(true);
    app.set_left_collapsed(false);
    click(BG);
    click(BG);
    check(app.get_left_collapsed(), "a double-click with the left panel open closes it", &mut fails);
    check(app.get_art_full(), "...and does NOT shrink the expanded cover", &mut fails);

    println!("{:-<72}", "");
    if fails.is_empty() {
        println!("PASS \u{2014} right-before-left dismiss, sidebars-first double-click, and controls keep their clicks");
    } else {
        println!("FAIL \u{2014} {} check(s): {}", fails.len(), fails.join("; "));
    }
}

/// Hold-to-select on a real track list, driven with real pointer events and the real
/// hold timer (`RowTouch`): a quick click plays; a press held past the hold time
/// starts a drag on that row, the drag reports the row under the pointer, release
/// ends it without playing; with a selection a quick click clears it instead of
/// playing; a second hold starts a second drag (the host adds it — `drag_selection`'s
/// unit tests cover the arithmetic).
fn selftest_hold() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
    use slint::LogicalPosition;
    use std::cell::RefCell;

    const W: u32 = 980;
    const H: u32 = 660;
    // First drilled track row's centre and its pitch (TrackRow is 46px), as in
    // `selftest_menus`. Rows 0..8 are on screen at this size (the footer starts at 618).
    const TRACK0_Y: f32 = 196.0;
    const X: f32 = 120.0;

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
    app.global::<Motion>().set_slide(0);
    let rows: Vec<Trk> = (0..30)
        .map(|i| Trk {
            uri: format!("spotify:track:t{i}").into(),
            name: format!("Track {i}").into(),
            artist: "Someone".into(),
            playable: true,
            active: false,
            playing: false,
            selected: false,
        })
        .collect();
    app.set_tracks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    app.set_drilled(true);
    app.set_drill_title("Test".into());

    let plays = Rc::new(RefCell::new(Vec::<i32>::new()));
    let begins = Rc::new(RefCell::new(Vec::<(i32, i32)>::new()));
    let moves = Rc::new(RefCell::new(Vec::<(i32, f32)>::new()));
    let ends = Rc::new(RefCell::new(0usize));
    let clears = Rc::new(RefCell::new(0usize));
    {
        let p = plays.clone();
        app.on_play_track(move |i| p.borrow_mut().push(i));
    }
    {
        let b = begins.clone();
        app.on_sel_drag_begin(move |s, r| b.borrow_mut().push((s, r)));
    }
    {
        let m = moves.clone();
        app.on_sel_drag_move(move |s, y| m.borrow_mut().push((s, y)));
    }
    {
        let e = ends.clone();
        app.on_sel_drag_end(move || *e.borrow_mut() += 1);
    }
    {
        let c = clears.clone();
        app.on_clear_selection(move || *c.borrow_mut() += 1);
    }

    window.set_size(slint::PhysicalSize::new(W, H));
    // Timers (the hold) only tick on a shown window; no OS window exists here.
    if let Err(e) = app.show() {
        println!("WINDOW ERROR: {e}");
        return;
    }
    let mut buffer = vec![Rgb565Pixel(0); (W * H) as usize];
    let win = window.window();
    let mut frame = || {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buffer, W as usize);
        });
    };
    let at = |row: f32| LogicalPosition::new(X, TRACK0_Y + 46.0 * row);
    let press = |p: LogicalPosition| {
        win.dispatch_event(WindowEvent::PointerMoved { position: p });
        win.dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
    };
    let release = |p: LogicalPosition| {
        win.dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
    };
    // Hold long enough for the 300ms timer, letting it tick.
    let hold = |frame: &mut dyn FnMut()| {
        for _ in 0..12 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            frame();
        }
    };

    let mut fails: Vec<String> = Vec::new();
    let check = |cond: bool, what: &str, fails: &mut Vec<String>| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };

    frame();
    // 1. A quick click plays the row.
    press(at(2.0));
    frame();
    release(at(2.0));
    frame();
    check(*plays.borrow() == vec![2], "a quick click plays the row", &mut fails);
    check(begins.borrow().is_empty(), "...and starts no drag", &mut fails);
    // Let the double-click window pass so the next press is a fresh click.
    std::thread::sleep(std::time::Duration::from_millis(600));
    frame();

    // 2. Hold on row 3, drag to row 6, release.
    press(at(3.0));
    frame();
    hold(&mut frame);
    check(*begins.borrow() == vec![(SEL_TRACKS, 3)], "holding starts a drag on that row", &mut fails);
    // The edge auto-scroll ticks from the hold on; before the pointer moves it must
    // not see a stale position (it used to read the list's top: rows 0..=3 selected).
    check(
        moves.borrow().iter().all(|&(s, y)| drag_row(s, y, 30) == 3),
        "...and selects nothing else before the pointer moves",
        &mut fails,
    );
    win.dispatch_event(WindowEvent::PointerMoved { position: at(4.5) });
    frame();
    win.dispatch_event(WindowEvent::PointerMoved { position: at(6.0) });
    frame();
    let last = moves.borrow().last().copied();
    let landed = last.map(|(s, y)| (s, drag_row(s, y, 30)));
    check(landed == Some((SEL_TRACKS, 6)), "dragging reports the row under the pointer", &mut fails);
    release(at(6.0));
    frame();
    check(*ends.borrow() == 1, "releasing ends the drag", &mut fails);
    check(*plays.borrow() == vec![2], "...and does not play anything", &mut fails);
    std::thread::sleep(std::time::Duration::from_millis(600));
    frame();

    // 3. With a selection, a quick click clears it instead of playing.
    app.set_sel_count(4);
    press(at(8.0));
    frame();
    release(at(8.0));
    frame();
    check(*clears.borrow() == 1, "with a selection, a quick click clears it", &mut fails);
    check(*plays.borrow() == vec![2], "...instead of playing", &mut fails);
    std::thread::sleep(std::time::Duration::from_millis(600));
    frame();

    // 4. A second hold (selection still there) starts another drag — added on top.
    press(at(7.0));
    frame();
    hold(&mut frame);
    release(at(7.0));
    frame();
    check(begins.borrow().last() == Some(&(SEL_TRACKS, 7)), "a second hold starts a second group", &mut fails);
    check(*clears.borrow() == 1, "...without clearing the first", &mut fails);

    println!("{:-<72}", "");
    if fails.is_empty() {
        println!("PASS \u{2014} quick click plays/clears, hold starts an additive drag-select");
    } else {
        println!("FAIL \u{2014} {} check(s): {}", fails.len(), fails.join("; "));
    }
}

/// `--selftest-local-hold`: the Local track list's touch handling, headless — a quick
/// click plays; holding 300 ms (not sooner) starts a drag-select that reports each row
/// crossed and ends on release; with a selection a quick click clears it. Then the
/// tree's collapse: the list takes its room, a carry reopens it for the drop and
/// closes it after, and reopening gives the tree its room back. Last, a typed length
/// in the filter panel commits on Enter.
/// `--selftest-lava`: the lava's inputs, headless. A right-click on the blank Spotify
/// pane and on Local's now-playing view opens the backdrop menu (kind 11); over the
/// screensaver a mouse move does nothing, while a key or any mouse button ends it.
fn selftest_lava() {
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
    let opened = Rc::new(RefCell::new(Vec::<(i32, i32)>::new()));
    {
        let o = opened.clone();
        app.on_open_context(move |k, i| o.borrow_mut().push((k, i)));
    }
    let ends = Rc::new(RefCell::new(0usize));
    {
        let e = ends.clone();
        app.on_lava_saver_end(move || *e.borrow_mut() += 1);
    }
    install_filter_helpers(&app);
    window.set_size(slint::PhysicalSize::new(W, H));
    if let Err(e) = app.show() {
        println!("WINDOW ERROR: {e}");
        return;
    }
    let mut buffer = vec![Rgb565Pixel(0); (W * H) as usize];
    let win = window.window();
    let mut frame = || {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buffer, W as usize);
        });
    };
    frame();
    let mut ok = true;
    let mut check = |what: &str, pass: bool, detail: String| {
        println!("{} {what}: {detail}", if pass { "ok  " } else { "FAIL" });
        ok &= pass;
    };
    let right = |p: LogicalPosition| {
        win.dispatch_event(WindowEvent::PointerMoved { position: p });
        win.dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Right });
        win.dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Right });
    };
    // Control: typing reaches the window's keys before anything happens (or the check
    // after the screensaver proves nothing). A real window is active; this one says so.
    win.dispatch_event(WindowEvent::WindowActiveChanged(true));
    frame();
    win.dispatch_event(WindowEvent::KeyPressed { text: "z".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "z".into() });
    frame();
    check("keys work before", app.get_list_filter() == "z", format!("filter {:?}", app.get_list_filter()));
    app.set_list_filter("".into());
    // Spotify: blank pane to the right of the cover.
    let blank = LogicalPosition::new(930.0, 120.0);
    right(blank);
    frame();
    check("Spotify pane right-click", *opened.borrow() == vec![(11, 0)], format!("{:?}", opened.borrow()));
    check(
        "menu at the pointer",
        (app.get_ctx_x() - 930.0).abs() < 1.0 && (app.get_ctx_y() - 120.0).abs() < 1.0,
        format!("({}, {})", app.get_ctx_x(), app.get_ctx_y()),
    );
    opened.borrow_mut().clear();
    // Local's now-playing view.
    app.set_local_mode(true);
    app.set_local_now_open(true);
    frame();
    std::thread::sleep(Duration::from_millis(350));
    frame();
    right(blank);
    frame();
    check("Local now-playing right-click", *opened.borrow() == vec![(11, 0)], format!("{:?}", opened.borrow()));

    // The screensaver's way out.
    app.set_lava_saver(true);
    frame();
    let mid = LogicalPosition::new(400.0, 300.0);
    win.dispatch_event(WindowEvent::PointerMoved { position: mid });
    win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(420.0, 310.0) });
    frame();
    check("mouse move keeps it", *ends.borrow() == 0, format!("{} ends", ends.borrow()));
    win.dispatch_event(WindowEvent::KeyPressed { text: "a".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "a".into() });
    frame();
    check("a key ends it", *ends.borrow() == 1, format!("{} ends", ends.borrow()));
    for (n, b) in [(2, PointerEventButton::Left), (3, PointerEventButton::Right), (4, PointerEventButton::Middle)] {
        win.dispatch_event(WindowEvent::PointerPressed { position: mid, button: b });
        win.dispatch_event(WindowEvent::PointerReleased { position: mid, button: b });
        frame();
        check(&format!("{b:?} button ends it"), *ends.borrow() == n, format!("{} ends", ends.borrow()));
    }
    check("no menu opened under it", opened.borrow().len() == 1, format!("{:?}", opened.borrow()));
    // Gone again: typing reaches the window's own keys (type-to-filter on the Library).
    app.set_lava_saver(false);
    app.set_local_mode(false);
    frame();
    win.dispatch_event(WindowEvent::KeyPressed { text: "q".into() });
    win.dispatch_event(WindowEvent::KeyReleased { text: "q".into() });
    frame();
    check("keys work after it", app.get_list_filter() == "q", format!("filter {:?}", app.get_list_filter()));
    println!("{}", if ok { "ALL OK" } else { "FAILED" });
}

/// `--selftest-lava-fullscreen`: the real window. Starting the screensaver takes it
/// full screen with a frame up; ending it brings back the size it had. (It shows ~3 s.)
fn selftest_lava_fullscreen() {
    use std::cell::RefCell;
    type Log = Rc<RefCell<Vec<(String, (u32, u32), bool)>>>;
    let app = match MainWindow::new() {
        Ok(a) => a,
        Err(e) => {
            println!("WINDOW ERROR: {e}");
            return;
        }
    };
    local_backdrop::install(&app);
    if let Err(e) = app.show() {
        println!("WINDOW ERROR: {e}");
        return;
    }
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    fn snap(app: &MainWindow, what: &str, log: &Log) {
        let s = app.window().size();
        log.borrow_mut().push((what.to_string(), (s.width, s.height), app.window().is_fullscreen()));
    }
    // What the window-geometry store would record, captured as the live window does.
    let geom = Rc::new(RefCell::new(Vec::<String>::new()));
    fn capture(app: &MainWindow) -> String {
        use slint::winit_030::WinitWindowAccessor;
        app.window().with_winit_window(|w| window_geom::capture(w, false));
        let g = window_geom::get();
        format!("normal {:?} maximized {}", g.normal.map(|r| (r.w, r.h)), g.maximized)
    }
    // One round: start, look, end, look. `max` maximizes the window first.
    fn round(weak: slint::Weak<MainWindow>, l: Log, geom: Rc<RefCell<Vec<String>>>, max: bool, then: Box<dyn FnOnce()>) {
        let tag = if max { "max " } else { "" };
        if let Some(a) = weak.upgrade() {
            if max {
                a.window().set_maximized(true);
            }
        }
        slint::Timer::single_shot(Duration::from_millis(700), move || {
            let Some(a) = weak.upgrade() else { return };
            snap(&a, &format!("{tag}before"), &l);
            geom.borrow_mut().push(format!("{tag}before: {}", capture(&a)));
            local_backdrop::start_saver(&a);
            slint::Timer::single_shot(Duration::from_millis(1200), move || {
                let Some(a) = weak.upgrade() else { return };
                snap(&a, &format!("{tag}saver"), &l);
                geom.borrow_mut().push(format!("{tag}saver: {}", capture(&a)));
                let up = a.get_saver_bg().size().width > 0;
                l.borrow_mut().push((format!("{tag}saver frame up: {up}"), (0, 0), a.get_lava_saver()));
                a.invoke_lava_saver_end();
                slint::Timer::single_shot(Duration::from_millis(900), move || {
                    if let Some(a) = weak.upgrade() {
                        snap(&a, &format!("{tag}after"), &l);
                        l.borrow_mut().push((format!("{tag}after maximized: {}", a.window().is_maximized()), (0, 0), max));
                        geom.borrow_mut().push(format!("{tag}after: {}", capture(&a)));
                    }
                    then();
                });
            });
        });
    }
    {
        let (w1, l1, g1) = (app.as_weak(), log.clone(), geom.clone());
        let (w2, l2, g2) = (app.as_weak(), log.clone(), geom.clone());
        round(w1, l1, g1, false, Box::new(move || {
            round(w2, l2, g2, true, Box::new(|| {
                let _ = slint::quit_event_loop();
            }));
        }));
    }
    let _ = slint::run_event_loop();
    let (sw, sh) = {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        // SAFETY: plain metric queries.
        unsafe { (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32) }
    };
    let log = log.borrow();
    for (what, size, flag) in log.iter() {
        println!("{what}: {}x{} (fullscreen/saver flag {flag})", size.0, size.1);
    }
    println!("primary screen: {sw}x{sh}");
    for g in geom.borrow().iter() {
        println!("geometry {g}");
    }
    let get = |w: &str| log.iter().find(|e| e.0 == w).map(|e| (e.1, e.2));
    let sized = matches!(
        (get("before"), get("saver"), get("after")),
        (Some((b, false)), Some((s, true)), Some((a, false))) if s == (sw, sh) && a == b
    );
    let max_sized = matches!(
        (get("max before"), get("max saver"), get("max after")),
        (Some((b, false)), Some((s, true)), Some((a, false))) if s == (sw, sh) && a == b
    );
    let framed = log.iter().filter(|e| e.0.ends_with("saver frame up: true") && e.2).count() == 2;
    let remax = log.iter().any(|e| e.0 == "max after maximized: true");
    // The stored geometry is the same before, during and after each round.
    let gs = geom.borrow();
    let same = |tag: &str| {
        let v: Vec<&str> = gs.iter().filter(|g| g.starts_with(tag) && (tag != "" || !g.starts_with("max"))).map(|g| g.split_once(": ").map(|x| x.1).unwrap_or("")).collect();
        v.len() == 3 && v.iter().all(|x| *x == v[0])
    };
    let kept = same("") && same("max ");
    println!("normal round {sized}, maximized round {max_sized} (re-maximized {remax}), frames {framed}, geometry kept {kept}");
    println!("{}", if sized && max_sized && framed && remax && kept { "ALL OK" } else { "FAILED" });
}

fn selftest_local_hold() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
    use slint::LogicalPosition;
    use std::cell::RefCell;

    const W: u32 = 980;
    const H: u32 = 660;
    // The first Local row's centre (titlebar 36 + header 52 + column heads 30, rows 36px).
    const ROW0_Y: f32 = 136.0;
    const X: f32 = 500.0;

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
    let rows: Vec<LocalRow> = (0..30)
        .map(|i| LocalRow {
            num: format!("{:02}", i + 1).into(),
            title: format!("Track {i}").into(),
            artist: "Someone".into(),
            album: "Something".into(),
            time: "3:00".into(),
            playing: false,
            selected: false,
        })
        .collect();
    app.set_local_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    app.set_local_title("All music".into());
    app.set_local_mode(true);

    let plays = Rc::new(RefCell::new(Vec::<i32>::new()));
    let begins = Rc::new(RefCell::new(Vec::<i32>::new()));
    let tos = Rc::new(RefCell::new(Vec::<i32>::new()));
    let ends = Rc::new(RefCell::new(0usize));
    let clears = Rc::new(RefCell::new(0usize));
    {
        let p = plays.clone();
        app.on_local_row_activate(move |i| p.borrow_mut().push(i));
    }
    {
        let b = begins.clone();
        app.on_local_drag_begin(move |r| b.borrow_mut().push(r));
    }
    {
        let t = tos.clone();
        app.on_local_drag_to(move |r| t.borrow_mut().push(r));
    }
    {
        let e = ends.clone();
        app.on_local_drag_end(move || *e.borrow_mut() += 1);
    }
    {
        let c = clears.clone();
        app.on_local_row_clear_sel(move || *c.borrow_mut() += 1);
    }

    window.set_size(slint::PhysicalSize::new(W, H));
    if let Err(e) = app.show() {
        println!("WINDOW ERROR: {e}");
        return;
    }
    let mut buffer = vec![Rgb565Pixel(0); (W * H) as usize];
    let win = window.window();
    let mut frame = || {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buffer, W as usize);
        });
    };
    // Let the Spotify → Local cross-fade finish.
    frame();
    std::thread::sleep(std::time::Duration::from_millis(350));
    frame();
    let at = |row: f32| LogicalPosition::new(X, ROW0_Y + 36.0 * row);
    let press = |p: LogicalPosition| {
        win.dispatch_event(WindowEvent::PointerMoved { position: p });
        win.dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
    };
    let release = |p: LogicalPosition| {
        win.dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
    };
    let wait = |ms: u64, frame: &mut dyn FnMut()| {
        for _ in 0..ms / 25 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            frame();
        }
    };

    let mut fails: Vec<String> = Vec::new();
    let check = |cond: bool, what: &str, fails: &mut Vec<String>| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what.to_string());
        }
    };

    // 1. A quick click plays the row.
    press(at(2.0));
    frame();
    release(at(2.0));
    frame();
    check(*plays.borrow() == vec![2], "a quick click plays the row", &mut fails);
    check(begins.borrow().is_empty(), "...and starts no drag", &mut fails);
    wait(600, &mut frame);

    // 2. Hold on row 3: nothing at 200 ms, a drag by 400 ms; drag to row 6; release.
    press(at(3.0));
    frame();
    wait(200, &mut frame);
    check(begins.borrow().is_empty(), "no drag before the 300 ms hold", &mut fails);
    wait(200, &mut frame);
    check(*begins.borrow() == vec![3], "holding 300 ms starts a drag on that row", &mut fails);
    wait(150, &mut frame);
    check(
        tos.borrow().iter().all(|&r| r == 3),
        "...and selects nothing else before the pointer moves",
        &mut fails,
    );
    win.dispatch_event(WindowEvent::PointerMoved { position: at(4.0) });
    frame();
    win.dispatch_event(WindowEvent::PointerMoved { position: at(6.0) });
    frame();
    check(tos.borrow().last() == Some(&6), "dragging reports the row under the pointer", &mut fails);
    check(tos.borrow().contains(&4), "...and the rows crossed on the way", &mut fails);
    check(tos.borrow().iter().all(|r| (3..=6).contains(r)), "...and no row it did not cross", &mut fails);
    release(at(6.0));
    frame();
    check(*ends.borrow() == 1, "releasing ends the drag", &mut fails);
    check(*plays.borrow() == vec![2], "...and does not play anything", &mut fails);
    wait(600, &mut frame);

    // 3. With a selection, a quick click clears it instead of playing.
    app.set_local_sel_count(4);
    press(at(8.0));
    frame();
    release(at(8.0));
    frame();
    check(*clears.borrow() == 1, "with a selection, a quick click clears it", &mut fails);
    check(*plays.borrow() == vec![2], "...instead of playing", &mut fails);
    app.set_local_sel_count(0);
    wait(600, &mut frame);

    // 4. Collapse the tree (titlebar wordmark): a click where the tree was now hits the list.
    let at_x = |x: f32, row: f32| LogicalPosition::new(x, ROW0_Y + 36.0 * row);
    app.set_left_collapsed(true);
    wait(400, &mut frame);
    press(at_x(150.0, 1.0));
    frame();
    release(at_x(150.0, 1.0));
    frame();
    check(*plays.borrow() == vec![2, 1], "collapsed: the list fills the tree's room", &mut fails);
    wait(600, &mut frame);
    // A carry reopens the tree for the drop, and closes it again when it ends.
    app.set_local_carry_active(true);
    frame();
    check(!app.get_left_collapsed(), "a carry opens the collapsed tree", &mut fails);
    app.set_local_carry_active(false);
    frame();
    check(app.get_left_collapsed(), "...and closes it again after", &mut fails);
    // Reopened, the same spot is the tree again.
    app.set_left_collapsed(false);
    wait(400, &mut frame);
    press(at_x(150.0, 1.0));
    frame();
    release(at_x(150.0, 1.0));
    frame();
    check(*plays.borrow() == vec![2, 1], "reopened: that spot is the tree again", &mut fails);

    // 5. A right-click on a row (with a selection, and with the list scrolled) opens
    //    the menu at the pointer, not somewhere else in the window.
    let menus = Rc::new(RefCell::new(Vec::<(i32, i32, f32)>::new()));
    {
        let m = menus.clone();
        let w = app.as_weak();
        app.on_open_context(move |kind, index| {
            let y = w.upgrade().map(|a| a.get_ctx_y()).unwrap_or(-1.0);
            m.borrow_mut().push((kind, index, y));
        });
    }
    app.set_local_sel_count(3);
    let right = |p: LogicalPosition| {
        win.dispatch_event(WindowEvent::PointerMoved { position: p });
        win.dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Right });
        win.dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Right });
    };
    right(at(4.0));
    frame();
    // Scroll the list down five rows, then right-click the row under the same spot.
    win.dispatch_event(WindowEvent::PointerScrolled { position: at(4.0), delta_x: 0.0, delta_y: -180.0 });
    wait(100, &mut frame);
    right(at(4.0));
    frame();
    let got = menus.borrow().clone();
    println!("  menus opened: {got:?} (pointer y {})", ROW0_Y + 36.0 * 4.0);
    check(
        got.len() == 2 && got.iter().all(|&(k, _, y)| k == 8 && (y - (ROW0_Y + 36.0 * 4.0)).abs() < 20.0),
        "a right-click opens the row menu at the pointer (also scrolled)",
        &mut fails,
    );
    app.set_local_sel_count(0);
    app.set_ctx_open(false);
    wait(100, &mut frame);

    // 5b. Starting a track (the host re-pushes the rows to mark it playing) must not
    //     move the scrolled list: the same spot clicked again is the same row.
    let hits = Rc::new(RefCell::new(Vec::<i32>::new()));
    {
        let h = hits.clone();
        let w = app.as_weak();
        app.on_local_row_activate(move |i| {
            h.borrow_mut().push(i);
            if let Some(a) = w.upgrade() {
                // What playing does (`local_ui::push_rows`): the rows again, this one
                // marked playing.
                use slint::Model;
                let m = a.get_local_rows();
                let rows: Vec<LocalRow> = (0..m.row_count())
                    .filter_map(|r| m.row_data(r))
                    .enumerate()
                    .map(|(r, mut row)| {
                        row.playing = r as i32 == i;
                        row
                    })
                    .collect();
                local_ui::apply_rows(&a, rows);
            }
        });
    }
    let click = |p: LogicalPosition, frame: &mut dyn FnMut()| {
        win.dispatch_event(WindowEvent::PointerMoved { position: p });
        win.dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
        frame();
        win.dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
        frame();
    };
    click(at(6.0), &mut frame);
    wait(600, &mut frame);
    click(at(6.0), &mut frame);
    wait(600, &mut frame);
    // ...and near the end of the list (scrolled almost, not quite, to the bottom).
    win.dispatch_event(WindowEvent::PointerScrolled { position: at(6.0), delta_x: 0.0, delta_y: -2000.0 });
    wait(100, &mut frame);
    win.dispatch_event(WindowEvent::PointerScrolled { position: at(6.0), delta_x: 0.0, delta_y: 50.0 });
    wait(100, &mut frame);
    click(at(6.0), &mut frame);
    wait(600, &mut frame);
    click(at(6.0), &mut frame);
    wait(600, &mut frame);
    let got = hits.borrow().clone();
    check(
        got.len() == 4 && got[0] == got[1] && got[2] == got[3],
        &format!("starting a track keeps the scrolled list still (rows clicked: {got:?})"),
        &mut fails,
    );

    // 6. The filter panel's typed length: click "Shortest", type, Enter → committed.
    let lens = Rc::new(RefCell::new(Vec::<(bool, String)>::new()));
    {
        let l = lens.clone();
        app.on_local_flt_set_len(move |longest, text| l.borrow_mut().push((longest, text.to_string())));
    }
    app.set_local_flt_mode(1);
    app.set_local_filters_open(true);
    wait(100, &mut frame);
    // The Shortest field (filter card under the search field, below Show and its hint).
    let field = LogicalPosition::new(80.0, 297.0);
    win.dispatch_event(WindowEvent::PointerMoved { position: field });
    win.dispatch_event(WindowEvent::PointerPressed { position: field, button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: field, button: PointerEventButton::Left });
    frame();
    for ch in ["9", "0", "\n"] {
        win.dispatch_event(WindowEvent::KeyPressed { text: ch.into() });
        win.dispatch_event(WindowEvent::KeyReleased { text: ch.into() });
        frame();
    }
    check(
        *lens.borrow() == vec![(false, "90".to_string())],
        &format!("filters: typing in Shortest and pressing Enter commits it once (got {:?})", lens.borrow()),
        &mut fails,
    );
    app.set_local_filters_open(false);
    wait(100, &mut frame);

    // 7. U12: a carry whose row is replaced mid-drag (the list changed under it) still
    //    ends on release — before, the row took the release with it and the carry stuck.
    let carry_ends = Rc::new(RefCell::new(0usize));
    {
        let e = carry_ends.clone();
        app.on_local_carry_end(move |_| *e.borrow_mut() += 1);
    }
    let rows_sel = |sel: bool, n: usize| -> slint::ModelRc<LocalRow> {
        slint::ModelRc::from(Rc::new(slint::VecModel::from(
            (0..n)
                .map(|i| LocalRow {
                    num: format!("{:02}", i + 1).into(),
                    title: format!("Track {i}").into(),
                    artist: "Someone".into(),
                    album: "Something".into(),
                    time: "3:00".into(),
                    playing: false,
                    selected: sel && i == 2,
                })
                .collect::<Vec<_>>(),
        )))
    };
    win.dispatch_event(WindowEvent::PointerScrolled { position: at(2.0), delta_x: 0.0, delta_y: 3000.0 });
    app.set_local_rows(rows_sel(true, 30));
    app.set_local_sel_count(1);
    wait(100, &mut frame);
    press(at(2.0));
    frame();
    win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(X + 30.0, ROW0_Y + 36.0 * 2.0 + 20.0) });
    frame();
    check(app.get_local_carry_active(), "U12: moving a pressed selected row starts a carry", &mut fails);
    // The list is replaced by a shorter one (a rescan, a filter): the carrying row
    // is gone, not just given new data.
    app.set_local_rows(rows_sel(false, 2));
    frame();
    win.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(X + 60.0, ROW0_Y + 36.0 * 5.0) });
    frame();
    release(LogicalPosition::new(X + 60.0, ROW0_Y + 36.0 * 5.0));
    frame();
    check(
        !app.get_local_carry_active() && *carry_ends.borrow() == 1,
        "U12: ...and still ends on release after its row was replaced",
        &mut fails,
    );
    app.set_local_sel_count(0);

    println!("{:-<72}", "");
    if fails.is_empty() {
        println!("PASS \u{2014} Local list: quick click plays/clears, a 300 ms hold drag-selects, the tree collapses");
    } else {
        println!("FAIL \u{2014} {} check(s): {}", fails.len(), fails.join("; "));
    }
}

/// Whether `press` completes a double-click begun by `prev` (both `(time, x, y)`).
fn is_double_press(
    prev: Option<(std::time::Instant, f32, f32)>,
    press: (std::time::Instant, f32, f32),
    limit: Duration,
    slop: f32,
) -> bool {
    prev.is_some_and(|(t, px, py)| {
        press.0.saturating_duration_since(t) <= limit && (press.1 - px).abs() <= slop && (press.2 - py).abs() <= slop
    })
}

/// The system's double-click time and the distance (logical px) the second click may
/// stray from the first.
fn double_click_limits(scale: f32) -> (Duration, f32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK};
        // SAFETY: plain system queries with no arguments to validate.
        let (ms, cx, cy) = unsafe { (GetDoubleClickTime(), GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK)) };
        let ms = if ms == 0 { 500 } else { ms };
        // The metric is the whole rectangle around the first click.
        let slop = (cx.max(cy).max(4) as f32 / 2.0) / scale.max(0.5);
        (Duration::from_millis(ms as u64), slop)
    }
    #[cfg(not(windows))]
    {
        let _ = scale;
        (Duration::from_millis(500), 2.0)
    }
}

/// `--selftest-language [code]`: a live language switch, headless, for each bundled
/// language (or just `code`). It switches the way Settings › language does (without
/// saving the choice) and checks that a Slint string, a Rust status message, a library
/// row and a row menu built in Rust, and a tray item all changed, that the open menu
/// and the status line were cleared and the worker was asked to re-say its models —
/// then switches back to English and checks every one of them is English again.
fn selftest_language(only: Option<&str>) {
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
    let app = match MainWindow::new() {
        Ok(a) => a,
        Err(e) => {
            println!("WINDOW ERROR: {e}");
            return;
        }
    };
    let codes: Vec<&str> = LANGS.iter().map(|l| l.code).filter(|c| only.is_none_or(|o| o.eq_ignore_ascii_case(c))).collect();
    if codes.is_empty() {
        println!("SKIP: no bundled language{}", only.map(|o| format!(" called {o}")).unwrap_or_default());
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    // What each check reads, in whatever language is active.
    let sample = |app: &MainWindow| -> Vec<(&'static str, String)> {
        #[cfg(windows)]
        let tray = tray::labels().join(" / ");
        #[cfg(not(windows))]
        let tray = String::from("(no tray)");
        vec![
            ("Slint string (Beatport hint)", app.get_bp_status().to_string()),
            ("Slint string (engine line)", app.get_engine_status().to_string()),
            ("Rust status message", tr!("That list just changed \u{2014} try again")),
            ("Rust library row", build_rows(&[], &[], "", false)[0].name.to_string()),
            ("Rust menu label", liked_row_menu().last().map(|i| i.label.clone()).unwrap_or_default()),
            ("Settings › language row", app.get_settings_languages().row_data(0).map(|r| r.name.to_string()).unwrap_or_default()),
            ("tray menu", tray),
        ]
    };
    let mut fails: Vec<String> = Vec::new();
    let mut check = |cond: bool, what: String| {
        println!("  {} {what}", if cond { "ok  " } else { "FAIL" });
        if !cond {
            fails.push(what);
        }
    };
    apply_switch(&app, lightify_i18n::ENGLISH, &tx);
    let english = sample(&app);
    while rx.try_recv().is_ok() {}
    for code in codes {
        println!("{code} ({}):", lightify_i18n::native_name(code));
        app.set_ctx_open(true);
        app.set_status_text("a notice".into());
        apply_switch(&app, code, &tx);
        check(lightify_i18n::current() == code, format!("{code}: the Rust half is now {code} (it is {})", lightify_i18n::current()));
        for ((what, en), (_, now)) in english.iter().zip(sample(&app)) {
            check(*en != now, format!("{code}: {what} changed (\u{201c}{en}\u{201d} \u{2192} \u{201c}{now}\u{201d}){}",
                if *en == now { " — untranslated in this catalog?" } else { "" }));
        }
        check(!app.get_ctx_open(), format!("{code}: the open menu was closed"));
        check(app.get_status_text().is_empty(), format!("{code}: the status line was cleared"));
        check(matches!(rx.try_recv(), Ok(Cmd::Relabel)), format!("{code}: the worker was asked to re-say its models"));
        apply_switch(&app, lightify_i18n::ENGLISH, &tx);
        while rx.try_recv().is_ok() {}
        let back = sample(&app);
        for ((what, en), (_, now)) in english.iter().zip(back) {
            check(*en == now, format!("{code} → en: {what} is English again (\u{201c}{now}\u{201d})"));
        }
    }
    if fails.is_empty() {
        println!("PASS: every language switches live and back");
    } else {
        println!("FAIL ({}): {}", fails.len(), fails.join(" | "));
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
    // The app's row wiring, with the drill done at once: the drilled view replaces the
    // library list straight away, so the row a double-click started on is gone before
    // its second press — the app's worst case (U2).
    {
        let weak = app.as_weak();
        let played = played.clone();
        wire_open_row(
            &app,
            move |_| {
                if let Some(a) = weak.upgrade() {
                    a.set_drilled(true);
                }
            },
            move |j| played.borrow_mut().push(j as i32),
        );
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
    // starts under the tab control + sort toolbar, ListRow is 52px, TrackRow is 46px.
    const LIKED_Y: f32 = 154.0;
    const PLAYLIST1_Y: f32 = 206.0;
    const TRACK0_Y: f32 = 196.0;
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
    win.dispatch_event(WindowEvent::PointerPressed { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    check(
        app.get_drilled() && played.borrow().is_empty(),
        "a single click on a playlist drills in at once (and plays nothing)",
        &mut fails,
    );
    win.dispatch_event(WindowEvent::PointerPressed { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    check(
        played.borrow().as_slice() == [1],
        "...and its second press, over the drilled view, plays row 1 (exactly once)",
        &mut fails,
    );
    app.set_drilled(false);
    // A press elsewhere straight after a drill is not swallowed by the catcher.
    win.dispatch_event(WindowEvent::PointerPressed { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(PLAYLIST1_Y), button: PointerEventButton::Left });
    app.set_drilled(false);
    win.dispatch_event(WindowEvent::PointerPressed { position: at(LIKED_Y), button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(LIKED_Y), button: PointerEventButton::Left });
    check(
        app.get_drilled() && played.borrow().as_slice() == [1],
        "...while a click on another row straight after still opens that row",
        &mut fails,
    );

    // 4. right-click a drilled track row
    app.set_ctx_open(false);
    app.set_drilled(true);
    app.set_drill_title(tr!("Liked Songs").into());
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



    let row_y = |i: f32| TRACK0_Y + 46.0 * i;

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

    // 11. with rows selected, a quick plain click clears them instead of playing —
    //     that is how a selection is dropped (`RowTouch`)
    app.set_ctx_open(false);
    win.dispatch_event(WindowEvent::PointerMoved { position: at(row_y(6.0)) });
    win.dispatch_event(WindowEvent::PointerPressed { position: at(row_y(6.0)), button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position: at(row_y(6.0)), button: PointerEventButton::Left });
    check(
        activated.borrow().is_empty() && app.get_sel_count() == 0,
        "with rows selected, a quick click clears them instead of playing",
        &mut fails,
    );
    // 12. ...and with nothing selected, a plain click plays the row it hit (past the
    //     double-click window, so it is a fresh click)
    std::thread::sleep(std::time::Duration::from_millis(600));
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

    // U1: whatever removes or hides a focused field hands the keyboard back to the
    // window, so Ctrl+K and type-to-filter keep working (before, nothing was focused
    // afterwards and every key was dropped). Plus U3, U4, U6, U7, U9 and U11 (case).
    let queries: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    {
        let queries = queries.clone();
        app.on_query_changed(move |q| queries.borrow_mut().push(q.to_string()));
    }
    let lens: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    {
        let lens = lens.clone();
        app.on_local_flt_set_len(move |_, t| lens.borrow_mut().push(t.to_string()));
    }
    let ctrl: slint::SharedString = slint::platform::Key::Control.into();
    let chord = |t: &str| {
        app.window().dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
        key(t);
        app.window().dispatch_event(WindowEvent::KeyReleased { text: ctrl.clone() });
    };
    let esc = || key(&slint::SharedString::from(slint::platform::Key::Escape));
    let frame = std::cell::RefCell::new(vec![PremultipliedRgbaColor::default(); 980 * 660]);
    // Run change handlers and build conditional elements (an overlay's field), as a
    // frame of the real window would.
    let settle = || {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut frame.borrow_mut(), 980);
        });
        slint::platform::update_timers_and_animations();
    };
    let click = |x: f32, y: f32| {
        let p = slint::LogicalPosition::new(x, y);
        app.window().dispatch_event(WindowEvent::PointerMoved { position: p });
        app.window().dispatch_event(WindowEvent::PointerPressed { position: p, button: slint::platform::PointerEventButton::Left });
        app.window().dispatch_event(WindowEvent::PointerReleased { position: p, button: slint::platform::PointerEventButton::Left });
    };
    // Does a plain letter reach type-to-filter now? (Cleans up after itself.)
    let types = || {
        key("a");
        let ok = filt(&app) == "a";
        esc();
        ok
    };
    settle();
    chord("k");
    settle();
    check(app.get_palette_open(), "Ctrl+K opens the palette");
    key("z");
    check(filt(&app).is_empty(), "...whose own field takes the typing");
    app.set_palette_open(false); // what running a palette command does (`on_run_palette`)
    settle();
    check(types(), "U1: after a palette command, type-to-filter works again");
    chord("K");
    settle();
    check(app.get_palette_open(), "U11: Ctrl+K with Caps Lock or Shift (text \"K\") opens it too");
    esc();
    settle();
    check(!app.get_palette_open() && types(), "...and Esc in it hands the keyboard back");
    app.set_name_prompt_open(true);
    settle();
    key("q");
    check(filt(&app).is_empty(), "the name prompt's field takes the typing");
    esc();
    settle();
    check(!app.get_name_prompt_open() && types(), "U1: closing the name prompt hands the keyboard back");
    // The Search tab's box sits at (150, 118) at 980x660.
    app.set_tab(1);
    settle();
    click(150.0, 118.0);
    key("s");
    check(app.get_search_text() == "s", "typing in the search box reaches search-text");
    app.set_search_text("midnight".into()); // a recent search clicked (`PaletteSearch`)
    key(&slint::SharedString::from(slint::platform::Key::End));
    key("x");
    check(
        queries.borrow().last().map(String::as_str) == Some("midnightx"),
        "U4: after an edit the box still follows the host's text (a recent search)",
    );
    app.set_tab(0);
    settle();
    check(types(), "U1: leaving the Search tab with its box focused hands the keyboard back");
    app.set_tab(1);
    settle();
    click(150.0, 118.0);
    app.set_local_mode(true);
    settle();
    key("x");
    check(filt(&app).is_empty(), "U3: type-to-filter is off in Local mode");
    chord("k");
    settle();
    check(app.get_palette_open(), "U1: switching to Local with the search box focused, Ctrl+K still works");
    app.set_palette_open(false);
    settle();
    // The Local filters' "Shortest" field sits at (80, 255) at 980x660 (no library,
    // so no mode note above it), once the
    // view has faded in (280 ms).
    app.set_local_filters_open(true);
    settle();
    std::thread::sleep(Duration::from_millis(350));
    settle();
    click(80.0, 255.0);
    key("9");
    key("0");
    app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(600.0, 300.0) });
    settle();
    check(app.get_local_len_away(), "U6: a length field with the keyboard and the pointer off it arms the click-away");
    // What the window hook does on the next press (a winit event, not reachable here).
    app.invoke_hand_back_focus();
    settle();
    check(
        lens.borrow().last().map(String::as_str) == Some("90") && !app.get_local_len_away(),
        "U6: ...and that press commits what was typed",
    );
    click(80.0, 255.0);
    key("5");
    key(&slint::SharedString::from(slint::platform::Key::Return));
    settle();
    let committed = lens.borrow().len();
    chord("k");
    settle();
    check(committed == 2 && app.get_palette_open(), "U1: Enter in a length field commits and hands the keyboard back");
    // U7: the arrow keys keep the highlight on screen (rendered below for a look).
    app.set_cmdk_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        (1..=30).map(|n| CmdRow { title: format!("Command {n}").into(), hint: "".into() }).collect::<Vec<_>>(),
    ))));
    settle();
    for _ in 0..14 {
        key(&slint::SharedString::from(slint::platform::Key::DownArrow));
    }
    settle();
    check(app.get_cmdk_selected() == 14, "U7: the palette's Down arrow moves the selection");
    if let Some(path) = png {
        let rgb: Vec<u8> = frame.borrow().iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
        let out = path.replace(".png", "-palette-down14.png");
        let file = std::fs::File::create(&out).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 980, 660);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
        println!("  wrote {out}");
    }
    app.set_palette_open(false);
    app.set_local_filters_open(false);
    app.set_local_mode(false);
    settle();
    app.set_left_collapsed(true);
    settle();
    key("x");
    check(filt(&app).is_empty(), "U3: type-to-filter is off while the left panel is collapsed");
    app.set_left_collapsed(false);
    app.set_tab(0);
    settle();
    app.set_settings_open(true);
    settle();
    app.set_settings_open(false);
    settle();
    check(types(), "U1: closing Settings hands the keyboard back");
    app.set_signin_open(true);
    settle();
    chord("k");
    settle();
    check(!app.get_palette_open(), "U9: Ctrl+K doesn't open the palette over the sign-in page");
    app.set_signin_open(false);
    settle();
    check(types(), "U1: the sign-in page closing hands the keyboard back");
    app.set_mini_mode("bar".into());
    settle();
    chord("k");
    settle();
    check(!app.get_palette_open(), "U9: ...nor in the 136 px bar mini player");
    app.set_mini_mode("off".into());
    settle();
    check(types(), "U1: leaving the mini player hands the keyboard back");
    // U10: only the left button seeks or sets the volume. At 980x660 the seek bar
    // runs along y 409 (x 474-794), the volume bar along y 507 (x 569-699).
    let bars: Rc<std::cell::RefCell<Vec<&'static str>>> = Rc::default();
    {
        let bars = bars.clone();
        app.on_seek(move |_| bars.borrow_mut().push("seek"));
    }
    {
        let bars = bars.clone();
        app.on_set_volume(move |_| bars.borrow_mut().push("volume"));
    }
    settle();
    for (x, y) in [(600.0, 409.0), (630.0, 507.0)] {
        let p = slint::LogicalPosition::new(x, y);
        app.window().dispatch_event(WindowEvent::PointerMoved { position: p });
        for b in [slint::platform::PointerEventButton::Right, slint::platform::PointerEventButton::Middle] {
            app.window().dispatch_event(WindowEvent::PointerPressed { position: p, button: b });
            app.window().dispatch_event(WindowEvent::PointerReleased { position: p, button: b });
        }
    }
    check(bars.borrow().is_empty(), "U10: a right or middle click on the seek or volume bar changes nothing");
    click(600.0, 409.0);
    click(630.0, 507.0);
    check(*bars.borrow() == ["seek", "volume"], "...while a left click still seeks and sets the volume");

    for c in ["d", "e", "e", "p"] {
        key(c);
    }
    app.set_list_filter_count(2);
    // M28: a track list still paging says how much the matches come from.
    app.set_list_filter_note(tr!("{} / {} loaded", "300", "900").into());
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
    /// The track whose check last failed, and when (see `like_check_due`).
    failed: Option<(String, std::time::Instant)>,
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
            note_polled_repeat(&pb);
            // Our engine already said what it is playing; a read that still reports
            // the track before it (the Web API lags a natural track end by a second
            // or two) must not put the old title and cover back up.
            let stale = engine_now_override(&mut pb, last);
            LAST_POLL_STALE.store(stale, std::sync::atomic::Ordering::Relaxed);
            // Real playback supersedes a resume offer, and becomes the next one.
            take_resume_offer();
            save_resume(&pb);
            show_track_extras(weak, session, &pb, last_art, liked).await;
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
            // The sign-in itself is gone: the worker loop takes the user to sign in.
            if lightify_core::spotify::is_token_revoked(&e) {
                note_revoked_sign_in();
                return None;
            }
            set_status(weak, tr!("Playback error \u{2014} {}", e));
            None
        }
    }
}

/// Push `pb` to the now-playing view, then its cover (fetched and decoded only when
/// the art URL changed) and its liked state (one `me/library/contains` per track
/// change, not per poll — mirrors the host's checkLiked on trackchanged).
async fn show_track_extras(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    pb: &PlaybackState,
    last_art: &mut String,
    liked: &mut LikeState,
) {
    let art_url = pb.track.as_ref().map(|t| t.album_art.clone()).unwrap_or_default();
    // Before the push, so the media flyout picks up the new cover together with the
    // new title.
    #[cfg(windows)]
    let art_moved = media_controls::set_art_url(&art_url);
    push_now_playing(weak, pb);
    // A title that didn't change (or changed before this URL was known) fires nothing, and
    // the flyout would keep the old cover under it.
    #[cfg(windows)]
    if art_moved {
        let _ = weak.upgrade_in_event_loop(|app| app.invoke_media_changed());
    }
    if art_url != *last_art {
        *last_art = art_url.clone();
        // A failed fetch/decode clears the art rather than leaving the previous
        // track's cover (and accent) up for the whole new track.
        if art_url.is_empty() {
            clear_art(weak);
        } else {
            let decoded = match take_prefetched_art(&art_url) {
                Some(d) => Some(d),
                None => session.fetch_bytes(&art_url).await.ok().and_then(|b| decode_art(&b)),
            };
            match decoded {
                Some((rgba, w, h, glow)) => set_art(weak, rgba, w, h, glow),
                None => {
                    clear_art(weak);
                    // Not shown, so not "the cover on screen": the next read of this
                    // song fetches it again instead of leaving it blank for good.
                    last_art.clear();
                }
            }
        }
    }
    let tid = pb.track.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    if tid != liked.id && like_check_due(liked, &tid) {
        let saved = if tid.is_empty() { Ok(false) } else { session.is_track_saved(&tid).await };
        match saved {
            Ok(on) => {
                *liked = LikeState { id: tid, on, failed: None };
            }
            // Unknown, not "not liked": the heart shows empty for now and a later read
            // asks again, instead of the failure standing for the whole song.
            Err(_) => like_check_failed(liked, tid, std::time::Instant::now()),
        }
        push_liked(weak, liked.on, false);
    }
}

/// A liked-state check for `tid` failed: the heart shows empty for now. The id it
/// knew goes too — it described the song before, and keeping it made that song show an
/// empty heart (no new check) when it came round again.
fn like_check_failed(liked: &mut LikeState, tid: String, at: std::time::Instant) {
    liked.id.clear();
    liked.on = false;
    liked.failed = Some((tid, at));
}

/// How long a failed liked-state check waits before the next read tries again.
const LIKE_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Ask Spotify whether `tid` is liked now? Not again right after that same check
/// failed (every poll would repeat it).
fn like_check_due(liked: &LikeState, tid: &str) -> bool {
    match &liked.failed {
        Some((id, at)) if id == tid => at.elapsed() >= LIKE_RETRY_AFTER,
        _ => true,
    }
}

/// What our own engine last said it is playing (`engine::Event::Track`), and on which
/// Connect device. While that device is the one Spotify reports active, this is the
/// truth about the track: the engine knows the moment it changes, the Web API a
/// second or two later.
struct EngineNow {
    device_id: String,
    track: Track,
}

static ENGINE_NOW: std::sync::Mutex<Option<EngineNow>> = std::sync::Mutex::new(None);
/// Whether the last `me/player` read was behind the engine (see `engine_now_override`).
/// The post-load catch-up keeps re-reading only while this holds.
static LAST_POLL_STALE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn set_engine_now(device_id: &str, track: Option<Track>) {
    if let Ok(mut g) = ENGINE_NOW.lock() {
        *g = track.map(|track| EngineNow { device_id: device_id.to_string(), track });
    }
}

/// A read of our own device that names a different track than the engine is playing
/// is behind: keep the engine's track (and the position the UI already has for it).
/// Returns whether it was behind.
fn engine_now_override(pb: &mut PlaybackState, last: &Option<PlaybackState>) -> bool {
    let Ok(g) = ENGINE_NOW.lock() else { return false };
    let Some(now) = g.as_ref() else { return false };
    if now.device_id.is_empty() || pb.device_id != now.device_id {
        return false;
    }
    if pb.track.as_ref().is_some_and(|t| t.id == now.track.id) {
        return false;
    }
    pb.duration_ms = now.track.duration_ms;
    pb.progress_ms = last
        .as_ref()
        .filter(|l| l.track.as_ref().is_some_and(|t| t.id == now.track.id))
        .map(|l| l.progress_ms)
        .unwrap_or(0);
    pb.track = Some(now.track.clone());
    true
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
fn spawn_media_key_listener(weak: slint::Weak<MainWindow>, tx: tokio::sync::mpsc::UnboundedSender<Cmd>) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_NOREPEAT};
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
            // MOD_NOREPEAT: a held key is one press (holding Next skipped several songs).
            if RegisterHotKey(std::ptr::null_mut(), id, MOD_NOREPEAT, vk) != 0 {
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
                // There is no separate stop; pausing is the closest honest thing — and
                // only a pause: on paused music a toggle started it. The player on
                // screen says whether anything plays (Spotify's, or Local's).
                4 => {
                    let tx = tx.clone();
                    let _ = weak.upgrade_in_event_loop(move |app| {
                        if app.get_mini_playing() {
                            let _ = tx.send(Cmd::TogglePlay);
                        }
                    });
                    continue;
                }
                _ => continue,
            };
            if tx.send(cmd).is_err() {
                break; // the worker is gone; so is the app
            }
        }
    });
}

#[cfg(not(windows))]
fn spawn_media_key_listener(_weak: slint::Weak<MainWindow>, _tx: tokio::sync::mpsc::UnboundedSender<Cmd>) {}

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
    // Any new play ends a running chart's background matching too — it appends to
    // a list that is about to be replaced (see `bp_match_background`).
    bp_active_seq.store(0, std::sync::atomic::Ordering::SeqCst);
    let was_station = *station_active;
    // NOT `bp_autoplay.is_some()`: the poll tick already drops `bp_autoplay` back to
    // `None` on its own the moment the playing track leaves the chart's sequence
    // (`bp_remaining_ahead` returning `None`, above) — well before the user takes
    // whatever NEW action calls this function. `bp_queued` deliberately outlives that,
    // so the leftover check below still runs even when `bp_autoplay` is already gone.
    let had_bp_leftovers = !bp_queued.is_empty();
    // A station's leftovers outlive `station_active` the same way: a check that
    // couldn't read the queue (below) keeps them for the next call — which then
    // returned here, so they stayed queued for good.
    let had_station_leftovers = !station_queued.is_empty();
    if !was_station && !had_bp_leftovers && !had_station_leftovers {
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
    set_status(weak, tr!("Cleared the queued-ahead tracks"));
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
    /// Failed tries of the row at `next_index` (see `bp_retry_row`).
    strikes: u32,
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

/// `engine_owns_playback` for code without `last` at hand (background tasks): our
/// engine has announced a track and no other device has taken over since
/// (`ENGINE_NOW` is cleared on `Event::Inactive` and `Event::Exited`).
fn engine_owns_now() -> bool {
    engine::running() && ENGINE_NOW.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Add one track to the queue. While our engine is the player this is a local
/// command (the vendored `Spirc::add_to_queue`, the same path a remote "add to queue"
/// takes), so it costs no Web-API request; otherwise `POST me/player/queue`.
///
/// Called once per track for a multi-track add, in order: each lands behind the
/// one queued before it, on the engine exactly as on the Web API.
async fn queue_one(session: &mut Session, uri: &str, engine_owned: bool) -> Result<(), String> {
    if engine_owned && uri.starts_with("spotify:track:") {
        engine::send(&serde_json::json!({ "cmd": "addtoqueue", "uris": [uri] }).to_string());
        note_queued_by_hand(uri);
        return Ok(());
    }
    session.add_to_queue(uri).await?;
    note_queued_by_hand(uri);
    Ok(())
}

/// Uris put in Spotify's user queue (by hand, a chart, a restore) that may still be
/// there. Spotify lists them ahead of the context's own next tracks, and a QUEUE mirror
/// rebuilt from its read (`Cmd::SidebarFetched`) counts them from this — the mirror that
/// knew the count is often gone by then — so a click on one of them jumps by uri list
/// instead of looking for it in the playlist, where it isn't (track 1 played).
static QUEUED_BY_HAND: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
const QUEUED_BY_HAND_MAX: usize = 200;

fn note_queued_by_hand(uri: &str) {
    if let Ok(mut g) = QUEUED_BY_HAND.lock() {
        if g.len() >= QUEUED_BY_HAND_MAX {
            g.remove(0);
        }
        g.push(uri.to_string());
    }
}

/// How many of Spotify's `queue` (in its order) lead with tracks queued by hand. Uris
/// no longer in the queue are forgotten (`forget_unqueued`).
fn queued_by_hand_head(queue: &[Track]) -> usize {
    let Ok(mut g) = QUEUED_BY_HAND.lock() else { return 0 };
    forget_unqueued(&mut g, queue);
    queued_head(&g, queue)
}

/// Drop the hand-queued uris that have played or were cleared. Spotify's read is a
/// window (~20 rows), and with more queued by hand than that it is hand-queued to its
/// end: the rest are still waiting behind it, unlisted. Those (queued after the last
/// one listed; they are noted in queue order) are kept, so they count once the window
/// reaches them. Forgetting them made a click on one jump into the playlist instead.
fn forget_unqueued(hand: &mut Vec<String>, queue: &[Track]) {
    let listed = |u: &str| queue.iter().any(|t| t.uri == u);
    let window_full = !queue.is_empty() && queued_head(hand, queue) == queue.len();
    let last_listed = hand.iter().rposition(|u| listed(u));
    let mut i = 0;
    hand.retain(|u| {
        let keep = listed(u) || (window_full && last_listed.is_some_and(|l| i > l));
        i += 1;
        keep
    });
}

/// The leading run of `queue` made of `hand` entries, each counted once.
fn queued_head(hand: &[String], queue: &[Track]) -> usize {
    let mut left: Vec<&str> = hand.iter().map(String::as_str).collect();
    queue
        .iter()
        .take_while(|t| match left.iter().position(|u| *u == t.uri) {
            Some(i) => {
                left.swap_remove(i);
                true
            }
            None => false,
        })
        .count()
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

fn stale_track_note() -> String {
    tr!("Waiting for Spotify to confirm the new track \u{2014} try again in a moment.")
}

/// Is another device the active one? Told apart by **id**: every Lightify (a second PC,
/// the phone app) shares our "Lightify" name, and comparing names made two PCs trade
/// playback back and forth. An unknown id (`""`) counts any active device.
fn another_device_active(devices: &[Device], our_id: &str) -> bool {
    devices.iter().any(|d| d.is_active && (our_id.is_empty() || d.id != our_id))
}

/// The library is read again this often while it hasn't loaded once this run.
const LIBRARY_RETRY_EVERY: Duration = Duration::from_secs(60);

/// A Spotify `Playing(true)` this soon after a Local song started is the engine catching
/// up (Local just paused it), not a new Spotify play.
const LOCAL_START_GRACE: Duration = Duration::from_secs(2);

/// Should a Spotify `Playing(true)` stop Local playback? Not right after a Local start.
fn spotify_play_stops_local(local_started_at: Option<std::time::Instant>) -> bool {
    local_started_at.is_none_or(|t| t.elapsed() >= LOCAL_START_GRACE)
}

/// Is the drilled list shown in its source order (no re-sort, no filter)? Only then are
/// the rows below a clicked one what a context play continues with.
fn view_is_source_order(view: &[usize]) -> bool {
    view.iter().enumerate().all(|(n, &k)| n == k)
}

/// Q2: should the engine that just came up take playback over? Only when no other
/// device is playing: launching the desktop must not pull music off the phone or another
/// PC. Not knowing (a failed read) is not a yes.
fn launch_takes_playback(ours: &Device, now: &Result<Option<PlaybackState>, String>) -> bool {
    if ours.is_active {
        return false; // already ours (a redundant transfer answers 500)
    }
    match now {
        Ok(Some(pb)) => !(pb.is_playing && pb.device_id != ours.id),
        Ok(None) => true,
        Err(_) => false,
    }
}

/// One downloader sign-in status poller at a time (see `Cmd::DownloaderLogin`).
struct DownloaderLoginPoller;

static DL_LOGIN_POLLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl DownloaderLoginPoller {
    fn claim() -> Option<Self> {
        (!DL_LOGIN_POLLING.swap(true, std::sync::atomic::Ordering::SeqCst)).then_some(Self)
    }
}

impl Drop for DownloaderLoginPoller {
    fn drop(&mut self) {
        DL_LOGIN_POLLING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Is the active device the downloader's own Connect device (the bridge advertises
/// itself as "OnTheSpot" for its sign-in)?
fn downloader_active(devices: &[Device]) -> bool {
    devices.iter().any(|d| d.is_active && d.name.eq_ignore_ascii_case("OnTheSpot"))
}

/// How long a crashed engine's "carry on where it was" stays good. A restart is up in
/// seconds; a `Ready` much later (Settings → Restart, a reclaim) must not start the
/// old track again.
const RESUME_AFTER_RESTART_FOR: Duration = Duration::from_secs(90);

/// Should the engine that just came up pick up what the crashed one was playing?
fn restart_resume_due(age: Duration) -> bool {
    age < RESUME_AFTER_RESTART_FOR
}

/// How long after a downloader sign-in starts an engine knocked off by it is taken
/// back: the bridge's own login wait gives up after 150 s, the status poll at 170 s.
const DL_LOGIN_RECLAIM_WINDOW: Duration = Duration::from_secs(180);

/// Restart budget for the playback engine. A lost audio device is worth recovering
/// from automatically, but an engine that cannot start at all (no output, revoked
/// credential) must not spin forever hammering Spotify and the log.
struct EngineWatchdog {
    restarts: u32,
    window_start: Option<std::time::Instant>,
    /// When the current engine registered its device (`note_ready`).
    up_since: Option<std::time::Instant>,
}

impl EngineWatchdog {
    const MAX_RESTARTS: u32 = 3;
    const WINDOW: Duration = Duration::from_secs(300);
    /// An engine counts as healthy (the old crash run forgotten) only after staying up
    /// this long. Counting every `Ready` as healthy refilled the budget each time, so an
    /// output that dies right after opening restarted — and resumed — forever.
    const HEALTHY_AFTER: Duration = Duration::from_secs(60);

    fn new() -> Self {
        Self { restarts: 0, window_start: None, up_since: None }
    }

    /// An engine registered its device at `now`.
    fn note_ready(&mut self, now: std::time::Instant) {
        self.up_since = Some(now);
    }

    /// Consumes one attempt. The budget refills once `WINDOW` has elapsed since the
    /// first restart in it, so an engine that dies once an hour is always recovered,
    /// and once an engine has stayed up `HEALTHY_AFTER` (see `note_ready`).
    fn should_restart(&mut self, now: std::time::Instant) -> bool {
        if let Some(up) = self.up_since.take() {
            if now.saturating_duration_since(up) >= Self::HEALTHY_AFTER {
                self.note_healthy();
            }
        }
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
    start_engine_after(weak, session, tx, Duration::ZERO);
}

/// `start_engine`, after `delay`. Runs beside the command loop: stopping the engine
/// it replaces waits up to `audio_engine`'s `STOP_WAIT_MS` (3 s), and a restart's
/// pause used to be slept on the loop too — Play/Pause/Next all waited behind both.
fn start_engine_after(
    weak: &slint::Weak<MainWindow>,
    session: &Session,
    tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
    delay: Duration,
) {
    let Some(bin) = engine::find_binary() else {
        // Only a developer build can lack the engine: English, not for translators.
        let msg = "Playback engine not found \u{2014} build it with `cargo build --release` in lightify-audio".to_string();
        push_engine_status(weak, || tr!("Not found"), false);
        set_status(weak, msg);
        return;
    };
    let (weak, tx, token) = (weak.clone(), tx.clone(), session.access_token().to_string());
    tokio::task::spawn_blocking(move || {
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        // Never pass a device name we haven't just confirmed exists (see resolve_output)
        // as one to open at once — but a pinned output that is missing right now (BT
        // headphones off at launch) stays pinned: it was erased from the config here,
        // for good. The output thread plays on the system default meanwhile and moves
        // back to the pin when it shows up again (`audio_output`).
        let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
        let (output, stale) = match engine::resolve_output(&configured) {
            Ok(o) => (o, None),
            Err(missing) => (Some(missing.clone()), Some(missing)),
        };
        let events = tx.clone();
        match engine::start(&bin, &token, output.as_deref(), move |ev| {
            let _ = events.send(Cmd::Engine(ev));
        }) {
            Ok(()) => {
                let cached = engine::has_cached_login();
                let say = move || {
                    if let Some(missing) = &stale {
                        tr!("Audio output \u{201c}{}\u{201d} isn\u{2019}t available \u{2014} playing on the system default", missing)
                    } else if cached {
                        tr!("Starting Lightify playback\u{2026}")
                    } else {
                        tr!("One-time Spotify streaming sign-in \u{2014} finish it in your browser")
                    }
                };
                let msg = say();
                push_engine_status(&weak, say, false);
                set_status(&weak, msg);
            }
            Err(e) => {
                let e2 = e.clone();
                push_engine_status(&weak, move || tr!("Failed \u{2014} {}", e2), false);
                set_status(&weak, tr!("Playback engine \u{2014} {}", e));
            }
        }
    });
}

/// Our engine's device is gone (stopped, restarting, exited): forget its id, so player
/// commands take the Web API — and say so if that fails — instead of going to an
/// engine that isn't there or isn't ready, which dropped them without a word.
fn forget_engine_device(engine_device_id: &mut String) {
    engine_device_id.clear();
    if let Ok(mut d) = ENGINE_DEVICE_ID.lock() {
        d.clear();
    }
}

/// Build the audio-output rows: "System default" first, then the real devices, with
/// the configured one ticked. When the configured device has vanished the note says
/// so plainly instead of leaving the user staring at an unticked list.
/// The output names Settings is showing, in row order after "System default". A pick
/// is a row index, so it must be read against THIS list: re-enumerating at click time
/// (as this used to) shifted every row when a device came or went, or the OS default
/// changed, in between — saving a device the user never clicked.
static SHOWN_OUTPUTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn push_outputs(weak: &slint::Weak<MainWindow>) {
    let configured = lightify_core::config::load_config(&lightify_core::config::data_dir()).audio_output;
    let outputs = engine::audio_outputs();
    if let Ok(mut g) = SHOWN_OUTPUTS.lock() {
        *g = outputs.clone();
    }
    let want = configured.trim();
    let missing = !want.is_empty()
        && !want.eq_ignore_ascii_case("default")
        && !outputs.iter().any(|n| n == want);
    let default_active = want.is_empty() || want.eq_ignore_ascii_case("default") || missing;

    let mut rows: Vec<DeviceRow> = Vec::with_capacity(outputs.len() + 1);
    rows.push(DeviceRow { name: tr!("System default").into(), active: default_active });
    for name in &outputs {
        rows.push(DeviceRow { name: name.clone().into(), active: !default_active && name == want });
    }
    let note = if missing {
        tr!("Configured output \u{201c}{}\u{201d} isn\u{2019}t connected \u{2014} playing on the system default.", want)
    } else {
        String::new()
    };
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_settings_outputs(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        app.set_settings_output_note(note.into());
    });
}

/// A line Rust shows in Settings, kept as how to say it rather than what it said, so a
/// language switch can say it again (`relabel_settings_lines`).
type Say = Box<dyn Fn() -> String + Send>;

static ENGINE_LINE: std::sync::Mutex<Option<(Say, bool)>> = std::sync::Mutex::new(None);
static DEVICE_LINE: std::sync::Mutex<Option<Say>> = std::sync::Mutex::new(None);

/// Settings › playback engine's status line: `say` builds it (`|| tr!("Not running")`).
fn push_engine_status(weak: &slint::Weak<MainWindow>, say: impl Fn() -> String + Send + 'static, ok: bool) {
    let text = say();
    if let Ok(mut g) = ENGINE_LINE.lock() {
        *g = Some((Box::new(say), ok));
    }
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_engine_status(text.into());
        app.set_engine_ok(ok);
    });
}

/// After a language switch: the engine and device lines in the new language.
fn relabel_settings_lines(weak: &slint::Weak<MainWindow>) {
    let engine = ENGINE_LINE.lock().ok().and_then(|g| g.as_ref().map(|(say, ok)| (say(), *ok)));
    let device = DEVICE_LINE.lock().ok().and_then(|g| g.as_ref().map(|say| say()));
    let _ = weak.upgrade_in_event_loop(move |app| {
        if let Some((text, ok)) = engine {
            app.set_engine_status(text.into());
            app.set_engine_ok(ok);
        }
        if let Some(text) = device {
            app.set_settings_device_status(text.into());
        }
    });
}

/// The engine's own Connect device, as the **Web API** sees it.
///
/// Registration propagates to `me/player/devices` a second or two after the engine
/// says `ready`, so poll until it shows up — the same wait the shipped host does in
/// `ensureDevice`. Matched by id only: the "Lightify" name is shared by the phone app
/// and by a previous run's session Spotify hasn't dropped yet, and adopting one of
/// those aimed every later call (and a transfer) at the wrong device.
async fn find_engine_device(
    session: &mut Session,
    device_id: &str,
    attempts: usize,
) -> Option<Device> {
    find_engine_device_or(session, device_id, attempts).await.ok().flatten()
}

/// `find_engine_device`, telling a rate limit (or no network) apart from "not listed
/// yet": those end the wait at once with Spotify's own error, instead of asking ~12
/// more times and then blaming the listing.
async fn find_engine_device_or(
    session: &mut Session,
    device_id: &str,
    attempts: usize,
) -> Result<Option<Device>, String> {
    if device_id.is_empty() {
        return Ok(None);
    }
    for i in 0..attempts.max(1) {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        let devices = match session.devices().await {
            Ok(d) => d,
            Err(e) if rate_limit_backoff(&e).is_some() || lightify_core::net::is_offline_error(&e) => return Err(e),
            Err(_) => Vec::new(),
        };
        if let Some(d) = engine_device_in(&devices, device_id) {
            return Ok(Some(d.clone()));
        }
    }
    Ok(None)
}

/// Our engine's device in a `me/player/devices` answer, by its id alone.
fn engine_device_in<'a>(devices: &'a [Device], device_id: &str) -> Option<&'a Device> {
    devices.iter().find(|d| !device_id.is_empty() && d.id == device_id)
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
///
/// * **Launch never takes playback from another device that is playing** (owner, Q2):
///   music on the phone or another PC stays there; Lightify only becomes the active
///   device when nothing else is playing (`launch_takes_playback`).
async fn adopt_engine_device(session: &mut Session, device_id: &str) -> Result<String, String> {
    // Aim every subsequent player call at this device, listed yet or not.
    session.remember_device(device_id);
    let d = find_engine_device_or(session, device_id, 12)
        .await?
        .ok_or_else(|| tr!("the engine registered but Spotify hasn\u{2019}t listed it yet"))?;
    if !d.is_active {
        let now = session.playback().await;
        if launch_takes_playback(&d, &now) {
            if let Err(e) = session.transfer_playback(&d.id, false).await {
                eprintln!("[lightify] transfer to {} failed (continuing): {e}", d.id);
            }
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
        tr!("{}m", (secs + 59) / 60)
    } else {
        tr!("{}s", secs)
    }
}

// ── UI marshaling helpers (run the closure on the Slint thread) ──────────────

/// The titlebar pill: notices only (progress, confirmations, errors), never a
/// resting state. Anything that isn't still in progress (a trailing "…") clears
/// itself after a few seconds, unless a newer notice has replaced it.
fn set_status(weak: &slint::Weak<MainWindow>, text: String) {
    show_status(weak, text, false);
}

/// `set_status` for a line that describes a state, not a notice ("Offline —
/// reconnecting (next try in …)"): it stays until something replaces it, where a
/// notice goes after 5 s. It used to vanish while still offline.
fn set_status_held(weak: &slint::Weak<MainWindow>, text: String) {
    show_status(weak, text, true);
}

fn show_status(weak: &slint::Weak<MainWindow>, text: String, held: bool) {
    static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let gen = GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let weak2 = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |app| {
        let transient = !held && !text.is_empty() && !text.ends_with('\u{2026}');
        app.set_status_text(text.into());
        if transient {
            slint::Timer::single_shot(std::time::Duration::from_secs(5), move || {
                if GEN.load(std::sync::atomic::Ordering::Relaxed) == gen {
                    if let Some(a) = weak2.upgrade() {
                        a.set_status_text("".into());
                    }
                }
            });
        }
    });
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
        name: tr!("Liked Songs").into(),
        sub: tr!("Your saved Spotify tracks").into(),
        active: liked_active,
        playing: liked_active && playing,
        liked: true,
    });
    for &idx in order {
        let Some(p) = pls.get(idx) else { continue };
        let n = group_thousands(p.tracks);
        let sub = if p.owner.is_empty() {
            lightify_i18n::trn!("{} tracks", "{} tracks", p.tracks, n)
        } else {
            lightify_i18n::trn!("{} tracks \u{2022} {}", "{} tracks \u{2022} {}", p.tracks, n, p.owner)
        };
        let active = !active_id.is_empty() && p.id == active_id;
        rows.push(Row {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            sub: sub.into(),
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
        app.set_list_filter_note("".into());
    });
}

/// The filter pill's note while the open track list is still paging: the matches come
/// from what has loaded so far, and more load as the list scrolls (or by themselves
/// while the filtered list is short). Nothing once the whole list is in.
fn filter_loaded_note(pager: &TrackPager, loaded: usize) -> String {
    if pager.done || pager.source == PagerSource::None {
        String::new()
    } else if pager.total > 0 {
        tr!("{} / {} loaded", group_thousands(loaded as u32), group_thousands(pager.total))
    } else {
        tr!("{} loaded", group_thousands(loaded as u32))
    }
}

fn sort_playlist_view(pls: &[lightify_core::Playlist], mode: &str, dir: &str) -> Vec<usize> {
    playlist_order(pls, mode, dir, &filter_of(&PL_FILTER))
}

/// `sort_playlist_view` with the filter given (`""`: every playlist, as the command
/// palette lists them whatever is typed over the library).
fn playlist_order(pls: &[lightify_core::Playlist], mode: &str, dir: &str, q: &str) -> Vec<usize> {
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

/// A resume that failed to start: the offer goes back on screen (it never left it).
fn put_back_resume_offer(rp: ResumePoint) {
    if let Ok(mut g) = RESUME_OFFER.lock() {
        *g = Some(rp);
    }
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
    // The paused track + play button already say "resume"; no status line for it.
    set_status(weak, String::new());
    if !rp.album_art.is_empty() && *last_art != rp.album_art {
        match session.fetch_bytes(&rp.album_art).await.ok().and_then(|b| decode_art(&b)) {
            Some((rgba, w, h, glow)) => {
                *last_art = rp.album_art.clone();
                set_art(weak, rgba, w, h, glow);
            }
            // Not shown: a later read of this song tries again.
            None => last_art.clear(),
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
/// a poll agrees, or when the hold runs out. `*_confirmed`: our own device has since
/// reported that very value (see `accept_device_options`).
struct OptionsHold {
    shuffle: Option<bool>,
    repeat: Option<String>,
    until: std::time::Instant,
    shuffle_confirmed: bool,
    repeat_confirmed: bool,
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
        arm_options_hold(&mut g, shuffle, repeat.clone(), std::time::Instant::now());
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

/// Hold `shuffle`/`repeat` (whichever is given) from `now` on. A hold that has run out
/// is dropped first: only a poll used to clear one, so the next press re-armed whatever
/// it still held (a shuffle hold from minutes ago came back with a repeat press).
fn arm_options_hold(slot: &mut Option<OptionsHold>, shuffle: Option<bool>, repeat: Option<String>, now: std::time::Instant) {
    if slot.as_ref().is_some_and(|h| now >= h.until) {
        *slot = None;
    }
    let h = slot.get_or_insert(OptionsHold {
        shuffle: None,
        repeat: None,
        until: now,
        shuffle_confirmed: false,
        repeat_confirmed: false,
    });
    if shuffle.is_some() {
        h.shuffle = shuffle;
        h.shuffle_confirmed = false;
    }
    if repeat.is_some() {
        h.repeat = repeat;
        h.repeat_confirmed = false;
    }
    h.until = now + OPTIONS_HOLD_FOR;
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

/// Filter one shuffle/repeat report from our own engine through the hold, returning
/// what to show. With no hold the device is the authority and wins outright. While a
/// fresh choice is held:
/// * a report of that very value confirms it — and the hold stays, so the polls that
///   still carry the old value for a few seconds can't put the button back (this
///   used to release the hold, which let exactly that happen);
/// * any other value *before* that is the device passing through a step of the
///   change (librespot reports each flag separately: repeat-one → off reads
///   "repeat-one off" = repeat-all, then "repeat-all off"), and isn't shown;
/// * any other value *after* it is a real change from elsewhere, and wins.
fn accept_device_options(
    shuffle: Option<bool>,
    repeat: Option<String>,
) -> (Option<bool>, Option<String>) {
    let Ok(mut g) = OPTIONS_HOLD.lock() else { return (shuffle, repeat) };
    let Some(h) = g.as_mut() else { return (shuffle, repeat) };
    if std::time::Instant::now() >= h.until {
        *g = None;
        return (shuffle, repeat);
    }
    let shuffle = match (shuffle, h.shuffle) {
        (Some(v), Some(held)) if v == held => {
            h.shuffle_confirmed = true;
            Some(v)
        }
        (Some(v), Some(_)) if h.shuffle_confirmed => {
            h.shuffle = None;
            Some(v)
        }
        (Some(_), Some(_)) => None,
        (v, None) => v,
        (None, Some(_)) => None,
    };
    let repeat = match (repeat, h.repeat.clone()) {
        (Some(v), Some(held)) if v == held => {
            h.repeat_confirmed = true;
            Some(v)
        }
        (Some(v), Some(_)) if h.repeat_confirmed => {
            h.repeat = None;
            Some(v)
        }
        (Some(_), Some(_)) => None,
        (v, None) => v,
        (None, Some(_)) => None,
    };
    if h.shuffle.is_none() && h.repeat.is_none() {
        *g = None;
    }
    (shuffle, repeat)
}

/// The repeat mode the user wants: their last press, or the last *announced* change
/// (our engine's own reports, another device's state). Kept apart from what polls of
/// our device say, because a load silently resets repeat there (librespot's
/// `handle_load` → `reset_options`) and a poll can't tell that from a real change —
/// this is what gets put back afterwards (`restore_repeat_after_load`).
static REPEAT_CHOICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Our engine's Connect device id, for code that only sees a `PlaybackState`.
static ENGINE_DEVICE_ID: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// A track event this soon after a play request is that request's load.
const PLAY_LOAD_WINDOW: Duration = Duration::from_secs(15);

/// Shuffle as pressed while nothing was loaded (no `last` to hold it): the next load
/// starts with it, as `REPEAT_CHOICE` does for repeat. It used to be lost.
static SHUFFLE_CHOICE: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

fn set_shuffle_choice(on: bool) {
    if let Ok(mut g) = SHUFFLE_CHOICE.lock() {
        *g = Some(on);
    }
}

/// Shuffle for a new load: what is playing says, else the choice made with nothing loaded.
fn load_shuffle(last: &Option<PlaybackState>) -> bool {
    match last {
        Some(p) => p.shuffle_state,
        None => SHUFFLE_CHOICE.lock().ok().and_then(|g| *g).unwrap_or(false),
    }
}

fn repeat_choice() -> Option<String> {
    REPEAT_CHOICE.lock().ok().and_then(|g| g.clone())
}

fn set_repeat_choice(mode: &str) {
    if let Ok(mut g) = REPEAT_CHOICE.lock() {
        *g = Some(mode.to_string());
    }
}

/// Learn the repeat choice from a poll — wholesale when playback is on another device
/// (that device's state is the choice), but on our own device only the one silent
/// change that is a real one: a skip out of repeat-one leaving repeat-all (a phone's
/// "next", say). A silent drop to "off" is a load's reset, not a choice.
fn note_polled_repeat(pb: &PlaybackState) {
    let ours = engine::running()
        && ENGINE_DEVICE_ID.lock().is_ok_and(|d| !d.is_empty() && pb.device_id == *d);
    if let Ok(mut g) = REPEAT_CHOICE.lock() {
        let skipped_out_of_one = g.as_deref() == Some("track") && pb.repeat_state == "context";
        if g.is_none() || !ours || skipped_out_of_one {
            *g = Some(pb.repeat_state.clone());
        }
    }
}

/// A load we asked for just reached our engine. librespot resets repeat on every load
/// unless the play command carried options, and doesn't say so — so playing a new
/// playlist quietly turned repeat off on the device while the button (until the next
/// poll) still showed it on. Read the device's real state once Spotify shows the new
/// track, and put the user's mode back if the load dropped it. Only the flags that
/// differ are sent.
async fn restore_repeat_after_load(
    weak: &slint::Weak<MainWindow>,
    session: &mut Session,
    last: &mut Option<PlaybackState>,
    engine_device_id: &str,
    track_name: &str,
) {
    let Some(want) = repeat_choice() else { return };
    if !engine::running() || engine_device_id.is_empty() || track_name.is_empty() {
        return;
    }
    for attempt in 0..4 {
        tokio::time::sleep(Duration::from_millis(if attempt == 0 { 450 } else { 650 })).await;
        match session.playback().await {
            Ok(Some(pb)) if pb.device_id != engine_device_id => return,
            Ok(Some(pb)) if pb.track.as_ref().is_some_and(|t| t.name == track_name) => {
                if pb.repeat_state != want {
                    engine::send(
                        &serde_json::json!({ "cmd": "repeat", "mode": want, "from": pb.repeat_state }).to_string(),
                    );
                    // Held, so the polls that still read the reset value don't show it.
                    show_options(weak, last, None, Some(want));
                }
                return;
            }
            Err(e) if lightify_core::ratelimit::is_rate_limited(&e) => return,
            _ => {}
        }
    }
}

fn push_idle(weak: &slint::Weak<MainWindow>) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_track_name("\u{2014}".into());
        app.set_track_artist(tr!("Nothing playing").into());
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
///
/// Rare on purpose: track changes, play/pause, shuffle/repeat and volume all arrive as
/// engine events, and a handoff to another device as `Event::Inactive`.
const ENGINE_OWNED_POLL: Duration = Duration::from_secs(120);
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
    /// Where `track` was at the last tick, as the bar showed it.
    pos_ms: u64,
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
            self.pos_ms = pb.progress_ms;
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
        self.pos_ms = shown;
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

    /// `last` at the position the bar shows, for saving the resume point between reads.
    fn live(&self, last: &Option<PlaybackState>) -> Option<PlaybackState> {
        let mut pb = last.clone()?;
        if self.anchor_at.is_some() && pb.track.as_ref().is_some_and(|t| t.id == self.track) {
            pb.progress_ms = self.pos_ms;
        }
        Some(pb)
    }
}

/// What was playing, as last read, and when that was noted: `save_exit_resume` runs
/// the position on from it. Noted only when the track, its read position or the play
/// state changes, so the clock tick doesn't copy the playback state each time.
static EXIT_RESUME: std::sync::Mutex<Option<(PlaybackState, std::time::Instant)>> = std::sync::Mutex::new(None);

fn note_exit_resume(last: &Option<PlaybackState>) {
    let Some(pb) = last.as_ref().filter(|p| p.track.is_some()) else { return };
    let Ok(mut g) = EXIT_RESUME.lock() else { return };
    let same = g.as_ref().is_some_and(|(o, _)| {
        o.is_playing == pb.is_playing
            && o.progress_ms == pb.progress_ms
            && o.track.as_ref().map(|t| &t.uri) == pb.track.as_ref().map(|t| &t.uri)
    });
    if !same {
        *g = Some((pb.clone(), std::time::Instant::now()));
    }
}

/// `pb` (read `since` ago) at the position playback has reached since.
fn exit_resume_point(mut pb: PlaybackState, since: Duration) -> PlaybackState {
    if pb.is_playing {
        pb.progress_ms = pb.progress_ms.saturating_add(since.as_millis() as u64).min(pb.duration_ms);
    }
    pb
}

/// On exit: save the resume point where the music had got to (`save_resume` skips a
/// write within 5 s of the last one, so at most that much is lost).
fn save_exit_resume() {
    let noted = EXIT_RESUME.lock().ok().and_then(|g| g.clone());
    if let Some((pb, at)) = noted {
        save_resume(&exit_resume_point(pb, at.elapsed()));
    }
}

/// How often the resume point is saved while something plays. A read saves it too, but
/// those are up to `ENGINE_OWNED_POLL` apart on our own engine, so the offer at the next
/// launch could start minutes back.
const RESUME_SAVE_EVERY: Duration = Duration::from_secs(10);

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

/// `settle_refresh` without holding the worker: the re-read happens on the loop's
/// `poll_soon` timer, so the next click isn't queued behind a 350 ms sleep and three
/// requests. A pending earlier deadline wins; two plays in a row read once.
fn refresh_soon(poll_soon: &mut Option<tokio::time::Instant>) {
    let at = tokio::time::Instant::now() + Duration::from_millis(350);
    *poll_soon = Some(poll_soon.map_or(at, |p| p.min(at)));
}

/// What the Web API is sent for a uri-list play when our engine can't take it.
/// (`me/player/play` with `uris` has a practical length limit.)
const WEB_API_URIS_MAX: usize = 50;

/// Play the plain track list `uris` starting at `index` (Liked Songs, an artist's
/// tracks, a selection — anything without a context). On our own engine it is
/// loaded directly: the whole list, no Web-API round trip, shuffle/repeat kept.
/// Returns whether it went that way; otherwise only `WEB_API_URIS_MAX` tracks from
/// `index` went through the Web API, and only those will play.
async fn play_track_list(
    session: &mut Session,
    uris: &[String],
    index: usize,
    position_ms: u64,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
) -> Result<bool, String> {
    if uris.is_empty() {
        return Err(tr!("Nothing to play"));
    }
    let index = index.min(uris.len() - 1);
    if engine::running() && !engine_device_id.is_empty() {
        let shuffle = load_shuffle(last);
        let repeat = repeat_choice()
            .or_else(|| last.as_ref().map(|p| p.repeat_state.clone()))
            .unwrap_or_else(|| "off".into());
        engine::send(
            &serde_json::json!({
                "cmd": "loadtracks", "uris": uris, "index": index, "position_ms": position_ms.min(u32::MAX as u64),
                "shuffle": shuffle, "repeat": repeat,
            })
            .to_string(),
        );
        note_engine_load();
        return Ok(true);
    }
    let end = (index + WEB_API_URIS_MAX).min(uris.len());
    if position_ms > 0 && end - index == 1 {
        // One song: the position rides on the play itself, so it can't start at 0.
        session.play_at(None, &uris[index], position_ms).await?;
        return Ok(false);
    }
    if position_ms > 0 {
        // The position rides on the play itself, for a list too: a seek sent after it
        // could land before the load and be lost, and the song started at 0 (DM2-13).
        session.play_uris_at(&uris[index..end], position_ms).await?;
    } else {
        session.play_uris(&uris[index..end]).await?;
    }
    Ok(false)
}

/// The account's Liked Songs as a playable context, or `None` without a user id.
/// What's wrong with the account the player signed in with, if anything: an account
/// other than Lightify's plays songs the rest of the app knows nothing about. Unknown
/// details are never a problem.
fn streaming_account_problem(username: &str, account_type: &str, catalogue: &str, web_user: &str) -> Option<String> {
    // No Free-account branch: librespot 0.8 ends the process itself for a non-Premium
    // account (`Session::check_catalogue`), before the session is ever ready.
    let _ = (account_type, catalogue);
    if !username.is_empty() && !web_user.is_empty() && !username.eq_ignore_ascii_case(web_user) {
        return Some(tr!(
            "The player is signed in as \u{201c}{}\u{201d}, not the account Lightify uses (\u{201c}{}\u{201d}). Sign in again with the same account.",
            username,
            web_user
        ));
    }
    None
}

/// Start `context_uri` from its first track (or shuffled, with shuffle on) on our own
/// engine when it is up: a local load, without the Web API's round trip through
/// Spotify back to this same device. The Web API otherwise, as before.
async fn play_context_from_start(
    session: &mut Session,
    context_uri: &str,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
) -> Result<(), String> {
    if engine_play_context(context_uri, "", 0, last, engine_device_id) {
        return Ok(());
    }
    session.play_context_start(context_uri).await
}

/// Have our engine fetch `context_uri` now, while the user looks at the list, so a
/// click on one of its rows starts the track without first waiting for the whole
/// list (~0.7 s for a large Liked Songs, measured). One context request, no Web-API
/// quota and no audio key; nothing happens when the engine isn't running.
fn prepare_context(context_uri: &str) {
    if engine::running() && !context_uri.is_empty() {
        engine::send(&serde_json::json!({ "cmd": "preparecontext", "context_uri": context_uri }).to_string());
    }
}

fn liked_context(session: &Session) -> Option<String> {
    let id = session.user_id();
    (!id.is_empty()).then(|| format!("spotify:user:{id}:collection"))
}

/// Start `context_uri` at `track_uri` on our own engine, which resolves the context
/// itself (the only way to start Liked Songs as a whole: the Web API won't). False
/// when the engine isn't up to take it.
fn engine_play_context(
    context_uri: &str,
    track_uri: &str,
    position_ms: u64,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
) -> bool {
    engine_load_context(context_uri, track_uri, position_ms, last, engine_device_id, false)
}

/// `engine_play_context`; `keep_removed` for a jump to a later row of the list already
/// playing, where songs removed from QUEUE must stay gone (BUGS-PLAN QD5).
fn engine_load_context(
    context_uri: &str,
    track_uri: &str,
    position_ms: u64,
    last: &Option<PlaybackState>,
    engine_device_id: &str,
    keep_removed: bool,
) -> bool {
    if !engine::running() || engine_device_id.is_empty() {
        return false;
    }
    let shuffle = load_shuffle(last);
    let repeat = repeat_choice()
        .or_else(|| last.as_ref().map(|p| p.repeat_state.clone()))
        .unwrap_or_else(|| "off".into());
    engine::send(
        &serde_json::json!({
            "cmd": "loadcontext", "context_uri": context_uri, "track_uri": track_uri,
            "position_ms": position_ms.min(u32::MAX as u64), "shuffle": shuffle, "repeat": repeat,
            "keep_removed": keep_removed,
        })
        .to_string(),
    );
    note_engine_load();
    true
}

/// When we last asked our engine to load something (`loadtracks` / `loadcontext`).
/// The track such a load brings is shown from the engine's own event at once, then
/// read from the Web API ONCE, a moment later, for what only a read knows: the new
/// context (the library's "playing here" row) — not the 2–6 reads a Web-API load needs.
static ENGINE_LOAD_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn note_engine_load() {
    if let Ok(mut g) = ENGINE_LOAD_AT.lock() {
        *g = Some(std::time::Instant::now());
    }
}

/// Was there an engine load in the last few seconds? Consumes it.
fn take_engine_load() -> bool {
    ENGINE_LOAD_AT
        .lock()
        .ok()
        .and_then(|mut g| g.take())
        .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
}

/// Is `uri` a Liked Songs context (`spotify:user:<id>:collection`)?
fn is_liked_context(uri: &str) -> bool {
    uri.starts_with("spotify:user:") && uri.ends_with(":collection")
}

/// Recent search answers, so going back to a query (or backspacing onto one) shows
/// it again without another request. Short-lived: results do change.
static SEARCH_CACHE: std::sync::Mutex<Vec<(String, std::time::Instant, SearchResults)>> =
    std::sync::Mutex::new(Vec::new());
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(300);
const SEARCH_CACHE_MAX: usize = 20;

fn cached_search(q: &str) -> Option<SearchResults> {
    let key = q.trim().to_lowercase();
    let g = SEARCH_CACHE.lock().ok()?;
    g.iter()
        .find(|(k, at, _)| *k == key && at.elapsed() < SEARCH_CACHE_TTL)
        .map(|(_, _, r)| r.clone())
}

fn cache_search(q: &str, r: &SearchResults) {
    let key = q.trim().to_lowercase();
    if let Ok(mut g) = SEARCH_CACHE.lock() {
        g.retain(|(k, at, _)| *k != key && at.elapsed() < SEARCH_CACHE_TTL);
        if g.len() >= SEARCH_CACHE_MAX {
            g.remove(0);
        }
        g.push((key, std::time::Instant::now(), r.clone()));
    }
}

/// Whether a Spotify plan (`me.product`) is one Lightify's player and downloader are
/// made for. An unknown answer is never held against anyone.
fn plan_is_premium(product: &str) -> bool {
    let p = product.trim();
    p.is_empty() || p.eq_ignore_ascii_case("premium")
}

/// The Free-account notes, or nothing on Premium: the status line once, and a line on
/// the playback-engine and downloader cards in Settings. Informational only.
fn show_plan_notes(weak: &slint::Weak<MainWindow>, product: &str) {
    let free = !plan_is_premium(product);
    let (engine, downloader) = if free {
        set_status(weak, tr!("This is a Spotify Free account. Lightify\u{2019}s player and downloader need Spotify Premium to work properly. Local mode works with any account."));
        (
            tr!("This account is Spotify Free. The player needs Spotify Premium, so songs may not play. Local mode works with any account."),
            tr!("This account is Spotify Free. The downloader needs Spotify Premium, so downloads may not work."),
        )
    } else {
        (String::new(), String::new())
    };
    let _ = weak.upgrade_in_event_loop(move |app| {
        app.set_engine_plan_note(engine.into());
        app.set_downloader_plan_note(downloader.into());
    });
}

fn connected_status(session: &Session) -> String {
    let n = session.display_name();
    if n.is_empty() { tr!("Connected") } else { tr!("Connected as {}", n) }
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
        lightify_i18n::trn!("Artist \u{2022} {} followers", "Artist \u{2022} {} followers", a.followers, compact_count(a.followers))
    } else {
        tr!("Artist")
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
            sub: tr!("Album \u{2022} {}", a.artists).into(),
            kind: "album".into(),
            selected: false,
            art: a.thumb.clone().into(),
            meta: Default::default(),
        },
        SearchAction::Album { id: a.id.clone(), uri: a.uri.clone(), name: a.name.clone() },
    )
}

fn search_playlist_row(p: &lightify_core::Playlist) -> (SearchRow, SearchAction) {
    let n = group_thousands(p.tracks);
    let sub = if p.owner.is_empty() {
        lightify_i18n::trn!("Playlist \u{2022} {} tracks", "Playlist \u{2022} {} tracks", p.tracks, n)
    } else {
        lightify_i18n::trn!("Playlist \u{2022} {} tracks \u{2022} {}", "Playlist \u{2022} {} tracks \u{2022} {}", p.tracks, n, p.owner)
    };
    (
        SearchRow {
            name: p.name.clone().into(),
            sub: sub.into(),
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
        // Below 999,950: from there "{:.1}K" rounds up to "1000K", which is 1M.
        10_000..=999_949 => format!("{:.1}K", n as f64 / 1_000.0).replace(".0K", "K"),
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
                    meta: tr!("See all").into(),
                });
                actions.push(SearchAction::Header { see_all: see_all.to_string() });
                for (row, action) in items.into_iter().take(SEARCH_SECTION_ROWS) {
                    rows.push(row);
                    actions.push(action);
                }
            };
            let songs = tracks.iter().enumerate().filter(|(i, _)| !skip("track", *i)).map(|(_, t)| search_track_row(r, t)).collect();
            section(&tr!("Songs"), "tracks", songs, &mut rows);
            let artists = artist_order.iter().filter(|&&i| !skip("artist", i)).map(|&i| search_artist_row(&r.artists[i])).collect();
            section(&tr!("Artists"), "artists", artists, &mut rows);
            let albs = albums.iter().enumerate().filter(|(i, _)| !skip("album", *i)).map(|(_, a)| search_album_row(a)).collect();
            section(&tr!("Albums"), "albums", albs, &mut rows);
            let pls = r.playlists.iter().enumerate().filter(|(i, _)| !skip("playlist", *i)).map(|(_, p)| search_playlist_row(p)).collect();
            section(&tr!("Playlists"), "playlists", pls, &mut rows);
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
        name: tr!("Recent searches").into(),
        sub: Default::default(),
        kind: "header".into(),
        selected: false,
        art: Default::default(),
        meta: tr!("Clear").into(),
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
    if let Ok(mut tops) = SEARCH_ROW_TOPS.lock() {
        *tops = search_row_tops(&rows);
    }
    let keys: Vec<String> = rows.iter().map(|r| format!("{}\u{1f}{}\u{1f}{}", r.kind, r.name, r.sub)).collect();
    let v = list_pushed(SEL_SEARCH as usize, list_identity(keys.iter().map(|k| k.as_str())));
    thumbs::request_search(weak, &rows);
    let _ = weak.upgrade_in_event_loop(move |app| {
        // A page loaded under a filter extends the list: append in place, so the
        // list keeps its scroll position instead of jumping back to the top. Rows are
        // compared without their selection marks: a selected row used to fail the
        // check, so with anything selected every page jumped the list to the top.
        let cur = app.get_search_results();
        let art = app.get_search_art();
        let shown: Vec<SearchRow> = cur.iter().collect();
        let same = |a: &SearchRow, b: &SearchRow| SearchRow { selected: b.selected, ..a.clone() } == *b;
        let kept = rows.len() >= shown.len() && shown.iter().zip(&rows).all(|(a, b)| same(a, b));
        let appendable = !shown.is_empty() && kept && art.row_count() == shown.len();
        if let (true, Some(vm), Some(am)) = (
            appendable,
            cur.as_any().downcast_ref::<slint::VecModel<SearchRow>>(),
            art.as_any().downcast_ref::<slint::VecModel<slint::Image>>(),
        ) {
            // Unchanged rows stay as they are (marks included); only new ones go on,
            // and an unchanged page touches nothing (the covers used to flicker).
            for r in rows.into_iter().skip(vm.row_count()) {
                am.push(thumbs::search_art(&r));
                vm.push(r);
            }
        } else {
            let mut rows = rows;
            // The same rows (identity unchanged): the selection still stands, so its
            // marks carry over instead of vanishing under a live selection.
            if LIST_SHOWN[SEL_SEARCH as usize].load(std::sync::atomic::Ordering::SeqCst) == v {
                carry_marks(&shown, &mut rows, |r| r.selected, |r, on| r.selected = on, |a, b| a.name == b.name && a.kind == b.kind);
            }
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

fn current_uri(last: &Option<PlaybackState>) -> String {
    last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default()
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
            // RECENT: when it played. QUEUE: how long it is (the v6 queue design).
            time: if show_time {
                t.added_at.as_deref().map(rel_time).unwrap_or_default().into()
            } else if t.duration_ms > 0 {
                fmt_time(t.duration_ms).into()
            } else {
                slint::SharedString::new()
            },
            playing: !playing_id.is_empty() && t.id == *playing_id,
            selected: false,
            pending: 0,
        })
        .collect()
}

/// A Beatport chart lining up on our engine, as the QUEUE panel shows it: the strip
/// under the header, and — after the tracks already found (the queue mirror) — the
/// chart rows still to be looked up, as placeholders. Read by `set_sidebar`, so
/// every path that redraws the QUEUE panel keeps them.
struct BpPanel {
    /// The sequence it belongs to; shown only while that is the live one.
    seq_id: u64,
    active: std::sync::Arc<std::sync::atomic::AtomicU64>,
    title: String,
    found: usize,
    total: usize,
    /// Chart rows not looked up yet, (title, artists), next first; the first of them
    /// is chart row `next_index` of the matcher's source.
    waiting: Vec<(String, String)>,
    next_index: usize,
    /// The chart row the play started from (`total` counts from here).
    start: usize,
    done: bool,
    missed: usize,
}

/// The placeholder rows for `source[next..]`.
fn bp_waiting(source: &[BeatportTrack], next: usize) -> Vec<(String, String)> {
    source
        .get(next..)
        .unwrap_or(&[])
        .iter()
        .map(|b| (b.name.trim().to_string(), b.artists.clone()))
        .collect()
}

/// The matcher has looked up chart rows up to `next_index`, `found` of them on
/// Spotify: drop the placeholders it got through.
fn bp_panel_progress(seq_id: u64, found: usize, next_index: usize, done: bool) {
    let Ok(mut g) = BP_PANEL.lock() else { return };
    let Some(p) = g.as_mut().filter(|p| p.seq_id == seq_id) else { return };
    let through = next_index.saturating_sub(p.next_index).min(p.waiting.len());
    p.waiting.drain(..through);
    p.next_index = p.next_index.max(next_index);
    p.found = found;
    p.missed = p.next_index.saturating_sub(p.start).saturating_sub(found);
    p.done = done || p.waiting.is_empty();
}

static BP_PANEL: std::sync::Mutex<Option<BpPanel>> = std::sync::Mutex::new(None);

fn set_bp_panel(panel: Option<BpPanel>) {
    if let Ok(mut g) = BP_PANEL.lock() {
        *g = panel;
    }
}

/// `chart` from `source[next..]`, as placeholder rows plus the strip's numbers.
fn bp_panel_rows() -> (Vec<SideRow>, String, i32, i32, String) {
    let Ok(g) = BP_PANEL.lock() else { return (Vec::new(), String::new(), 0, 0, String::new()) };
    let Some(p) = g.as_ref().filter(|p| p.active.load(std::sync::atomic::Ordering::SeqCst) == p.seq_id) else {
        return (Vec::new(), String::new(), 0, 0, String::new());
    };
    if p.done {
        let note = if p.missed == 0 {
            lightify_i18n::trn!("All {n} found on Spotify", "All {n} found on Spotify", p.found)
        } else {
            // Short enough for the 300 px drawer in the bundled font (the longer
            // "… · 4 aren't on Spotify" was cut off).
            lightify_i18n::trn!(
                "{} of {} found \u{00b7} {n} not on Spotify",
                "{} of {} found \u{00b7} {n} not on Spotify",
                p.missed, p.found, p.total
            )
        };
        return (Vec::new(), String::new(), 0, 0, note);
    }
    let rows = p
        .waiting
        .iter()
        .enumerate()
        .map(|(i, (name, artist))| SideRow {
            name: name.clone().into(),
            artist: artist.clone().into(),
            time: slint::SharedString::new(),
            playing: false,
            selected: false,
            pending: if i == 0 { 1 } else { 2 },
        })
        .collect();
    (rows, p.title.clone(), p.found as i32, p.total as i32, String::new())
}

/// "beatport · top 100 · tech house" — the strip's label for a chart.
fn bp_chart_title(genres: &[(String, String)], idx: usize, kind: &str) -> String {
    let what = match kind {
        "hype" => "hype 100",
        "releases" => "top 100 releases",
        _ => "top 100",
    };
    match genres.get(idx).map(|(name, slug)| (name, slug.is_empty())) {
        Some((name, false)) => format!("// beatport \u{00b7} {what} \u{00b7} {}", name.to_lowercase()),
        _ => format!("// beatport \u{00b7} {what}"),
    }
}

/// Push the sidebar mode + rows to the UI. `empty` (when non-blank) sets the
/// no-rows placeholder text; a blank `empty` leaves the current text untouched.
/// A play of ours just counted (play_history.rs): if RECENT is open, show it now.
fn show_new_plays(weak: &slint::Weak<MainWindow>, sidebar_mode: i32, sidebar_tracks: &mut Vec<Track>, recent_spotify: &[Track], playing_id: &str) {
    if play_history::take_changed() && sidebar_mode == 2 {
        *sidebar_tracks = play_history::merged(recent_spotify);
        set_sidebar(weak, 2, build_side_rows(sidebar_tracks, playing_id, true), tr!("No recent tracks yet"));
    }
}

fn set_sidebar(weak: &slint::Weak<MainWindow>, mode: i32, rows: Vec<SideRow>, empty: String) {
    // Identity is the track, not its relative "5m ago" time, which ticks on its own.
    let keys: Vec<String> = rows.iter().map(|r| format!("{mode}\u{1f}{}\u{1f}{}", r.name, r.artist)).collect();
    let v = list_pushed(SEL_SIDEBAR as usize, list_identity(keys.iter().map(|k| k.as_str())));
    // A chart lining up adds its placeholder rows AFTER the identity above: they sit
    // below every real row (so no real row's index moves), and they change every few
    // seconds — counting them would turn every click on a real row into "that list
    // just changed" for as long as the chart lines up.
    let (pending, strip, found, total, done_note) = if mode == 1 { bp_panel_rows() } else { Default::default() };
    let mut rows = rows;
    rows.extend(pending);
    let _ = weak.upgrade_in_event_loop(move |app| {
        // The same rows as on screen (a playing mark moved, a relative time ticked): a
        // selection made on them still stands, so its marks stay. They used to be wiped
        // while "Play N selected" stayed live. Changed rows start unmarked, and the
        // worker drops the selection for them (the list-version check in its loop).
        if LIST_SHOWN[SEL_SIDEBAR as usize].load(std::sync::atomic::Ordering::SeqCst) == v {
            let shown: Vec<SideRow> = app.get_sidebar_rows().iter().collect();
            carry_marks(&shown, &mut rows, |r| r.selected, |r, on| r.selected = on, |a, b| {
                a.pending == 0 && b.pending == 0 && a.name == b.name && a.artist == b.artist
            });
        }
        list_shown(SEL_SIDEBAR as usize, v);
        app.set_bp_strip_title(strip.into());
        app.set_bp_strip_found(found);
        app.set_bp_strip_total(total);
        app.set_bp_done_note(done_note.into());
        app.set_sidebar_mode(mode);
        if !empty.is_empty() {
            // The plain "nothing here" messages get a hint under them; an error or
            // "Loading…" doesn't. Decided here, in the active language — not by the
            // UI comparing the message against English.
            app.set_sidebar_empty_plain(empty == tr!("No queued tracks") || empty == tr!("No recent tracks yet"));
            app.set_sidebar_empty(empty.into());
        }
        if let Some(m) = patch_model(&app.get_sidebar_rows(), rows) {
            app.set_sidebar_rows(m);
        }
    });
}

/// `new` begins with every row of `old`, unchanged: a page added rows below and moved
/// none, so a selection made on `old`'s rows still names the same rows.
fn rows_extend<T: PartialEq>(old: &[T], new: &[T]) -> bool {
    new.len() >= old.len() && new[..old.len()] == *old
}

/// UI thread: copy the selection marks of the rows on screen (`shown`) onto `rows`,
/// wherever the same row (`same`) sits at the same place.
fn carry_marks<T>(
    shown: &[T],
    rows: &mut [T],
    get: impl Fn(&T) -> bool,
    set: impl Fn(&mut T, bool),
    same: impl Fn(&T, &T) -> bool,
) {
    for (old, new) in shown.iter().zip(rows.iter_mut()) {
        if get(old) && same(old, new) {
            set(new, true);
        }
    }
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
                    set_sidebar(weak, 1, build_side_rows(tracks, &pid, false), tr!("No queued tracks"));
                    return;
                }
                *mirror = None;
                save_queue("", &[]);
            }
            // No mirror to fold forward locally — this is the common path for a
            // whole-playlist/album play or a station, which never built one. Ask
            // Spotify directly, in the background — see `Cmd::SidebarFetched` for
            // where the result (and the mirror backfill this used to do inline)
            // lands once it comes back. Rows already showing stay up until then;
            // only an empty panel says "Loading queue…" (blanking it on every
            // refresh read as the queue being cleared).
            *generation += 1;
            if tracks.is_empty() {
                set_sidebar(weak, 1, vec![], tr!("Loading queue\u{2026}"));
            }
            spawn_sidebar_fetch(self_tx, 1, *generation);
        }
        2 => {
            set_sidebar(weak, 2, build_side_rows(tracks, &pid, true), tr!("No recent tracks yet"));
        }
        _ => {}
    }
}

/// A play the QUEUE mirror doesn't describe (a selection, a bare track, a search
/// drill-in row) just started. The mirror is the list it replaced, and folding it
/// forward could keep it on screen, so it goes; the panel reads Spotify's own queue.
fn forget_mirror(
    mirror: &mut Option<QueueMirror>,
    generation: &mut u64,
    sidebar_mode: i32,
    self_tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
) {
    if mirror.take().is_some() {
        persist_mirror(mirror);
    }
    // A queue read already out describes the old list too.
    *generation += 1;
    if sidebar_mode == 1 {
        spawn_delayed_queue_refresh(self_tx);
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
            // Spotify only ever reports ~20 upcoming tracks; a mirror copied from that
            // ran down to an empty panel while the playlist kept playing. Ask for the
            // next window before it does (the answer replaces it — `SidebarFetched`).
            if m.wants_refill() {
                m.refill_for = m.current_uri.clone();
                *generation += 1;
                spawn_sidebar_fetch(self_tx, 1, *generation);
            }
            let moved = m.upcoming.len() != tracks.len()
                || m.upcoming.first().map(|t| &t.uri) != tracks.first().map(|t| &t.uri);
            if moved {
                *tracks = m.upcoming.clone();
                persist_mirror(mirror);
                let pid = current_id(last);
                set_sidebar(weak, 1, build_side_rows(tracks, &pid, false), tr!("No queued tracks"));
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
    /// Copied from Spotify's own `me/player/queue`, which only ever returns ~20
    /// items. Such a mirror is a window onto a longer context, not the whole of it,
    /// so it is topped up from Spotify as it runs low (`tick_queue_sidebar`) instead
    /// of draining to an empty panel while the playlist plays on.
    from_spotify: bool,
    /// The track a top-up was last requested on — at most one per track change.
    refill_for: String,
    /// When this mirror was built, and the track that was playing before the play
    /// that built it (`stale_uri`, empty when none) — see `advance`.
    built_at: std::time::Instant,
    stale_uri: String,
}

/// Top a Spotify-backed mirror up once it is down to this many upcoming tracks.
const QUEUE_REFILL_AT: usize = 5;

impl QueueMirror {
    fn new(current_uri: String, upcoming: Vec<Track>) -> Self {
        Self {
            upcoming,
            current_uri,
            queued: 0,
            from_spotify: false,
            refill_for: String::new(),
            built_at: std::time::Instant::now(),
            stale_uri: String::new(),
        }
    }

    /// Songs queued by hand outlive a new play (a load spares the engine's user queue), so
    /// they play before the new list: the new mirror lists them first, as they will play,
    /// still counted as queued (BUGS-PLAN QD6).
    fn keeping_queued(mut self, old: Option<&QueueMirror>) -> Self {
        if let Some(old) = old {
            let kept: Vec<Track> = old.upcoming.iter().take(old.queued).cloned().collect();
            if !kept.is_empty() {
                self.queued = kept.len();
                self.upcoming.splice(0..0, kept);
            }
        }
        self
    }

    /// Built by a play that replaced `previous_uri` (see `advance`).
    fn replacing(mut self, previous_uri: &str) -> Self {
        if previous_uri != self.current_uri {
            self.stale_uri = previous_uri.to_string();
        }
        self
    }

    /// A mirror backfilled from Spotify's (truncated) queue — see `from_spotify`.
    fn from_spotify(current_uri: String, upcoming: Vec<Track>) -> Self {
        Self { from_spotify: true, ..Self::new(current_uri, upcoming) }
    }

    /// Spotify-backed and running low: time to ask Spotify for the next window.
    /// Once per track, so a context that is genuinely ending isn't re-polled.
    fn wants_refill(&self) -> bool {
        self.from_spotify && self.upcoming.len() <= QUEUE_REFILL_AT && self.refill_for != self.current_uri
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
        if playing_uri.is_empty() {
            return true;
        }
        if playing_uri == self.current_uri {
            // The play this mirror was built for is confirmed: from now on the
            // previous track playing again is a real change (it used to be ignored
            // for 15 s, leaving rows that had already played on screen).
            self.stale_uri.clear();
            return true;
        }
        // Right after a local play, polls can still report the track that was playing
        // before it. That is neither a departure (dropping the mirror swapped the list
        // the user clicked in for Spotify's ~20-item view of the context) nor progress
        // (when it sits further down the same list, folding to it ate the rows above).
        if playing_uri == self.stale_uri && self.built_at.elapsed() < PLAY_LOAD_WINDOW {
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
        // The empty-panel line is shared with QUEUE/RECENT: a refresh still in flight
        // when the panel switched must not write over theirs.
        if !empty.is_empty() && app.get_sidebar_mode() == 3 {
            app.set_sidebar_empty_plain(false);
            app.set_sidebar_empty(empty.into());
        }
        app.set_downloads_clearable(clearable);
        // In place (`patch_model`): replacing the model every 1.5 s rebuilt the list.
        if let Some(m) = patch_model(&app.get_sidebar_dl_rows(), rows) {
            app.set_sidebar_dl_rows(m);
        }
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
    // Asked for while a read was out: that read may predate what changed (a download
    // just added), so read once more when it lands rather than dropping the request.
    static AGAIN: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        AGAIN.store(true, Ordering::SeqCst);
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
        loop {
            AGAIN.store(false, Ordering::SeqCst);
            let result = downloader::snapshot().await;
            match result {
                Ok(items) => {
                    // `updateSidebarClearButton`: Clear is live only when something is
                    // actually in a finished state for `/api/clear_items` to drop, or a
                    // failed row carried past an idle stop (Clear drops those too).
                    let clearable = items.iter().any(|i| i.clearable());
                    set_dl_rows(
                        &weak,
                        build_dl_rows(&items),
                        tr!("No downloads queued"),
                        clearable,
                    );
                }
                // The bridge can't be reached: the rows it last had (failed ones carried
                // past an idle stop included) stay up with the error, instead of
                // vanishing behind it.
                Err(e) => set_dl_rows(&weak, build_dl_rows(&downloader::last_snapshot()), tr!("Downloads \u{2014} {}", e), false),
            }
            if !AGAIN.load(Ordering::SeqCst) {
                break;
            }
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
    set_status(&weak, tr!("Queueing download: {}\u{2026}", label));
    tokio::spawn(async move {
        match downloader::enqueue(&url).await {
            Ok(_) => {
                set_status(&weak, tr!("Download queued: {}", label));
                let _ = tx.send(Cmd::RefreshDownloads);
            }
            Err(e) => {
                set_status(&weak, e);
                // `enqueue` starts a downloader sign-in itself when the account isn't
                // ready yet — same reclaim reasoning as `Cmd::DownloaderLogin` above.
                if downloader::status().await.is_ok_and(|st| st.login_in_progress) {
                    let _ = tx.send(Cmd::DownloaderLoginStarted);
                }
            }
        }
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

/// Match a Beatport selection on Spotify, in order. Paced as the chart matcher is
/// (`BP_MATCH_GAP` after each lookup that went to Spotify), and stopped by the first
/// failed search: lightify-core answers `Ok(None)` only for a real miss, so a rate
/// limit or an outage must not be counted as unmatched. Returns the matches, the real
/// misses, and the error that stopped it.
async fn bp_match_rows(session: &mut Session, rows: &[BeatportTrack]) -> (Vec<Track>, usize, Option<String>) {
    let mut found = Vec::new();
    let mut missed = 0usize;
    for bt in rows {
        match session.beatport_match_with(&bt.name, &bt.artists, "").await {
            Ok((hit, how)) => {
                match hit {
                    Some(t) => found.push(t),
                    None => missed += 1,
                }
                if how != lightify_core::MatchHow::Cached {
                    tokio::time::sleep(BP_MATCH_GAP).await;
                }
            }
            Err(e) => return (found, missed, Some(e)),
        }
    }
    (found, missed, None)
}

/// A chart row whose lookup or queue write failed is tried again on the next pass,
/// not skipped (a skipped row is a hole in the chart), unless it has failed
/// `BP_ROW_TRIES` times in a row. `next_index` has already moved past the row;
/// returns where the next pass starts and the row's new strike count.
fn bp_retry_row(next_index: usize, strikes: u32) -> (usize, u32) {
    if strikes + 1 >= BP_ROW_TRIES {
        (next_index, 0)
    } else {
        (next_index.saturating_sub(1), strikes + 1)
    }
}

/// `bp_retry_row` for the error `e`. Being offline or rate-limited is nothing to do with
/// the row: it is tried again with no strike, so a two-minute blip no longer leaves
/// holes in the chart.
fn bp_retry_row_after(next_index: usize, strikes: u32, e: &str) -> (usize, u32) {
    if bp_error_is_transient(e) {
        (next_index.saturating_sub(1), strikes)
    } else {
        bp_retry_row(next_index, strikes)
    }
}

/// Offline or rate-limited: an error about the connection, not about a chart row.
fn bp_error_is_transient(e: &str) -> bool {
    rate_limit_backoff(e).is_some() || lightify_core::net::is_offline_error(e)
}

/// Chart rows one refill pass may look up, found or not. Misses used not to count, so
/// a run of rows Spotify doesn't have searched on unpaced (and, inline, froze the UI).
const BP_REFILL_LOOKUPS: usize = 8;

/// See `bp_retry_row`; the same three tries `bp_match_background` gives a row.
const BP_ROW_TRIES: u32 = 3;


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
    mut strikes: u32,
) -> BpRefillResult {
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            let completed = next_index >= source.len();
            return (Vec::new(), seen, next_index, completed, Some(e), strikes);
        }
    };
    if let Err(e) = session.ensure_fresh().await {
        let completed = next_index >= source.len();
        return (Vec::new(), seen, next_index, completed, Some(e), strikes);
    }

    let mut queued = Vec::new();
    let mut error = None;
    let mut count = 0usize;
    let mut looked = 0usize;
    while count < BP_REFILL_CHUNK && looked < BP_REFILL_LOOKUPS && next_index < source.len() {
        // Checked on every iteration, not just once at entry: the whole point is that
        // ownership can change *while this loop is mid-flight*, and each iteration
        // makes two more network calls' worth of room for exactly that.
        if active_seq.load(std::sync::atomic::Ordering::SeqCst) != seq_id {
            break;
        }
        // Paced like the chart matcher (`bp_match_background`): the rest waits for the
        // next pass rather than bursting searches into a rate limit.
        if lightify_core::ratelimit::blocked_for().is_some()
            || lightify_core::ratelimit::recent_requests() >= BP_MATCH_BUDGET
        {
            break;
        }
        if looked > 0 {
            tokio::time::sleep(BP_MATCH_GAP).await;
        }
        looked += 1;
        let bt = source[next_index].clone();
        next_index += 1;
        let matched = match session.beatport_match(&bt.name, &bt.artists).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                strikes = 0;
                continue;
            }
            Err(e) => {
                // Tried again on the next pass (see `bp_retry_row`).
                (next_index, strikes) = bp_retry_row_after(next_index, strikes, &e);
                error = Some(e);
                break;
            }
        };
        if !seen.insert(matched.uri.clone()) {
            strikes = 0;
            continue;
        }
        // Re-checked here too: the match call above just awaited a round trip, which is
        // exactly the window a `PlayTrack`/`StartStation`/etc. can land in.
        if active_seq.load(std::sync::atomic::Ordering::SeqCst) != seq_id {
            break;
        }
        if let Err(e) = queue_one(&mut session, &matched.uri, engine_owns_now()).await {
            seen.remove(&matched.uri);
            (next_index, strikes) = bp_retry_row_after(next_index, strikes, &e);
            error = Some(e);
            break;
        }
        strikes = 0;
        queued.push((matched.uri, matched.id));
        count += 1;
    }
    let completed = next_index >= source.len();
    (queued, seen, next_index, completed, error, strikes)
}

/// `bp_refill_background`'s answer: the `(uri, id)` pairs queued, `seen`, `next_index`,
/// whether the chart is fully matched, the error that stopped the pass, and the
/// failing row's strike count (see `bp_retry_row`).
type BpRefillResult = (Vec<(String, String)>, std::collections::HashSet<String>, usize, bool, Option<String>, u32);

/// A Beatport chart playing as one list on our engine (`Cmd::BpPlay`'s engine path):
/// the rest of the chart is appended to it as `bp_match_background` matches it.
struct BpList {
    /// The `BpAutoplay::id` it was started under (`bp_active_seq` while it's current).
    id: u64,
    /// The first track it was loaded with — appends are dropped once it's gone.
    anchor: String,
    /// Tracks on the list so far, and chart rows it started from.
    lined_up: usize,
    total: usize,
    /// Chart rows (of the whole chart) looked up so far, found or not.
    next_index: usize,
}

/// Chart rows matched per append once the first few are in. Small enough that the
/// list keeps growing every few seconds, large enough not to churn the engine.
const BP_APPEND_CHUNK: usize = 5;
/// The first rows after the clicked one go to the engine one at a time, so what plays
/// next is lined up within a second or two of the click.
const BP_FRONT_ROWS: usize = 5;
/// Requests chart matching may have in flight per rolling 30 s window
/// (`ratelimit::WINDOW`). Under the background budget (`BACKGROUND_BUDGET`, 45) on
/// purpose: what the user does meanwhile — a search, a click — keeps the rest.
const BP_MATCH_BUDGET: usize = 25;
/// The least gap between two looked-up rows (a cached answer needs none).
const BP_MATCH_GAP: Duration = Duration::from_millis(120);
/// ISRC lookups are tried for the first rows of every chart, then only while they
/// keep paying off: chart rows are mostly extended mixes, which carry their own ISRC,
/// and Spotify often has only the radio edit — a miss then costs the name search on
/// top. Below one hit in three after this many tries, the rest of the chart goes
/// straight to the name search.
const BP_ISRC_PROBE: u32 = 8;

/// Match `source[next_index..]` on Spotify and self-send the results as
/// `Cmd::BpMatched` for the worker to append. Runs to the end of the chart — nothing
/// here depends on recognising what is playing — and stops the moment the sequence
/// is no longer the current one (`active_seq`). Paced by the rate window rather than
/// a fixed sleep: it uses up to `BP_MATCH_BUDGET` requests per window and waits out a
/// rate limit instead of skipping rows; answers from the match cache cost nothing and
/// don't wait at all.
async fn bp_match_background(
    source: Vec<BeatportTrack>,
    mut next_index: usize,
    mut seen: std::collections::HashSet<String>,
    seq_id: u64,
    active_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
) {
    use lightify_core::MatchHow;
    let current = |a: &std::sync::Arc<std::sync::atomic::AtomicU64>| a.load(std::sync::atomic::Ordering::SeqCst) == seq_id;
    let mut session = match Session::load() {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Cmd::BpMatched { seq_id, tracks: Vec::new(), next_index, done: true, error: Some(e) });
            return;
        }
    };
    let first = next_index;
    let (mut isrc_tries, mut isrc_hits) = (0u32, 0u32);
    let mut strikes = 0u32;
    let mut batch: Vec<Track> = Vec::new();
    let mut since_send = 0usize;
    while next_index < source.len() {
        if !current(&active_seq) {
            return;
        }
        // Wait for room in the window (or out a rate limit) before a row that may
        // need the network.
        if lightify_core::ratelimit::blocked_for().is_some()
            || lightify_core::ratelimit::recent_requests() >= BP_MATCH_BUDGET
        {
            tokio::time::sleep(Duration::from_millis(800)).await;
            continue;
        }
        let bt = &source[next_index];
        let use_isrc = isrc_tries < BP_ISRC_PROBE || isrc_hits * 3 >= isrc_tries;
        let isrc = if use_isrc { bt.isrc.as_str() } else { "" };
        let mut error = None;
        match session.beatport_match_with(&bt.name, &bt.artists, isrc).await {
            Ok((found, how)) => {
                next_index += 1;
                since_send += 1;
                strikes = 0;
                match how {
                    MatchHow::Isrc => {
                        isrc_tries += 1;
                        isrc_hits += 1;
                    }
                    MatchHow::IsrcMissed => isrc_tries += 1,
                    _ => {}
                }
                if let Some(t) = found.filter(|t| seen.insert(t.uri.clone())) {
                    batch.push(t);
                }
                if how != MatchHow::Cached {
                    tokio::time::sleep(BP_MATCH_GAP).await;
                }
            }
            Err(e) => {
                // Retried, not skipped: a skipped row is a hole in the chart.
                // A row that keeps failing on its own is given up on, though —
                // being offline or rate-limited isn't the row failing.
                if !bp_error_is_transient(&e) {
                    strikes += 1;
                    if strikes >= BP_ROW_TRIES {
                        next_index += 1;
                        strikes = 0;
                    }
                }
                error = Some(e);
            }
        }
        let done = next_index >= source.len();
        let chunk = if next_index - first <= BP_FRONT_ROWS { 1 } else { BP_APPEND_CHUNK };
        // Send when a chunk is ready — and every few rows regardless, so the panel's
        // placeholders move on even through a run of rows Spotify doesn't have.
        if batch.len() >= chunk || since_send >= BP_APPEND_CHUNK || done || error.is_some() {
            if !current(&active_seq) {
                return;
            }
            let wait = error.as_deref().map(|e| rate_limit_backoff(e).unwrap_or(5).clamp(2, 120));
            let tracks = std::mem::take(&mut batch);
            since_send = 0;
            let _ = tx.send(Cmd::BpMatched { seq_id, tracks, next_index, done, error });
            if let Some(secs) = wait {
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
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
        tr!("just now")
    } else if d < 3600 {
        tr!("{}m", (d + 30) / 60)
    } else if d < 86400 {
        tr!("{}h", d / 3600)
    } else {
        tr!("{}d", d / 86400)
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

/// Fetch a Beatport chart for the current genre/kind selection, off the command loop:
/// a chart can take up to a minute through its fallbacks, and the worker stays free
/// meanwhile. The rows go blank now; the answer comes back as `Cmd::BpFetched`, tagged
/// with `seq` so a reply for a genre or chart picked before the latest is dropped.
fn bp_fetch(
    weak: &slint::Weak<MainWindow>,
    tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
    genres: &[(String, String)],
    idx: usize,
    kind: &str,
    out: &mut Vec<BeatportTrack>,
    seq: &mut u64,
) {
    let (label, slug) = match genres.get(idx) {
        Some((l, s)) => (l.clone(), s.clone()),
        None => return,
    };
    *seq += 1;
    let this = *seq;
    // The old chart leaves the screen now, and with it the rows a click could reach.
    out.clear();
    push_bp_rows(weak, vec![]);
    set_bp_status(weak, tr!("Fetching {} \u{2014} {}\u{2026}", bp_chart_label(kind), label));
    let (tx, kind) = (tx.clone(), kind.to_string());
    tokio::spawn(async move {
        let result = lightify_core::beatport::fetch_chart(&slug, &kind).await;
        let _ = tx.send(Cmd::BpFetched { seq: this, genre: idx, kind, result });
    });
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

/// What a failed token refresh means at the sign-in check: the notice to sign in again
/// with, or `None` when the sign-in itself still stands (offline, or Spotify's token
/// service failing) and the app should start on what it has and keep retrying.
fn refresh_failure_notice(e: &str) -> Option<String> {
    lightify_core::spotify::is_token_revoked(e)
        .then(|| tr!("Your Spotify sign-in has expired or was revoked \u{2014} authorise again. ({})", e))
}

/// A refresh token Spotify refused mid-session (`invalid_grant`), noted wherever a
/// request saw it and acted on at the top of the worker loop.
static SIGN_IN_REVOKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn note_revoked_sign_in() {
    SIGN_IN_REVOKED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn take_revoked_sign_in() -> bool {
    SIGN_IN_REVOKED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// The worker while the sign-in page waits. Local mode needs no Spotify session, so its
/// commands (and the transport keys while it owns them) are handled here as usual; the
/// engine's news is held and handed back to the worker loop once there is a session.
/// The page stays out of the way while Local mode is on.
///
/// A sign-in revoked mid-session lands here while our engine plays on: Play/Pause/Next
/// (buttons, media keys) still reach it, and a language switch re-says the page.
struct SignInWait<'a> {
    rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    tx: &'a tokio::sync::mpsc::UnboundedSender<Cmd>,
    local: &'a mut local_ui::LocalState,
    local_on: bool,
    held: Vec<Cmd>,
    /// What the page says (how to say it, so a language switch can say it again), so a
    /// Local mode switch can show or hide it unchanged.
    page: (PageLine, SignInTone, bool),
    /// Our engine's last play state as its held events told it (`None`: no news).
    engine_playing: Option<bool>,
}

/// A line on the sign-in page, kept as how to say it (see `SignInWait::page`).
type PageLine = std::sync::Arc<dyn Fn() -> String + Send + Sync>;

/// A page line that is already final text (an error from Spotify, nothing to translate).
fn page_text(text: String) -> PageLine {
    std::sync::Arc::new(move || text.clone())
}

impl<'a> SignInWait<'a> {
    fn new(
        rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
        tx: &'a tokio::sync::mpsc::UnboundedSender<Cmd>,
        local: &'a mut local_ui::LocalState,
    ) -> Self {
        Self {
            rx,
            tx,
            local,
            local_on: local_ui::load_mode(),
            held: Vec::new(),
            page: (page_text(String::new()), SignInTone::Info, false),
            engine_playing: None,
        }
    }

    fn show(&mut self, weak: &slint::Weak<MainWindow>, status: PageLine, tone: SignInTone, waiting: bool) {
        let text = status();
        self.page = (status, tone, waiting);
        push_signin(weak, !self.local_on, text, tone, waiting);
    }

    /// Hold an engine event for the worker loop. Only the latest position and volume
    /// matter, so those replace the one held before: every event used to be kept
    /// (~7,000 an hour at the 500 ms position tick) and replayed in one burst.
    fn hold(&mut self, c: Cmd) {
        if let Cmd::Engine(engine::Event::Playing { playing }) = &c {
            self.engine_playing = Some(*playing);
        }
        let kind = |c: &Cmd| match c {
            Cmd::Engine(engine::Event::Position { .. }) => 1,
            Cmd::Engine(engine::Event::Volume { .. }) => 2,
            Cmd::Relabel => 3,
            _ => 0,
        };
        let k = kind(&c);
        if k != 0 {
            self.held.retain(|h| kind(h) != k);
        }
        self.held.push(c);
    }

    /// The next command for the sign-in page. `None` when the app is closing.
    async fn recv(&mut self, weak: &slint::Weak<MainWindow>) -> Option<Cmd> {
        loop {
            let cmd = unstamp(self.rx.recv().await?);
            if let Cmd::Local(local_ui::LocalCmd::ModeChanged(on)) = &cmd {
                self.local_on = *on;
                let (status, tone, waiting) = self.page.clone();
                push_signin(weak, !self.local_on, status(), tone, waiting);
            }
            match route_local(cmd, self.local, weak, self.tx) {
                None => {}
                // Engine news, and a chart's background results (dropping one would
                // leave its refill marked pending for good, or the chart list blank).
                Some(c @ (Cmd::Engine(_) | Cmd::BpRefillDone { .. } | Cmd::BpMatched { .. } | Cmd::BpFetched { .. })) => {
                    self.hold(c)
                }
                // The language changed: the page's own lines now, the rest of the app's
                // Rust-made text once the worker loop is back (held for it).
                Some(Cmd::Relabel) => {
                    let (status, tone, waiting) = self.page.clone();
                    push_signin(weak, !self.local_on, status(), tone, waiting);
                    set_status(weak, tr!("Sign in to Spotify to start"));
                    self.hold(Cmd::Relabel);
                }
                // Our engine plays on while the page waits (a mid-session re-sign-in):
                // the transport still reaches it, locally, with no Web API needed.
                Some(c @ (Cmd::TogglePlay | Cmd::Next | Cmd::Prev)) if engine::running() => {
                    if let Some(line) = signin_transport(&c, self.engine_playing) {
                        engine::send(line);
                    }
                }
                Some(c) => return Some(c),
            }
        }
    }

    /// Signed in: the held engine events go back to the worker loop, in order.
    fn release_held(&mut self) {
        for c in self.held.drain(..) {
            let _ = self.tx.send(c);
        }
    }
}

/// The engine command for a transport press made while the sign-in page waits.
/// `playing`: the engine's last report (unknown: a toggle plays).
fn signin_transport(cmd: &Cmd, playing: Option<bool>) -> Option<&'static str> {
    match cmd {
        Cmd::TogglePlay if playing == Some(true) => Some(r#"{"cmd":"pause"}"#),
        Cmd::TogglePlay => Some(r#"{"cmd":"play"}"#),
        Cmd::Next => Some(r#"{"cmd":"next"}"#),
        Cmd::Prev => Some(r#"{"cmd":"prev"}"#),
        _ => None,
    }
}

/// The commands Local mode handles itself. `None` when `cmd` was one of them.
fn route_local(
    cmd: Cmd,
    local: &mut local_ui::LocalState,
    weak: &slint::Weak<MainWindow>,
    tx: &tokio::sync::mpsc::UnboundedSender<Cmd>,
) -> Option<Cmd> {
    let c = match cmd {
        Cmd::Local(c) => c,
        Cmd::PlaylistNamed(n) if local_ui::wants_name(local) => local_ui::LocalCmd::Named(Some(n)),
        Cmd::PlaylistNameCancelled if local_ui::wants_name(local) => local_ui::LocalCmd::Named(None),
        // Media keys / tray / palette transport drive local playback while it is the one
        // going (or paused in Local mode).
        Cmd::TogglePlay if local_ui::owns_transport(local) => local_ui::LocalCmd::TogglePlay,
        Cmd::Next if local_ui::owns_transport(local) => local_ui::LocalCmd::Next,
        Cmd::Prev if local_ui::owns_transport(local) => local_ui::LocalCmd::Prev,
        other => return Some(other),
    };
    local_ui::handle(local, c, weak, tx);
    None
}

/// Return a signed-in `Session`, showing the sign-in page for as long as it takes.
/// `None` only when the app is closing. Only a refresh token Spotify refused needs the
/// page: offline, or with Spotify's token service failing, the saved session comes
/// back as it is and every later request retries the refresh.
async fn obtain_session(weak: &slint::Weak<MainWindow>, wait: &mut SignInWait<'_>) -> Option<Session> {
    let mut notice: Option<(PageLine, SignInTone)> = None;
    loop {
        if let Ok(mut s) = Session::load() {
            let fresh = s.ensure_fresh().await;
            match fresh.as_ref().err().filter(|e| refresh_failure_notice(e).is_some()) {
                None => {
                    if let Err(e) = fresh {
                        eprintln!("[lightify] starting without a fresh token: {e}");
                    }
                    push_signin(weak, false, String::new(), SignInTone::Info, false);
                    wait.release_held();
                    return Some(s);
                }
                Some(e) => {
                    let e = e.clone();
                    let say: PageLine = std::sync::Arc::new(move || refresh_failure_notice(&e).unwrap_or_default());
                    notice = Some((say, SignInTone::Error));
                }
            }
        }
        let saved = lightify_core::auth::saved_client_id();
        let _ = weak.upgrade_in_event_loop(move |app| {
            if app.get_signin_client_id().is_empty() {
                app.set_signin_client_id(saved.into());
            }
        });
        let (msg, tone) = notice.take().unwrap_or((page_text(String::new()), SignInTone::Info));
        wait.show(weak, msg, tone, false);
        set_status(weak, tr!("Sign in to Spotify to start"));

        // Wait for the user to authorise. Spotify commands are ignored until then —
        // there's nothing they could act on without a session.
        loop {
            let raw = match wait.recv(weak).await? {
                Cmd::SignInAuthorise(cid) | Cmd::SignInRetry(cid) => cid,
                Cmd::SignInOpenDashboard => {
                    open_url(lightify_core::auth::DASHBOARD_URL);
                    continue;
                }
                _ => continue,
            };
            match sign_in(weak, wait, &raw).await {
                Ok(Some(name)) => {
                    wait.show(weak, std::sync::Arc::new(move || tr!("Signed in as {}", name)), SignInTone::Ok, false);
                    break; // back to the top: load the session that was just saved
                }
                Ok(None) => return None, // closing
                Err(e) => wait.show(weak, e, SignInTone::Error, false),
            }
        }
    }
}

/// One browser sign-in. `Ok(None)` means the app is closing. While it waits, Retry
/// re-opens the browser on the same pending request (the shipped app's
/// `cmd_retry_auth_browser`) — or, when the Client ID was edited since, starts over with
/// the new one (it used to re-open the old URL for up to 5 minutes) — and the dashboard
/// link still works. The error is a page line (`PageLine`), said again on a language
/// switch.
async fn sign_in(
    weak: &slint::Weak<MainWindow>,
    wait: &mut SignInWait<'_>,
    raw: &str,
) -> Result<Option<String>, PageLine> {
    use lightify_core::auth;
    let mut raw = raw.to_string();
    'flow: loop {
        let cid = auth::normalize_client_id(&raw).map_err(page_text)?;
        let callback = auth::bind_callback().await.map_err(page_text)?;
        let pkce = auth::PkceAuth::new(&cid);
        let url = pkce.auth_url();
        if !open_url(&url) {
            return Err(std::sync::Arc::new(|| tr!("Couldn't open your web browser to sign in.")));
        }
        wait.show(weak, std::sync::Arc::new(|| tr!("Opening browser \u{2014} complete the login\u{2026}")), SignInTone::Info, true);
        let finish = pkce.finish(callback);
        tokio::pin!(finish);
        let deadline = tokio::time::sleep(SIGNIN_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                r = &mut finish => return r.map(Some).map_err(page_text),
                _ = &mut deadline => {
                    return Err(std::sync::Arc::new(|| tr!("Timed out waiting for Spotify \u{2014} click Authorise to try again.")));
                }
                cmd = wait.recv(weak) => match cmd {
                    None => return Ok(None),
                    Some(Cmd::SignInRetry(typed) | Cmd::SignInAuthorise(typed)) => {
                        if signin_restarts(&cid, &typed) {
                            // Another Client ID: this request can never finish for it.
                            raw = typed;
                            continue 'flow;
                        }
                        open_url(&url);
                        wait.show(weak, std::sync::Arc::new(|| tr!("Re-opening browser \u{2014} complete the login\u{2026}")), SignInTone::Info, true);
                    }
                    Some(Cmd::SignInOpenDashboard) => {
                        open_url(auth::DASHBOARD_URL);
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

/// While a sign-in waits for Client ID `pending`, does a Retry/Authorise with `typed`
/// start a new one? Only for a different, valid ID (a typo mid-edit keeps the request).
fn signin_restarts(pending: &str, typed: &str) -> bool {
    lightify_core::auth::normalize_client_id(typed).is_ok_and(|c| c != pending)
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
    use slint::winit_030::winit::dpi::{LogicalSize, PhysicalPosition};
    use slint::winit_030::winit::window::WindowLevel;
    use slint::winit_030::WinitWindowAccessor;

    let was_off = app.get_mini_mode() == "off";
    // Already the full window ("Exit mini player" from the palette with no mini player
    // up): nothing to do. Going through the exit below un-maximized a maximized window
    // and maximized it again 120 ms later — a visible flicker.
    if was_off && mode == "off" {
        return;
    }
    if was_off && mode != "off" {
        // Record the full window as it is right now (a move/resize capture may still be
        // pending), BEFORE the property flips and the window shrinks.
        app.window().with_winit_window(|w| window_geom::capture(w, false));
    }
    // Set the property first so the bound min-width/min-height relax before we resize.
    app.set_mini_mode(mode.into());
    if mode != "off" {
        app.set_mini_last(mode.into());
        save_mini_mode(mode);
    }
    let g = window_geom::get();

    let mut unmaximizing = false;
    app.window().with_winit_window(|w| {
        if mode == "off" {
            // Drop always-on-top and the mini size lock FIRST: a window whose min/max is
            // still the mini size silently clamps the restore.
            let _ = w.set_window_level(WindowLevel::Normal);
            w.set_resizable(true);
            w.set_max_inner_size(None::<LogicalSize<f64>>);
            w.set_min_inner_size(Some(LogicalSize::new(window_geom::MIN_W, window_geom::MIN_H)));
            if w.is_maximized() {
                // Only reachable if something maximized the mini player anyway. The
                // un-maximize completes asynchronously and restores the MINI size last,
                // over any size requested now — so the full rect goes back once it has.
                w.set_maximized(false);
                unmaximizing = true;
            } else {
                restore_full_window(w, &g);
            }
        } else {
            let (mw, mh) = mini_size(mode);
            w.set_maximized(false);
            // The mini layouts are fixed sizes: min AND max, so the edges don't resize
            // it — and not resizable, so Windows offers no maximize (Win+Up) either.
            w.set_min_inner_size(Some(LogicalSize::new(mw, mh)));
            w.set_max_inner_size(Some(LogicalSize::new(mw, mh)));
            w.set_resizable(false);
            let _ = w.request_inner_size(LogicalSize::new(mw, mh));
            // Back where the mini player was last time (switching between mini layouts
            // keeps it where it is).
            if was_off {
                if let Some((x, y)) = g.mini_pos.filter(|&(x, y)| window_geom::on_screen(x, y)) {
                    w.set_outer_position(PhysicalPosition::new(x, y));
                }
            }
            let _ = w.set_window_level(WindowLevel::AlwaysOnTop);
        }
    });
    if unmaximizing {
        let weak = app.as_weak();
        slint::Timer::single_shot(Duration::from_millis(120), move || {
            if let Some(a) = weak.upgrade() {
                a.window().with_winit_window(|w| restore_full_window(w, &window_geom::get()));
            }
        });
    }
}

/// Put the full window back: its normal rect first, then maximized on top when it was
/// — so a window maximized before the mini player comes back maximized AND
/// un-maximizes to its own size, not the mini player's.
fn restore_full_window(w: &slint::winit_030::winit::window::Window, g: &window_geom::Geom) {
    use slint::winit_030::winit::dpi::{LogicalSize, PhysicalPosition};
    let normal = window_geom::restore_rect(g);
    let (rw, rh) = normal.map(|r| (r.w, r.h)).unwrap_or((window_geom::DEFAULT_W, window_geom::DEFAULT_H));
    let _ = w.request_inner_size(LogicalSize::new(rw, rh));
    // Position after the size, so the window manager doesn't re-clamp it against the
    // mini player's geometry; and only to a place that is still on a screen.
    if let Some(r) = normal.filter(|r| window_geom::on_screen(r.x, r.y)) {
        w.set_outer_position(PhysicalPosition::new(r.x, r.y));
    }
    if g.maximized {
        w.set_maximized(true);
    }
    window_geom::fit_next();
}

/// Apply the saved geometry before the window first shows.
fn restore_window_geometry(app: &MainWindow) {
    let g = window_geom::get();
    // On the monitor it was maximized on, when that is another one (`restore_rect`).
    if let Some(r) = window_geom::restore_rect(&g) {
        app.window().set_size(slint::LogicalSize::new(r.w as f32, r.h as f32));
        if window_geom::on_screen(r.x, r.y) {
            app.window().set_position(slint::PhysicalPosition::new(r.x, r.y));
        }
    }
    if g.maximized {
        app.window().set_maximized(true);
    }
    // Once it has settled on its monitor, inside that monitor's work area.
    window_geom::fit_next();
}

/// Record the window's settled state now and write it if it changed.
fn capture_window_geometry(app: &MainWindow) {
    use slint::winit_030::WinitWindowAccessor;
    let mini = app.get_mini_mode() != "off";
    app.window().with_winit_window(|w| window_geom::capture(w, mini));
    window_geom::save_if_changed();
}

/// Follow moves and resizes: captured once the window has been still for a moment
/// (a drag sends dozens of events; only where it ends matters), and on close.
fn track_window_geometry(app: &MainWindow) {
    use slint::winit_030::winit::event::WindowEvent;
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    const SETTLE: Duration = Duration::from_millis(400);
    use slint::winit_030::winit::event::{ElementState, KeyEvent};
    use slint::winit_030::winit::keyboard::{Key, KeyCode, PhysicalKey};
    let weak = app.as_weak();
    let settle = Rc::new(slint::Timer::default());
    // Ctrl / Alt / Win held, for the palette key below (winit reports them apart).
    let mods = std::cell::Cell::new(slint::winit_030::winit::keyboard::ModifiersState::empty());
    app.window().on_winit_window_event(move |_, ev| {
        match ev {
            WindowEvent::ModifiersChanged(m) => mods.set(m.state()),
            // Ctrl+K under a layout without Latin letters (Russian, Greek, …): Slint only
            // sees the layout's character there ("л"), so app.slint's `is-palette-key`
            // can't match it. Go by the physical key — but only then, so a Latin layout
            // that moves K elsewhere (Dvorak) keeps its own K.
            WindowEvent::KeyboardInput {
                event: KeyEvent { physical_key: PhysicalKey::Code(KeyCode::KeyK), logical_key: Key::Character(c), state: ElementState::Pressed, .. },
                ..
            } if (mods.get().control_key() || mods.get().super_key())
                && !mods.get().alt_key()
                && !c.chars().any(|ch| ch.is_ascii_alphabetic()) =>
            {
                if let Some(a) = weak.upgrade() {
                    a.invoke_toggle_palette();
                    return EventResult::PreventDefault;
                }
            }
            // A press while a Local length field holds the keyboard and the pointer is
            // off it: the field gives the keyboard up first, which commits what was
            // typed (U6). Clicking a chip, a row or a button never moves the focus in
            // Slint, so without this the value just sat there unapplied.
            WindowEvent::MouseInput { state: ElementState::Pressed, .. } => {
                if let Some(a) = weak.upgrade().filter(|a| a.get_local_len_away()) {
                    a.invoke_hand_back_focus();
                }
            }
            WindowEvent::Resized(_) | WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. } => {
                // The mini player is a fixed size; if the system maximized it anyway
                // (a snap gesture the non-resizable style didn't stop), undo it.
                if let Some(a) = weak.upgrade().filter(|a| a.get_mini_mode() != "off") {
                    a.window().with_winit_window(|w| {
                        if w.is_maximized() {
                            w.set_maximized(false);
                        }
                    });
                }
                let weak = weak.clone();
                settle.start(slint::TimerMode::SingleShot, SETTLE, move || {
                    if let Some(a) = weak.upgrade() {
                        // A restore just landed: pull it inside its monitor first.
                        let mini = a.get_mini_mode() != "off";
                        a.window().with_winit_window(|w| window_geom::fit_if_pending(w, mini));
                        capture_window_geometry(&a);
                    }
                });
            }
            WindowEvent::CloseRequested => {
                settle.stop();
                if let Some(a) = weak.upgrade() {
                    capture_window_geometry(&a);
                }
            }
            // Files dragged in from Explorer (the Local view takes them; elsewhere they
            // are ignored). winit sends one event per file, so they are gathered first.
            WindowEvent::HoveredFile(p) => local_ui::drag_event(local_ui::Drag::Hover(p.clone())),
            WindowEvent::HoveredFileCancelled => local_ui::drag_event(local_ui::Drag::Cancel),
            WindowEvent::DroppedFile(p) => local_ui::drag_event(local_ui::Drag::Drop(p.clone())),
            _ => {}
        }
        EventResult::Propagate
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

// ── Self-update ───────────────────────────────────────────────────────────────
//
// `lightify_core::update` does the work (signed manifest on lightify.stream, verified
// download into %LOCALAPPDATA%\Lightify\updates); this is the UI side: when to check,
// what the footer pill and Settings › updates show, and handing over to the installer's
// `--update` mode on Restart. Nothing installs without the user pressing Restart.

// ── Language (LANGUAGE-PLAN.md) ──────────────────────────────────────────────

// The bundled languages, generated by build.rs from translations/ (empty while English is
// the only one).
include!(concat!(env!("OUT_DIR"), "/languages.rs"));

/// Settings › language: `auto` (Windows' display language) unless a language was picked,
/// here or in the installer.
fn language_setting_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_lang.txt")
}

/// `--lang <code>` from the command line, when given (see `main`).
static LANG_ARG: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn load_language_setting() -> String {
    std::fs::read_to_string(language_setting_path()).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Makes `setting` the language of Slint's strings and the Rust side's alike, and returns
/// the code chosen. The app resolves `auto` itself and tells Slint explicitly, so the two
/// never disagree about which language Windows asked for. Must run after the first
/// window exists.
fn apply_language(setting: &str) -> String {
    let available: Vec<&str> = LANGS.iter().map(|l| l.code).collect();
    let code = lightify_i18n::resolve(setting, &available, &lightify_i18n::system_languages());
    let code = lightify_i18n::select(LANGS, &code);
    if !LANGS.is_empty() {
        let _ = slint::select_bundled_translation(if code == lightify_i18n::ENGLISH { "" } else { &code });
    }
    code
}

/// What each row of Settings › language saves: `auto`, English, then every bundled
/// language (by code, as build.rs lists them). Empty while English is the only one.
fn language_choices() -> Vec<String> {
    if LANGS.is_empty() {
        return Vec::new();
    }
    let mut v = vec!["auto".to_string(), lightify_i18n::ENGLISH.to_string()];
    v.extend(LANGS.iter().map(|l| l.code.to_string()));
    v
}

/// Settings › language's rows for the saved `setting`: Automatic first, then each
/// language in its own name — "(preview)" after a machine draft no native speaker has
/// read yet (`lightify_i18n::PREVIEW`).
fn language_rows(setting: &str) -> Vec<DeviceRow> {
    let s = setting.trim();
    let auto = s.is_empty() || s.eq_ignore_ascii_case("auto");
    language_choices()
        .iter()
        .map(|c| {
            let name = if c == "auto" {
                tr!("Automatic (Windows language)")
            } else if lightify_i18n::is_preview(c) {
                tr!("{} (preview)", lightify_i18n::native_name(c))
            } else {
                lightify_i18n::native_name(c)
            };
            let active = if c == "auto" { auto } else { !auto && c.eq_ignore_ascii_case(s) };
            DeviceRow { name: name.into(), active }
        })
        .collect()
}

/// The language this run shows: `--lang` when given, else the saved choice.
fn language_in_use() -> String {
    LANG_ARG.get().cloned().unwrap_or_else(load_language_setting)
}

fn push_languages(app: &MainWindow, setting: &str) {
    let rows = language_rows(setting);
    // On the button: the row that is ticked (an unbundled saved code ticks none: English).
    let current = rows.iter().find(|r| r.active).map(|r| r.name.clone());
    app.set_settings_language_current(current.unwrap_or_else(|| lightify_i18n::native_name(lightify_i18n::ENGLISH).into()));
    app.set_settings_languages(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
}

/// Settings › language picked (`setting` = `auto` or a code): save it and switch live.
/// Slint re-says its own strings by itself (`select_bundled_translation` marks them
/// dirty); what Rust put on screen is said again here — the UI thread's part — and by
/// the worker (`Cmd::Relabel`: library rows, search, the sidebar, Local, prompts).
fn switch_language(app: &MainWindow, setting: &str, tx: &tokio::sync::mpsc::UnboundedSender<Cmd>) {
    let _ = std::fs::write(language_setting_path(), setting);
    apply_switch(app, setting, tx);
}

/// The switch itself, without saving (`--selftest-language` never touches the setting).
fn apply_switch(app: &MainWindow, setting: &str, tx: &tokio::sync::mpsc::UnboundedSender<Cmd>) {
    apply_language(setting);
    relabel_ui(app, setting);
    let _ = tx.send(Cmd::Relabel);
}

/// The UI thread's half of a language switch: the language list itself, the tray menu,
/// and no menu, palette or status line left over in the old language.
fn relabel_ui(app: &MainWindow, setting: &str) {
    push_languages(app, setting);
    app.set_ctx_open(false);
    app.set_palette_open(false);
    app.set_status_text("".into());
    #[cfg(windows)]
    tray::relabel();
    #[cfg(windows)]
    taskbar::relabel();
}

/// Chinese text: Slint's fallback list for Han characters (fontique's Windows default)
/// lacks glyphs some common ones need — 设 and 队 drew blank in the Phase 4 font test.
/// Microsoft YaHei UI (on every Windows since 7) goes after that list, so whatever it
/// already finds is unchanged. The list is process-wide; once, after the first window.
fn cjk_font_fallback() {
    use slint::fontique_010::fontique::{FallbackKey, Script};
    let mut c = slint::fontique_010::shared_collection();
    let hani = FallbackKey::new(Script::from_str_unchecked("Hani"), None);
    let Some(yahei) = c.family_id("Microsoft YaHei UI") else { return };
    let current: Vec<_> = c.fallback_families(hani).collect();
    if current.contains(&yahei) {
        return;
    }
    // `append_fallbacks` replaced the default list in testing; set the whole list.
    c.set_fallbacks(hani, current.into_iter().chain(std::iter::once(yahei)));
}

/// "Install updates automatically" (Settings › updates), on unless switched off.
fn update_auto_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_autoupdate.txt")
}

fn load_update_auto() -> bool {
    std::fs::read_to_string(update_auto_path()).map(|s| s.trim() != "off").unwrap_or(true)
}

fn save_update_auto(on: bool) {
    let _ = std::fs::write(update_auto_path(), if on { "on" } else { "off" });
}

/// The version the user pressed "Mark seen" on: the settings cog stops pulsing for
/// it, and it isn't downloaded automatically (a newer one is, and pulses again).
fn update_seen_path() -> std::path::PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_update_seen.txt")
}

fn load_update_seen() -> String {
    std::fs::read_to_string(update_seen_path()).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn install_updater(app: &MainWindow) {
    app.set_update_auto(load_update_auto());
    app.set_update_seen_version(load_update_seen().into());
    {
        let weak = app.as_weak();
        app.on_update_mark_seen(move || {
            if let Some(a) = weak.upgrade() {
                let v = a.get_update_version().to_string();
                if !v.is_empty() {
                    let _ = std::fs::write(update_seen_path(), &v);
                    a.set_update_seen_version(v.into());
                }
            }
        });
    }
    // A verified installer left over from last session is offered straight away.
    // `pending` re-hashes the file (~100 MB), so never on the UI thread.
    {
        let weak = app.as_weak();
        std::thread::spawn(move || {
            if let Some((ver, _)) = lightify_core::update::pending(env!("CARGO_PKG_VERSION")) {
                let _ = weak.upgrade_in_event_loop(move |a| {
                    if a.get_update_state() != "ready" && a.get_update_state() != "installing" {
                        a.set_update_version(ver.into());
                        a.set_update_state("ready".into());
                    }
                });
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_update_check(move || spawn_update_check(weak.clone(), true));
    }
    {
        let weak = app.as_weak();
        app.on_set_update_auto(move |on| {
            save_update_auto(on);
            if on {
                spawn_update_check(weak.clone(), false);
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_update_restart(move || {
            // Latched: a second click (or the footer pill and Settings both) while the
            // update hands over would start a second installer.
            let Some(a) = weak.upgrade() else { return };
            if a.get_update_state() != "ready" {
                return;
            }
            a.set_update_state("installing".into());
            // Re-verify off the UI thread (`pending` re-hashes), then hand over.
            let weak = weak.clone();
            std::thread::spawn(move || {
                let Some((_, installer)) = lightify_core::update::pending(env!("CARGO_PKG_VERSION")) else {
                    let _ = weak.upgrade_in_event_loop(|a| {
                        a.set_update_state("error".into());
                        a.set_update_note(tr!("The downloaded update is gone \u{2014} check again").into());
                    });
                    return;
                };
                match lightify_core::update::launch_update(&installer) {
                    // The installer waits for this process to exit, then swaps the
                    // files and relaunches; leave the same way the close button does.
                    Ok(()) => {
                        let _ = slint::invoke_from_event_loop(|| {
                            let _ = slint::quit_event_loop();
                        });
                    }
                    Err(e) => {
                        let _ = weak.upgrade_in_event_loop(move |a| {
                            a.set_update_state("error".into());
                            a.set_update_note(tr!("Couldn't start the update: {}", e).into());
                        });
                    }
                }
            });
        });
    }
    // First check shortly after launch (never in the way of startup), then every 6h.
    let weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_secs(8), move || {
        spawn_update_check(weak.clone(), false);
    });
    let weak = app.as_weak();
    let every = slint::Timer::default();
    every.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(6 * 3600), move || {
        spawn_update_check(weak.clone(), false);
    });
    // The timer must outlive this function; it lives as long as the app.
    std::mem::forget(every);
}

/// Check (and, if newer, download) on a worker thread. `manual` = the user pressed
/// "Check now" / "Download": it downloads whatever it finds, and reports failures.
/// A background check always looks (so a new version can be shown even with
/// automatic updates off) but only downloads when they're on and the version hasn't
/// been marked seen; otherwise it reports the version as "available".
fn spawn_update_check(weak: slint::Weak<MainWindow>, manual: bool) {
    let Some(app) = weak.upgrade() else { return };
    let state = app.get_update_state().to_string();
    if state == "checking" || state == "downloading" || state == "ready" || state == "installing" {
        return;
    }
    let auto = app.get_update_auto();
    let seen = app.get_update_seen_version().to_string();
    // A version already on offer stays on offer if a quiet re-check fails.
    let known = (state == "available").then(|| app.get_update_version().to_string());
    app.set_update_state("checking".into());
    std::thread::spawn(move || {
        let set = |weak: &slint::Weak<MainWindow>, state: &'static str, ver: String, note: String| {
            let _ = weak.upgrade_in_event_loop(move |a| {
                // A downloaded update found meanwhile (last session's, re-verified at
                // launch) is offered as "Restart": this check's result must not
                // replace that offer with "up to date" or "available".
                if !update_result_applies(&a.get_update_state(), state) {
                    return;
                }
                a.set_update_state(state.into());
                a.set_update_version(ver.into());
                a.set_update_note(note.into());
            });
        };
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            set(&weak, "error", String::new(), tr!("Couldn't start the update check"));
            return;
        };
        let current = env!("CARGO_PKG_VERSION");
        match rt.block_on(lightify_core::update::check(current)) {
            Ok(None) => set(&weak, "current", String::new(), String::new()),
            Ok(Some(rel)) if !manual && (!auto || rel.version == seen) => {
                set(&weak, "available", rel.version.clone(), String::new());
            }
            Ok(Some(rel)) => {
                set(&weak, "downloading", rel.version.clone(), String::new());
                let pw = weak.clone();
                let last = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
                let progress = move |done: u64, total: u64| {
                    if total == 0 {
                        return;
                    }
                    let pct = (done.saturating_mul(100) / total) as u32;
                    if last.swap(pct, std::sync::atomic::Ordering::Relaxed) != pct {
                        let _ = pw.upgrade_in_event_loop(move |a| a.set_update_progress(pct as f32 / 100.0));
                    }
                };
                match rt.block_on(lightify_core::update::download(&rel, progress)) {
                    Ok(_) => set(&weak, "ready", rel.version.clone(), String::new()),
                    Err(e) => set(&weak, "error", rel.version.clone(), tr!("Update failed: {}", e)),
                }
            }
            Err(e) if manual => set(&weak, "error", String::new(), tr!("Couldn't check for updates: {}", e)),
            // A quiet background check that failed (offline, server down) goes back to
            // what it knew: a version on offer stays, otherwise idle. It tries again later.
            Err(_) => match known {
                Some(v) => set(&weak, "available", v, String::new()),
                None => set(&weak, "idle", String::new(), String::new()),
            },
        }
    });
}

/// May an update check's `result` state replace `current`? Not a ready or installing
/// update, unless the check itself just made one ready.
fn update_result_applies(current: &str, result: &str) -> bool {
    !matches!(current, "ready" | "installing") || result == "ready"
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
    /// How many leading `upcoming` entries were queued by hand; the rest were the
    /// playing context's own next tracks. `None` (older files, stations): all of them.
    #[serde(default)]
    queued: Option<usize>,
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
    save_queue_marked(current_uri, upcoming, None);
}

/// `save_queue`, recording how many leading entries were hand-queued (see
/// `PersistedQueue::queued`).
fn save_queue_marked(current_uri: &str, upcoming: &[String], queued: Option<usize>) {
    if current_uri.is_empty() || upcoming.is_empty() {
        let _ = std::fs::remove_file(queue_path());
        return;
    }
    let keep = &upcoming[..upcoming.len().min(QUEUE_KEEP_MAX)];
    let pq = PersistedQueue { current_uri: current_uri.to_string(), upcoming: keep.to_vec(), queued };
    if let Ok(json) = serde_json::to_string(&pq) {
        let _ = lightify_core::config::write_atomic(&queue_path(), json.as_bytes());
    }
}

/// `save_queue`, but reading straight from the live mirror — every call site below
/// looks like this, so this is what they actually call.
fn persist_mirror(mirror: &Option<QueueMirror>) {
    match mirror {
        Some(m) if !m.upcoming.is_empty() => {
            save_queue_marked(
                &m.current_uri,
                &m.upcoming.iter().map(|t| t.uri.clone()).collect::<Vec<_>>(),
                Some(m.queued),
            );
        }
        _ => save_queue("", &[]),
    }
}

fn load_queue() -> Option<PersistedQueue> {
    serde_json::from_str(&std::fs::read_to_string(queue_path()).ok()?).ok()
}

/// What came after `track_uri` before the engine restarted, to hand to the new one:
/// the live mirror when it is still about this song, else the queue saved to disk (a
/// station clears the mirror, so its songs are only there).
fn restart_up_next(track_uri: &str, mirror: Option<&QueueMirror>, saved: Option<&PersistedQueue>) -> Vec<String> {
    if let Some(m) = mirror.filter(|m| m.current_uri == track_uri && !m.upcoming.is_empty()) {
        return m.upcoming.iter().map(|t| t.uri.clone()).collect();
    }
    saved
        .and_then(|pq| queue_restore_point(pq, track_uri))
        .map(<[String]>::to_vec)
        .unwrap_or_default()
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
    saved: Option<PersistedQueue>,
) {
    let Some(pq) = saved else { return };
    let playing_uri = last.as_ref().and_then(|p| p.track.as_ref()).map(|t| t.uri.clone()).unwrap_or_default();
    let context = last.as_ref().and_then(|p| p.context_uri.as_deref());
    let remaining = queue_to_restore(&pq, &playing_uri, context);
    if remaining.is_empty() {
        return;
    }
    let mut queued = 0usize;
    for uri in remaining.iter().take(QUEUE_KEEP_MAX) {
        match queue_one(session, uri, engine_owns_now()).await {
            Ok(()) => queued += 1,
            Err(_) => break,
        }
    }
    if queued == 0 {
        return;
    }
    if let Ok(t) = session.queue().await {
        if !t.is_empty() {
            set_status(weak, lightify_i18n::trn!("Restored queue \u{2014} {n} track", "Restored queue \u{2014} {n} tracks", t.len()));
            *mirror = Some(restored_mirror(playing_uri, t, queued));
        }
    }
}

/// What of a persisted queue to queue again behind `playing_uri`. Inside a playlist,
/// album or Liked Songs the context already carries its own next tracks; queueing the
/// remembered copies of them as well played each of them twice, so there only what was
/// queued by hand goes back.
fn queue_to_restore<'a>(pq: &'a PersistedQueue, playing_uri: &str, context: Option<&str>) -> &'a [String] {
    let Some(remaining) = queue_restore_point(pq, playing_uri) else { return &[] };
    let in_context = context.is_some_and(|c| {
        c.starts_with("spotify:playlist:") || c.starts_with("spotify:album:") || is_liked_context(c)
    });
    match pq.queued {
        Some(q) if in_context => {
            // `remaining` is a suffix of `upcoming`; keep only its hand-queued head.
            let skipped = pq.upcoming.len() - remaining.len();
            &remaining[..q.saturating_sub(skipped).min(remaining.len())]
        }
        _ => remaining,
    }
}

/// The QUEUE mirror after a restore: Spotify's own (~20-item) read, which puts the
/// `restored` hand-queued tracks first. It is a window onto the context, so it tops up
/// as it runs low, and new additions land after the restored ones.
fn restored_mirror(playing_uri: String, spotify_queue: Vec<Track>, restored: usize) -> QueueMirror {
    let mut m = QueueMirror::from_spotify(playing_uri, spotify_queue);
    m.queued = restored.min(m.upcoming.len());
    m
}

// ── Settings marshaling ──────────────────────────────────────────────────────

fn set_account(weak: &slint::Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |app| app.set_account(text.into()));
}

/// Settings › playback device's note while the list is empty; `say` builds it, like
/// `push_engine_status`.
fn set_device_status(weak: &slint::Weak<MainWindow>, say: impl Fn() -> String + Send + 'static) {
    let text = say();
    if let Ok(mut g) = DEVICE_LINE.lock() {
        *g = Some(Box::new(say));
    }
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
    // Shown in the active language; searched in it and in English (the English title
    // joins the English keywords), so a query typed either way finds the command.
    // (In English the title is already searched, so nothing is added.)
    let pc = |title: &'static str, hint: &'static str, keywords: &str, cmd: Cmd| {
        let shown = lightify_i18n::translate(title);
        let keywords = if shown == title { keywords.to_string() } else { format!("{} {keywords}", title.to_lowercase()) };
        PaletteCmd { title: shown, hint: lightify_i18n::translate(hint), keywords, cmd }
    };
    // `tr_noop!` puts each English title and hint in the template; `pc` translates it.
    let mut v = vec![
        pc(if playing { lightify_i18n::tr_noop!("Pause") } else { lightify_i18n::tr_noop!("Play") }, lightify_i18n::tr_noop!("Playback"), "toggle playback resume", Cmd::TogglePlay),
        pc(lightify_i18n::tr_noop!("Next track"), lightify_i18n::tr_noop!("Playback"), "skip forward", Cmd::Next),
        pc(lightify_i18n::tr_noop!("Previous track"), lightify_i18n::tr_noop!("Playback"), "back rewind", Cmd::Prev),
        pc(lightify_i18n::tr_noop!("Toggle shuffle"), lightify_i18n::tr_noop!("Playback"), "random", Cmd::ToggleShuffle),
        pc(lightify_i18n::tr_noop!("Cycle repeat"), lightify_i18n::tr_noop!("Playback"), "loop", Cmd::CycleRepeat),
        pc(lightify_i18n::tr_noop!("Open settings"), lightify_i18n::tr_noop!("App"), "preferences config devices poll rate", Cmd::OpenSettings),
        pc(lightify_i18n::tr_noop!("Show queue"), lightify_i18n::tr_noop!("Panel"), "up next sidebar", Cmd::ToggleSidebar(1)),
        pc(lightify_i18n::tr_noop!("Show recent"), lightify_i18n::tr_noop!("Panel"), "history sidebar", Cmd::ToggleSidebar(2)),
        pc(lightify_i18n::tr_noop!("Collapse left panel"), lightify_i18n::tr_noop!("Window"), "hide library toggle", Cmd::CollapseLeft),
        pc(lightify_i18n::tr_noop!("Mini player: square"), lightify_i18n::tr_noop!("Window"), "compact miniplayer cover", Cmd::SetMiniMode("square".into())),
        pc(lightify_i18n::tr_noop!("Mini player: bar"), lightify_i18n::tr_noop!("Window"), "compact miniplayer strip", Cmd::SetMiniMode("bar".into())),
        pc(lightify_i18n::tr_noop!("Mini player: nano (cover only)"), lightify_i18n::tr_noop!("Window"), "compact miniplayer cover tiny", Cmd::SetMiniMode("nano".into())),
        pc(lightify_i18n::tr_noop!("Exit mini player"), lightify_i18n::tr_noop!("Window"), "full restore miniplayer off", Cmd::SetMiniMode("off".into())),
        pc(lightify_i18n::tr_noop!("Liked Songs"), lightify_i18n::tr_noop!("Library"), "favorites hearts saved", Cmd::Drill(0)),
    ];
    // Every playlist, in the library's sort order — also ones a type-to-filter over the
    // library is hiding (the palette has its own search). Opened by id (`DrillId`): a
    // display row would point into the filtered list.
    for &idx in order {
        let Some(p) = playlists.get(idx) else { continue };
        v.push(PaletteCmd {
            title: p.name.clone(),
            hint: tr!("Playlist"),
            keywords: format!("playlist {}", p.owner),
            cmd: Cmd::DrillId(p.id.clone()),
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
                title: tr!("Search for \u{201c}{}\u{201d}", q),
                hint: tr!("Search"),
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
    /// An artist's discography (search drill-in only), paged by release, not track.
    Artist(String),
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
            PagerSource::Artist(_) => Session::ARTIST_RELEASE_PAGE,
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
    paged: &std::collections::HashMap<&'static str, (u32, bool)>,
) {
    let kind = search_kind(filter);
    let shown = rows.iter().filter(|r| !kind.is_empty() && r.kind == kind).count() as u32;
    // The initial search already consumed one page's worth of *raw* items, so the
    // next offset is one page on — not the number that happened to parse (Spotify
    // puts `null`s in playlist search results, so those differ). The All view never
    // pages; each single-type filter does.
    (*offset, *done) = resumed_search_pager(paged, kind, (Session::SEARCH_PAGE, kind.is_empty() || shown == 0));
    let more = !*done;
    let _ = weak.upgrade_in_event_loop(move |app| app.set_search_more(more));
}

/// A filter already paged for these results carries on where it stopped: its pages are
/// in the shared results, and starting again from the first page added them twice.
fn resumed_search_pager(
    paged: &std::collections::HashMap<&'static str, (u32, bool)>,
    kind: &str,
    fresh: (u32, bool),
) -> (u32, bool) {
    if kind.is_empty() {
        return fresh;
    }
    paged.get(kind).copied().unwrap_or(fresh)
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

/// A library row: a single click drills in at once (`drill`, the row as shown); a
/// double-click also plays the playlist (`play`, its row as shown by then). The drill
/// removes the row before the second press, which lands on app.slint's `dbl-armed`
/// catcher (`open-row-double`) instead. The row is remembered by id, not index: the
/// drill drops a filter typed over the library, which re-numbers the rows. Shared by
/// the app and `--selftest-menus`, so the test drives the real wiring.
fn wire_open_row(app: &MainWindow, drill: impl Fn(usize) + 'static, play: impl Fn(usize) + 'static) {
    let opened: Rc<std::cell::RefCell<Option<slint::SharedString>>> = Rc::default();
    {
        let opened = opened.clone();
        let weak = app.as_weak();
        app.on_open_row(move |i| {
            let i = i.max(0) as usize;
            *opened.borrow_mut() = weak.upgrade().and_then(|a| a.get_playlists().row_data(i)).map(|r| r.id);
            drill(i);
        });
    }
    let weak = app.as_weak();
    app.on_open_row_double(move || {
        let Some(id) = opened.borrow_mut().take() else { return };
        if let Some(j) = weak.upgrade().and_then(|a| a.get_playlists().iter().position(|r| r.id == id)) {
            play(j);
        }
    });
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
            if list_version(surface) == version {
                unstamp(*inner)
            } else if matches!(*inner, Cmd::SelectDragTo { .. }) {
                // A drag over a list that changed under it just ends; reporting it as a
                // stale click re-posted the notice ~30 times a second while the mouse moved.
                Cmd::SelectDragEnd
            } else {
                Cmd::StaleClick
            }
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

/// `drop_selection` for the left track list, which needs the view to un-highlight its
/// rows (they are addressed through the sort/filter view).
fn drop_track_selection(
    weak: &slint::Weak<MainWindow>,
    selected: &mut std::collections::BTreeSet<usize>,
    sel_anchor: &mut Option<usize>,
    sel_scope: &mut i32,
    view: &[usize],
) {
    if *sel_scope != SEL_TRACKS {
        return;
    }
    selected.clear();
    *sel_anchor = None;
    *sel_scope = SEL_NONE;
    push_selection(weak, SEL_TRACKS, selected, view);
}

/// The left track list's rows were rebuilt (a re-sort, a new page) and came back with
/// no marks. Its selection is keyed to tracks, so it still stands: show it again.
fn remark_tracks(
    weak: &slint::Weak<MainWindow>,
    sel_scope: i32,
    selected: &std::collections::BTreeSet<usize>,
    view: &[usize],
) {
    if sel_scope == SEL_TRACKS {
        push_selection(weak, SEL_TRACKS, selected, view);
    }
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
    drop_selection(weak, SEL_SEARCH_DRILL, selected, sel_anchor, sel_scope);
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
        // A page appends below what is shown: rows still in their place keep their
        // selection marks (the worker keeps the selection for an append).
        let mut rows = rows;
        let shown: Vec<Trk> = app.get_search_tracks().iter().collect();
        carry_marks(&shown, &mut rows, |r| r.selected, |r, on| r.selected = on, |a, b| a.uri == b.uri);
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
    // One failed page used to end the list for good ("stopped after N tracks") — a
    // network hiccup mid-scroll left a 900-track playlist looking like 300. Transient
    // failures get two more tries; a rate limit doesn't (hammering prolongs it).
    let mut attempt = 0u64;
    let fetched = loop {
        let r = match &pager.source {
            PagerSource::Liked => session.saved_tracks_page(pager.offset).await,
            PagerSource::Playlist(id) => session.playlist_tracks_page(id, pager.offset).await,
            PagerSource::Artist(id) => session.artist_tracks_page(id, pager.offset).await,
            PagerSource::None => Ok(lightify_core::TrackPage {
                tracks: Vec::new(),
                total: 0,
                partial: false,
                raw: 0,
            }),
        };
        match r {
            Err(e) if attempt < 2 && !lightify_core::ratelimit::is_rate_limited(&e) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(700 * attempt)).await;
            }
            r => break r,
        }
    };
    match fetched {
        Ok(page) => {
            pager.partial |= page.partial;
            // Entries Spotify sent, not rows kept (see `TrackPage::raw`).
            let n = page.raw as u32;
            if page.total > 0 {
                pager.total = page.total;
            }
            pager.offset += limit;
            if matches!(pager.source, PagerSource::Artist(_)) {
                // Paged by release: a page's track count says nothing about whether
                // more releases follow (pages also run short mid-list), and the same
                // song recurs across a single, its album and compilations.
                lightify_core::append_new_songs(tracks, page.tracks);
                pager.done = pager.offset >= pager.total;
            } else {
                tracks.extend(page.tracks);
                pager.done = n < limit || (pager.total > 0 && tracks.len() as u32 >= pager.total);
            }
            push_search_tracks(weak, tracks);
            let loaded = tracks.len();
            if loaded == 0 && !pager.done {
                // Nothing credited to the artist in these releases; the list's own
                // near-the-end check asks for the next page straight away.
                set_status(weak, tr!("Loading \u{201c}{}\u{201d}\u{2026}", pager.name));
            } else if loaded == 0 {
                set_status(weak, tr!("No tracks to show"));
            } else if pager.done && pager.partial {
                set_status(
                    weak,
                    lightify_i18n::trn!(
                        "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        loaded, pager.name
                    ),
                );
            } else if pager.done {
                set_status(weak, lightify_i18n::trn!("Loaded {n} tracks from \u{201c}{0}\u{201d}", "Loaded {n} tracks from \u{201c}{0}\u{201d}", loaded, pager.name));
            } else {
                set_status(
                    weak,
                    lightify_i18n::trn!(
                        "Loaded {n} tracks from \u{201c}{0}\u{201d}\u{2026}",
                        "Loaded {n} tracks from \u{201c}{0}\u{201d}\u{2026}",
                        loaded, pager.name
                    ),
                );
            }
        }
        Err(e) => {
            if tracks.is_empty() {
                set_status(weak, tr!("\u{201c}{}\u{201d} failed: {}", pager.name, e));
            } else {
                set_status(
                    weak,
                    lightify_i18n::trn!(
                        "\u{201c}{}\u{201d} stopped after {n} tracks: {}",
                        "\u{201c}{}\u{201d} stopped after {n} tracks: {}",
                        tracks.len(), pager.name, e
                    ),
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
        set_status(weak, tr!("Loading \u{201c}{}\u{201d}\u{2026}", pager.name));
    }

    let limit = pager.page_size();
    // One failed page used to end the list for good ("stopped after N tracks") — a
    // network hiccup mid-scroll left a 900-track playlist looking like 300. Transient
    // failures get two more tries; a rate limit doesn't (hammering prolongs it).
    let mut attempt = 0u64;
    let fetched = loop {
        let r = match &pager.source {
            PagerSource::Liked => session.saved_tracks_page(pager.offset).await,
            PagerSource::Playlist(id) => session.playlist_tracks_page(id, pager.offset).await,
            PagerSource::None | PagerSource::Artist(_) => Ok(lightify_core::TrackPage {
                tracks: Vec::new(),
                total: 0,
                partial: false,
                raw: 0,
            }),
        };
        match r {
            Err(e) if attempt < 2 && !lightify_core::ratelimit::is_rate_limited(&e) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(700 * attempt)).await;
            }
            r => break r,
        }
    };

    match fetched {
        Ok(page) => {
            let (batch, total, raw) = (page.tracks, page.total, page.raw);
            pager.partial |= page.partial;
            // Entries Spotify sent, not rows kept (see `TrackPage::raw`).
            let n = raw as u32;
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
                set_status(weak, tr!("No tracks to show"));
            } else if pager.done && pager.partial {
                // Non-owned playlists can only be read from the public embed page,
                // which stops at ~100 tracks. Say so rather than letting a 460-track
                // playlist quietly look like a 100-track one.
                set_status(
                    weak,
                    lightify_i18n::trn!(
                        "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        loaded, pager.name
                    ),
                );
            } else if pager.done {
                set_status(weak, lightify_i18n::trn!("Loaded {n} tracks from \u{201c}{0}\u{201d}", "Loaded {n} tracks from \u{201c}{0}\u{201d}", loaded, pager.name));
            } else {
                let done = group_thousands(loaded as u32);
                set_status(
                    weak,
                    if pager.total > 0 {
                        lightify_i18n::trn!(
                            "Loaded {0} / {1} tracks from \u{201c}{2}\u{201d}",
                            "Loaded {0} / {1} tracks from \u{201c}{2}\u{201d}",
                            pager.total, done, group_thousands(pager.total), pager.name
                        )
                    } else {
                        lightify_i18n::trn!("Loaded {0} tracks from \u{201c}{1}\u{201d}", "Loaded {0} tracks from \u{201c}{1}\u{201d}", loaded, done, pager.name)
                    },
                );
            }
        }
        Err(e) => {
            // Whatever already arrived stays on screen; the original does the same and
            // just stops paging rather than blanking the list.
            if tracks.is_empty() {
                set_status(weak, tr!("\u{201c}{}\u{201d} failed: {}", pager.name, e));
            } else {
                set_status(
                    weak,
                    lightify_i18n::trn!(
                        "\u{201c}{}\u{201d} stopped after {n} tracks: {}",
                        "\u{201c}{}\u{201d} stopped after {n} tracks: {}",
                        tracks.len(), pager.name, e
                    ),
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

/// `sel_key` backwards: the display row the key `anchor` is on now (`None` when its
/// track is filtered out of view).
fn anchor_row(scope: i32, anchor: usize, view: &[usize]) -> Option<usize> {
    if scope == SEL_TRACKS {
        view.iter().position(|&k| k == anchor)
    } else {
        Some(anchor)
    }
}

// ── Hold-to-select drags ──────────────────────────────────────────────────────
//
// The UI reports a drag as a y in the list's content; these map it to a display
// row. The heights must match the row components in ui/app.slint (TrackRow 46,
// BpRow 46, SideTrackRow 48; SearchResultRow varies by kind).
const ROW_H_TRACK: f32 = 46.0;
const ROW_H_BEATPORT: f32 = 46.0;
const ROW_H_SIDEBAR: f32 = 48.0;

/// Top edge of every search-result row, as last pushed (the list mixes heights).
static SEARCH_ROW_TOPS: std::sync::Mutex<Vec<f32>> = std::sync::Mutex::new(Vec::new());

fn search_row_tops(rows: &[SearchRow]) -> Vec<f32> {
    let mut y = 0.0;
    rows.iter()
        .map(|r| {
            let top = y;
            y += match r.kind.as_str() {
                "header" | "recent" => 36.0,
                "top" => 96.0,
                _ => 52.0,
            };
            top
        })
        .collect()
}

/// The display row under content-y `y` in `scope`'s list of `len` rows.
fn drag_row(scope: i32, y: f32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let y = y.max(0.0);
    let row = if scope == SEL_SEARCH {
        let tops = SEARCH_ROW_TOPS.lock().map(|t| t.clone()).unwrap_or_default();
        tops.iter().rposition(|t| *t <= y).unwrap_or(0)
    } else {
        let h = match scope {
            SEL_BEATPORT => ROW_H_BEATPORT,
            SEL_SIDEBAR => ROW_H_SIDEBAR,
            _ => ROW_H_TRACK,
        };
        (y / h) as usize
    };
    row.min(len - 1)
}

/// `base` plus every selectable row between `from` and `to` (display rows, either
/// order) — one hold-to-select drag added on top of what was already selected.
/// `selectable` says which display rows can be picked (search mixes in headers,
/// artists and albums, which never are).
fn drag_selection(
    scope: i32,
    base: &std::collections::BTreeSet<usize>,
    from: usize,
    to: usize,
    view: &[usize],
    selectable: impl Fn(usize) -> bool,
) -> std::collections::BTreeSet<usize> {
    let (lo, hi) = if to < from { (to, from) } else { (from, to) };
    let mut out = base.clone();
    for r in lo..=hi {
        if selectable(r) {
            if let Some(k) = sel_key(scope, r, view) {
                out.insert(k);
            }
        }
    }
    out
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
        _ => app.set_sel_count(selected.len() as i32),
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
        // No cap here: the engine plays any length, `play_track_list` slices for the
        // Web API itself, and a saved playlist must get every selected track.
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
    // The reset wipes repeat on the device; the engine keeps the user's mode (see its
    // `ClearQueue`), and a Web-API play that follows gets it back from
    // `restore_repeat_after_load`.
    let repeat = repeat_choice().unwrap_or_default();
    match current {
        Some((uri, position_ms, playing)) => serde_json::json!({
            "cmd": "clearqueue", "keep_uri": uri, "position_ms": position_ms, "playing": playing, "repeat": repeat,
        }),
        None => serde_json::json!({ "cmd": "clearqueue", "repeat": repeat }),
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
        set_status(weak, tr!("Start station needs a Spotify track"));
        return;
    }
    if !engine::running() {
        set_status(
            weak,
            tr!("Start station needs Lightify\u{2019}s own player \u{2014} Settings \u{2192} Restart"),
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
    set_status(weak, tr!("Building station\u{2026}"));
}

// ── Row context menus (the native `.ctx-menu`) ───────────────────────────────

/// Honest notes for menu rows the shipped app has but this shell can't run yet.
/// The rows stay visible (dimmed) rather than vanishing — see PARITY.md.
const FOLLOW_ARTIST_NOTE: &str = lightify_i18n::tr_noop!(
    "Follow artist needs the user-follow-modify scope, which this token doesn\u{2019}t carry");

const ALREADY_SAVED_NOTE: &str = lightify_i18n::tr_noop!("This playlist is already in your library");

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
        Self { label: label.into(), danger: false, muted: true, cmd: Cmd::Note(lightify_i18n::translate(note)) }
    }
}

/// The group menu shown when the right-clicked row is part of a multi-row
/// selection — ports `buildSpotifySelectionMenu` (`app.js:1253`).
fn selection_menu(count: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(lightify_i18n::trn!("Play {n} selected", "Play {n} selected", count), Cmd::PlaySelection),
        CtxItem::new(lightify_i18n::trn!("Add {n} to queue", "Add {n} to queue", count), Cmd::QueueSelection),
        CtxItem::new(tr!("Create playlist from selection"), Cmd::CreatePlaylistFromSelection),
    ]
}

/// The Liked Songs row menu — ports `app.js:1894`.
fn liked_row_menu() -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Open"), Cmd::Drill(0)),
        CtxItem::new(tr!("Download liked songs"), Cmd::DownloadLibraryRow(0)),
    ]
}

/// A playlist row menu — ports `app.js:1921`. `row` is the *display* index
/// (row 0 is Liked Songs, so a playlist is always >= 1).
fn playlist_row_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Play"), Cmd::PlayRow(row)),
        // Every row of this list IS in the library already: following it again did
        // nothing but say "saved". Kept (the original's menu has it), dimmed, and it
        // says so — the same treatment as "Follow artist".
        CtxItem::muted(tr!("Save to library"), ALREADY_SAVED_NOTE),
        CtxItem::new(tr!("Download"), Cmd::DownloadLibraryRow(row)),
        CtxItem::danger(tr!("Delete"), Cmd::ConfirmDeletePlaylist(row)),
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
    let mut items = vec![CtxItem::new(tr!("Play"), Cmd::PlayTrack { row, uri: t.uri.clone() })];
    if !t.uri.is_empty() {
        items.push(CtxItem::new(tr!("Add to queue"), Cmd::QueueTrack { row, uri: t.uri.clone() }));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new(tr!("Share"), Cmd::ShareTrack(row)));
        items.push(CtxItem::new(tr!("Start station"), Cmd::StationFromUri(t.uri.clone())));
        if !t.artist_ids.is_empty() {
            items.push(CtxItem::muted(tr!("Follow artist"), FOLLOW_ARTIST_NOTE));
        }
        items.push(CtxItem::new(tr!("Download"), Cmd::Download {
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
    let mut items = vec![CtxItem::new(tr!("Play"), Cmd::OpenSearch(row))];
    if !uri.is_empty() {
        items.push(CtxItem::new(tr!("Add to queue"), Cmd::QueueSearchTrack(row)));
    }
    if !id.is_empty() {
        if show_like {
            items.push(CtxItem::new(tr!("Like"), Cmd::LikeSearchTrack(row)));
        }
        items.push(CtxItem::new(tr!("Share"), Cmd::ShareSearchTrack(row)));
        items.push(CtxItem::new(tr!("Start station"), Cmd::StationFromUri(uri.to_string())));
        items.push(CtxItem::new(tr!("Download"), Cmd::DownloadSearchItem(row)));
    }
    items
}

/// A search *playlist* hit — `appendSearchPlaylistResult` (`app.js:2764`).
/// "Add to queue" is a shell addition (the original only queued single tracks).
fn search_playlist_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Play"), Cmd::PlaySearchContext(row)),
        CtxItem::new(tr!("Add to queue"), Cmd::QueueSearchHit(row)),
        CtxItem::new(tr!("Show tracks"), Cmd::OpenSearch(row)),
        CtxItem::new(tr!("Save playlist"), Cmd::SaveSearchItem(row)),
        CtxItem::new(tr!("Download"), Cmd::DownloadSearchItem(row)),
    ]
}

/// A search *album* hit — `app.js:2749`, plus the shell's "Add to queue".
fn search_album_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Play"), Cmd::PlaySearchContext(row)),
        CtxItem::new(tr!("Add to queue"), Cmd::QueueSearchHit(row)),
        CtxItem::new(tr!("Show tracks"), Cmd::OpenSearch(row)),
        CtxItem::new(tr!("Save album"), Cmd::SaveSearchItem(row)),
    ]
}

/// A search *artist* hit — `app.js:2731`, plus queueing the artist's top songs
/// (the same list "Show tracks" opens).
fn search_artist_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Show tracks"), Cmd::OpenSearch(row)),
        CtxItem::new(tr!("Add top songs to queue"), Cmd::QueueSearchHit(row)),
    ]
}

/// Most tracks one "Add to queue" on a whole album / playlist / artist sends. The
/// same 50 a multi-row selection is capped at (`scoped_selection_uris`): Spotify has
/// no batch queue endpoint, so every track is its own request against the dev-mode
/// quota, and the worker is busy until the run ends.
const QUEUE_HIT_MAX: usize = 50;

/// The tracks a search hit's "Add to queue" sends, in the hit's own order: playable,
/// de-duplicated, at most [`QUEUE_HIT_MAX`]. Also returns the hit's name and how many
/// tracks it holds, so a capped run can say so. Playlists are read a page at a time
/// (with the drill-in's embed fallback) only until enough tracks are in hand.
async fn search_hit_tracks(
    session: &mut Session,
    action: &SearchAction,
) -> Result<(String, Vec<Track>, usize), String> {
    let (name, fetched, total) = match action {
        SearchAction::Album { id, name, .. } => {
            let t = session.album_tracks(id).await?;
            let n = t.len();
            (name.clone(), t, n)
        }
        SearchAction::Artist { id, name } => {
            let t = session.artist_tracks(id, name).await?;
            let n = t.len();
            (name.clone(), t, n)
        }
        SearchAction::Playlist { id, name, .. } => {
            let mut out: Vec<Track> = Vec::new();
            let mut total = 0usize;
            let mut offset = 0u32;
            loop {
                let page = match playlist_page_retrying(session, id, offset).await {
                    Ok(p) => p,
                    // Keep what already arrived; a later page failing (after the
                    // pagers' retries) isn't a reason to queue nothing.
                    Err(e) if out.is_empty() => return Err(e),
                    Err(_) => break,
                };
                let n = page.raw as u32;
                total = total.max(page.total as usize);
                out.extend(page.tracks);
                offset += Session::PLAYLIST_PAGE;
                let usable = out.iter().filter(|t| t.is_playable && !t.uri.is_empty()).count();
                if n < Session::PLAYLIST_PAGE || usable >= QUEUE_HIT_MAX || (total > 0 && out.len() >= total) {
                    break;
                }
            }
            let total = total.max(out.len());
            (name.clone(), out, total)
        }
        _ => return Err("Nothing to queue here".to_string()),
    };
    let mut seen = std::collections::HashSet::new();
    let tracks: Vec<Track> = fetched
        .into_iter()
        .filter(|t| t.is_playable && !t.uri.is_empty() && seen.insert(t.uri.clone()))
        .take(QUEUE_HIT_MAX)
        .collect();
    Ok((name, tracks, total))
}

/// One playlist page with the track pagers' retry (`load_next_track_page`): two more
/// tries after a transient failure, none after a rate limit (hammering prolongs it).
/// lightify-core returns `Err` for anything but a refusal, so a hiccup on a later page
/// used to cut the run short.
async fn playlist_page_retrying(session: &mut Session, id: &str, offset: u32) -> Result<lightify_core::TrackPage, String> {
    let mut attempt = 0u64;
    loop {
        match session.playlist_tracks_page(id, offset).await {
            Err(e) if attempt < 2 && !lightify_core::ratelimit::is_rate_limited(&e) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(700 * attempt)).await;
            }
            r => return r,
        }
    }
}

/// A Beatport chart row — `app.js:3489`.
fn beatport_row_menu(row: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Play"), Cmd::BpPlay(row)),
        CtxItem::new(tr!("Add to queue"), Cmd::BpQueue(row)),
        CtxItem::new(tr!("Download"), Cmd::DownloadBeatportRow(row)),
    ]
}

/// `buildBeatportSelectionMenu` (`app.js:4020`).
fn beatport_selection_menu(count: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(lightify_i18n::trn!("Play {n} selected", "Play {n} selected", count), Cmd::BpPlaySelection),
        CtxItem::new(lightify_i18n::trn!("Add {n} to queue", "Add {n} to queue", count), Cmd::BpQueueSelection),
        CtxItem::new(tr!("Create playlist from selection"), Cmd::BpCreatePlaylist),
    ]
}

/// `buildRecentSidebarSelectionMenu` (`app.js:5773`) — RECENT has no "create
/// playlist"; the QUEUE mode uses the ordinary `buildSpotifySelectionMenu`.
fn sidebar_recent_selection_menu(count: usize) -> Vec<CtxItem> {
    vec![
        CtxItem::new(tr!("Play first selected"), Cmd::SidebarPlayFirstSelected),
        CtxItem::new(lightify_i18n::trn!("Add {n} to queue", "Add {n} to queue", count), Cmd::QueueSelection),
    ]
}

/// The now-playing menu — `showCurrentTrackContextMenu` (`app.js:5222`), bound to the
/// album art and the track info. It is `buildSpotifyTrackMenu` against the playing
/// track, so the rows match a track row's menu exactly.
fn now_playing_menu(t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new(tr!("Play"), Cmd::PlayCurrentTrack)];
    if !t.uri.is_empty() {
        items.push(CtxItem::new(tr!("Add to queue"), Cmd::AddQueue));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new(tr!("Share"), Cmd::ShareCurrentTrack));
        items.push(CtxItem::new(tr!("Start station"), Cmd::StationFromUri(t.uri.clone())));
        if !t.artist_ids.is_empty() {
            items.push(CtxItem::muted(tr!("Follow artist"), FOLLOW_ARTIST_NOTE));
        }
        items.push(CtxItem::new(tr!("Download"), Cmd::Download {
            url: format!("https://open.spotify.com/track/{}", t.id),
            label: t.name.clone(),
        }));
    }
    items
}

/// A row in the search tab's drill-in — `buildSpotifyTrackMenu` against a track of
/// the drilled playlist/album.
fn search_drill_menu(row: usize, t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new(tr!("Play"), Cmd::PlaySearchDrillTrack(row))];
    if !t.uri.is_empty() {
        items.push(CtxItem::new(tr!("Add to queue"), Cmd::QueueSearchDrillTrack(row)));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new(tr!("Share"), Cmd::ShareSearchDrillTrack(row)));
        items.push(CtxItem::new(tr!("Start station"), Cmd::StationFromUri(t.uri.clone())));
        if !t.artist_ids.is_empty() {
            items.push(CtxItem::muted(tr!("Follow artist"), FOLLOW_ARTIST_NOTE));
        }
        items.push(CtxItem::new(tr!("Download"), Cmd::Download {
            url: format!("https://open.spotify.com/track/{}", t.id),
            label: t.name.clone(),
        }));
    }
    items
}

/// A sidebar row — `buildSpotifyTrackMenu` against the queue/recent track.
fn sidebar_row_menu(row: usize, t: &Track) -> Vec<CtxItem> {
    let mut items = vec![CtxItem::new(tr!("Play"), Cmd::SidebarPlay(row))];
    if !t.uri.is_empty() {
        items.push(CtxItem::new(tr!("Add to queue"), Cmd::SidebarQueueRow(row)));
    }
    if !t.id.is_empty() {
        items.push(CtxItem::new(tr!("Share"), Cmd::ShareSidebarTrack(row)));
        items.push(CtxItem::new(tr!("Start station"), Cmd::StationFromUri(t.uri.clone())));
        items.push(CtxItem::new(tr!("Download"), Cmd::Download {
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
        "Failed" | "Cancelled" => items.push(CtxItem::new(tr!("Retry"), Cmd::DownloadRetry(item.local_id.clone()))),
        "Downloaded" | "Already Exists" | "Deleted" | "Unavailable" => {}
        _ => items.push(CtxItem::new(tr!("Cancel"), Cmd::DownloadCancel(item.local_id.clone()))),
    }
    items.push(CtxItem::new(
        if item.file_path.is_empty() { tr!("Open download folder") } else { tr!("Open folder") },
        Cmd::DownloadOpenFolder(item.file_path.clone()),
    ));
    // The bridge only knows the file *was* written; one deleted or moved since made
    // "Delete file" fail. (The bridge now treats that as done too, but a bundled
    // bridge from before still answers it with an error.)
    if item.on_disk() && std::path::Path::new(&item.file_path).is_file() {
        items.push(CtxItem::danger(tr!("Delete file"), Cmd::DownloadDelete(item.local_id.clone())));
    }
    items
}

/// Push a built menu to the UI (opening it) and return the parallel action list,
/// the same shape the command palette uses.
/// `.ctx-menu` has a minimum width and grows to fit its longest item. A Slint
/// element that isn't inside a layout can't size itself to its content, so the width
/// is estimated here from the label lengths (13px Space Grotesk medium ≈ 7.0px per
/// glyph, plus 10px row padding and 6px panel padding each side) and clamped so a
/// long label never runs off the panel.
/// The name prompt's note for a playlist made from the queue / a selection.
fn name_note_queue(n: usize) -> String {
    lightify_i18n::trn!("{n} track from your queue", "{n} tracks from your queue", n)
}

fn name_note_selected(n: usize) -> String {
    lightify_i18n::trn!("{n} track selected", "{n} tracks selected", n)
}

fn menu_width(items: &[CtxItem]) -> f32 {
    let longest = items.iter().map(|i| label_width(&i.label)).fold(0.0, f32::max);
    (longest + 34.0).clamp(176.0, 320.0)
}

/// A label's estimated width at 13px: 7px a character, as `menu_width` has always
/// assumed, except a CJK / Hangul / full-width character, which is about as wide as
/// the font is tall (13px). Latin text measures exactly as before.
fn label_width(label: &str) -> f32 {
    label.chars().map(|c| if is_wide(c) { 13.0 } else { 7.0 }).sum()
}

/// East Asian wide and full-width characters (Unicode's EastAsianWidth W/F, by block).
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F       // Hangul Jamo (leading)
        | 0x2E80..=0x303E     // CJK radicals, punctuation
        | 0x3041..=0x33FF     // kana, Bopomofo, CJK compatibility
        | 0x3400..=0x4DBF     // CJK extension A
        | 0x4E00..=0x9FFF     // CJK unified ideographs
        | 0xA000..=0xA4CF     // Yi
        | 0xAC00..=0xD7A3     // Hangul syllables
        | 0xF900..=0xFAFF     // CJK compatibility ideographs
        | 0xFE30..=0xFE4F     // CJK compatibility forms
        | 0xFF00..=0xFF60     // full-width forms
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD)  // CJK extensions B…
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
        set_status(weak, tr!("Share \u{2014} no Spotify id"));
        return;
    }
    let url = format!("https://open.spotify.com/{kind}/{id}");
    match copy_to_clipboard(&url) {
        Ok(()) => set_status(weak, tr!("Copied Spotify link")),
        Err(e) => set_status(weak, tr!("Share \u{2014} {}", e)),
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
    cb.set_text(text.to_string()).map_err(|e| tr!("could not set the clipboard: {}", e))
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
        // Solid (opaque) form of the same album tint. The UI re-points the global accent
        // from it (like the original's `applyDynamicAccent` reassigning `--accent`: the
        // play/pause button, volume, progress, tabs, highlighted rows, the mini frame) —
        // unless a local track is what's present, whose cover tint wins then.
        let solid = slint::Color::from_argb_u8(255, glow.red(), glow.green(), glow.blue());
        app.set_spotify_accent(solid);
    });
}

fn clear_art(weak: &slint::Weak<MainWindow>) {
    let _ = weak.upgrade_in_event_loop(|app| {
        app.set_album_art(slint::Image::default());
        app.set_glow(slint::Color::from_argb_u8(0x55, 0x34, 0xd3, 0x99));
        // Matches `resetDynamicAccent` — back to the default teal when no cover is loaded.
        app.set_spotify_accent(slint::Color::from_argb_u8(0xff, 0x34, 0xd3, 0x99));
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

/// For `--shot-settings … account-problem` only: the engine card's warning to render.
static SHOT_ACCOUNT_PROBLEM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// For `--shot-settings … free-account` only: render the Free-account notes.
static SHOT_FREE_PLAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
/// `--shot-artfull`: render with the cover expanded.
static SHOT_ART_FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `--shot-signin`: render the sign-in page.
static SHOT_SIGNIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `--shot-nameprompt`: render with the name prompt open (0 left, 1 right; -1 off).
static SHOT_NAME_PROMPT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
/// `--shot-update`: the update state to show in Settings ("" = not this shot).
static SHOT_UPDATE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// `--shot-bpqueue`: 0 off, 1 a chart lining up, 2 lined up.
/// `--shot-local`: the Local view variant to show ("" = not this shot).
static SHOT_LOCAL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// `--shot-lava <variant>[@secs]`, as `SHOT_LOCAL`.
static SHOT_LAVA: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static SHOT_BP_QUEUE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

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
                base.drill = Some((tr!("Liked Songs"), page.tracks));
            }
        }
    } else if let Some(pl) = base.playlists.get(index - 1).cloned() {
        if let Ok(mut s) = Session::load() {
            if let Ok(page) = s.playlist_tracks_page(&pl.id, 0).await {
                // Mirror the status the interactive pager sets, so the render shows
                // the same honesty about an embed-only list.
                if page.partial {
                    base.status = lightify_i18n::trn!(
                        "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                        "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
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
    // Only a sign-in Spotify refused is the "expired" page; offline (or Spotify's token
    // service failing) the live app starts on its cache (M1), and so does this.
    let fresh = s.ensure_fresh().await;
    if let Some(e) = fresh.err().filter(|e| refresh_failure_notice(e).is_some()) {
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
        if n.is_empty() { tr!("Connected") } else { tr!("Connected as {}", n) }
    };
    let playlists = match s.playlists().await {
        Ok(p) => p,
        Err(_) => load_library_cache().unwrap_or_default(),
    };
    thumbs::prefetch(&playlists).await;
    let mut now = s.playback().await.ok().flatten();
    // Mirror the live app's resume offer (UI-PLAN D3): nothing playing anywhere +
    // a saved resume point = that track, paused where it stopped.
    if now.is_none() {
        if let Some(rp) = load_resume() {
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
async fn fetch_search_drill_snapshot(query: &str, artist: bool) -> Snapshot {
    let mut base = fetch_search_snapshot(query, if artist { "artists" } else { "all" }).await;
    if let Ok(mut s) = Session::load() {
        if let Ok(res) = s.search(query).await {
            if artist {
                // The first artist hit's discography, two release pages deep — the
                // first page plus the one the list's near-the-end check asks for.
                if let Some(a) = res.artists.first().cloned() {
                    let mut tracks = Vec::new();
                    let mut offset = 0;
                    let mut error = None;
                    for _ in 0..2 {
                        match s.artist_tracks_page(&a.id, offset).await {
                            Ok(page) => {
                                lightify_core::append_new_songs(&mut tracks, page.tracks);
                                offset += Session::ARTIST_RELEASE_PAGE;
                                if offset >= page.total {
                                    break;
                                }
                            }
                            Err(e) => {
                                error = Some(e);
                                break;
                            }
                        }
                    }
                    base.status = match error {
                        Some(e) => tr!("Artist \u{2014} {}", e),
                        None => lightify_i18n::trn!(
                            "Loaded {n} tracks from \u{201c}{0}\u{201d}\u{2026}",
                            "Loaded {n} tracks from \u{201c}{0}\u{201d}\u{2026}",
                            tracks.len(),
                            a.name
                        ),
                    };
                    base.search_drill = Some((a.name, tracks));
                }
            } else if let Some(pl) = res.playlists.first().cloned() {
                if let Ok(page) = s.playlist_tracks_page(&pl.id, 0).await {
                    base.status = if page.partial {
                        lightify_i18n::trn!(
                            "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                            "Loaded {n} tracks from \u{201c}{0}\u{201d} \u{2014} preview only; Spotify won\u{2019}t serve this playlist to this app",
                            page.tracks.len(),
                            pl.name
                        )
                    } else {
                        lightify_i18n::trn!("Loaded {n} tracks from \u{201c}{0}\u{201d}", "Loaded {n} tracks from \u{201c}{0}\u{201d}", page.tracks.len(), pl.name)
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
/// headless verification. `kind` = tracks | hype | releases; `genre` indexes
/// `beatport_genres()` (0 = Overall).
async fn fetch_beatport_snapshot(kind: &str, genre: usize) -> Snapshot {
    let mut base = fetch_snapshot().await;
    base.tab = 2;
    let slug = lightify_core::beatport_genres().get(genre).map(|(_, s)| s.clone()).unwrap_or_default();
    if let Ok(s) = Session::load() {
        match s.beatport_chart(&slug, kind).await {
            Ok(t) => base.beatport = Some((genre, kind.to_string(), t)),
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
            2 => play_history::merged(&s.recently_played(50).await.unwrap_or_default()),
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
    // `status` is the account line (Settings); the titlebar pill stays empty at rest.
    app.set_status_text("".into());
    // No usable session: the live app shows the sign-in page (`obtain_session`), so a
    // headless render does too — `LIGHTIFY_DATA_DIR=<empty dir> --shot-live` is what
    // a fresh install sees.
    if let Some(err) = snap.status.strip_prefix("Auth error — ") {
        app.set_signin_open(true);
        app.set_signin_client_id(lightify_core::auth::saved_client_id().into());
        app.set_signin_status(
            tr!("Your Spotify sign-in has expired or was revoked \u{2014} authorise again. ({})", err).into(),
        );
        app.set_signin_error(true);
    } else if snap.status.starts_with("Not connected") {
        app.set_signin_open(true);
        app.set_signin_client_id(lightify_core::auth::saved_client_id().into());
        app.set_status_text(tr!("Sign in to Spotify to start").into());
    }
    // Headless renders derive the playback-presence highlight from the live context URI
    // (a snapshot has no launch history). `snap.sort` mirrors the toolbar selection.
    app.set_engine_status(snap.engine.0.clone().into());
    app.set_engine_ok(snap.engine.1);
    if SHOT_FREE_PLAN.load(std::sync::atomic::Ordering::SeqCst) {
        app.set_engine_plan_note(tr!("This account is Spotify Free. The player needs Spotify Premium, so songs may not play. Local mode works with any account.").into());
        app.set_downloader_plan_note(tr!("This account is Spotify Free. The downloader needs Spotify Premium, so downloads may not work.").into());
        app.set_signin_open(false);
    }
    if let Some(p) = SHOT_ACCOUNT_PROBLEM.lock().ok().and_then(|g| g.clone()) {
        app.set_engine_account_problem(p.into());
        // Rendered without an account (no live engine to disturb playback): the sign-in
        // page would cover the card this shot is about.
        app.set_signin_open(false);
    }
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
            app.set_track_artist(tr!("Nothing playing").into());
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
        app.set_spotify_accent(slint::Color::from_argb_u8(255, glow.red(), glow.green(), glow.blue()));
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
        app.set_sidebar_empty_plain(*mode == 1 || *mode == 2);
        app.set_sidebar_empty(
            match mode {
                1 => tr!("No queued tracks"),
                2 => tr!("No recent tracks yet"),
                _ => String::new(),
            }
            .into(),
        );
        app.set_sidebar_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    }
    if let Some(items) = &snap.downloads {
        app.set_sidebar_empty(tr!("No downloads queued").into());
        app.set_downloads_clearable(items.iter().any(|i| i.clearable()));
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
            lightify_i18n::trn!(
                "{n} tracks \u{2014} {} {}",
                "{n} tracks \u{2014} {} {}",
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
            app.set_settings_device_status(tr!("No devices yet \u{2014} Lightify\u{2019}s own player is still starting.").into());
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
        let mut rows: Vec<DeviceRow> = vec![DeviceRow { name: tr!("System default").into(), active: default_active }];
        for name in &outputs {
            rows.push(DeviceRow { name: name.clone().into(), active: !default_active && name == want });
        }
        app.set_settings_outputs(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        if missing {
            app.set_settings_output_note(
                tr!("Configured output \u{201c}{}\u{201d} isn\u{2019}t connected \u{2014} playing on the system default.", want).into(),
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
            app.set_confirm_title(tr!("DELETE PLAYLIST").into());
            app.set_confirm_message(
                tr!("Delete \u{201c}{}\u{201d} from your Spotify library?", name).into(),
            );
            app.set_confirm_open(true);
        } else {
            // Same builders the interactive path uses, so the shot verifies the
            // real menus. The coordinates put each menu over its own row.
            // The other three selectable surfaces. Live data can't drive these here
            // (Beatport is Cloudflare-blocked and the queue is usually empty), so the
            // rows are deterministic mocks — the selection marks, the group menus and
            // the action bars are the real code paths.
            // `search-album` / `search-playlist` / `search-artist` open that hit's own
            // row menu over the same mock results instead of a selection menu.
            if kind == "search" || kind.starts_with("search-") || kind == "bp" || kind == "sidebar" {
                let n = 2;
                let menu = match kind.as_str() {
                    k if k.starts_with("search") => {
                        app.set_tab(1);
                        let rows = vec![
                            SearchRow { name: "Midnight City".into(), sub: "Song \u{2022} M83".into(), kind: "track".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "Outro".into(), sub: "Song \u{2022} M83".into(), kind: "track".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "Hurry Up, We\u{2019}re Dreaming".into(), sub: "Album \u{2022} M83".into(), kind: "album".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "This Is M83".into(), sub: "Playlist \u{2022} 52 tracks".into(), kind: "playlist".into(), selected: false, art: Default::default(), meta: Default::default() },
                            SearchRow { name: "M83".into(), sub: "Artist".into(), kind: "artist".into(), selected: false, art: Default::default(), meta: Default::default() },
                        ];
                        let len = rows.len();
                        app.set_search_results(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
                        match k {
                            "search-album" => search_album_menu(2),
                            "search-playlist" => search_playlist_menu(3),
                            "search-artist" => search_artist_menu(4),
                            _ => {
                                apply_search_selection(app, &(0..len).map(|i| i < n).collect::<Vec<_>>());
                                selection_menu(n)
                            }
                        }
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
                                pending: 0,
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
                app.set_ctx_y(match kind.as_str() {
                    "search-album" => 230.0,
                    "search-playlist" => 280.0,
                    "search-artist" => 330.0,
                    _ => 160.0,
                });
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

/// The UI face: Space Grotesk, bundled into the binary by the `import`s at the top of
/// ui/app.slint, so it is the same on every machine with nothing installed.
fn ui_font() -> &'static str {
    "Space Grotesk"
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
    // English unless `--lang` asks otherwise: a render never depends on the saved choice.
    apply_language(LANG_ARG.get().map(String::as_str).unwrap_or(lightify_i18n::ENGLISH));
    push_languages(&app, LANG_ARG.get().map(String::as_str).unwrap_or("auto"));
    cjk_font_fallback();
    app.set_app_version(env!("CARGO_PKG_VERSION").into());
    app.set_ui_font(ui_font().into());
    app.global::<Motion>().set_reduced(reduce_motion());
    // One frame is all a shot gets: render the END of every slide, not its start.
    app.global::<Motion>().set_slide(0);
    if let Some(snap) = snap.as_ref() {
        apply_snapshot(&app, snap);
    } else {
        // Sample data stands in for a signed-in session; the property's default
        // ("Connecting…") made every sample render look stuck.
        app.set_status_text("".into());
    }
    if SHOT_ART_FULL.load(std::sync::atomic::Ordering::Relaxed) {
        app.set_art_full(true);
    }
    if SHOT_SIGNIN.load(std::sync::atomic::Ordering::Relaxed) {
        app.set_signin_open(true);
    }
    let up = SHOT_UPDATE.lock().map(|g| g.clone()).unwrap_or_default();
    if !up.is_empty() {
        let (state, closed) = up.split_once(':').map(|(s, _)| (s, true)).unwrap_or((up.as_str(), false));
        app.set_update_state(state.into());
        app.set_update_version("2.3.5".into());
        app.set_update_progress(0.42);
        app.set_settings_open(!closed);
    }
    let bq = SHOT_BP_QUEUE.load(std::sync::atomic::Ordering::Relaxed);
    if bq > 0 {
        let chart = [
            ("Low Tide Theory", "Marlo Vance", 372_000), ("Cold Pressed", "Juno Ortega, KLM", 348_000),
            ("Basement Hymn", "Tess Ardent", 390_000), ("Velvet Static", "Oren Pike", 422_000),
            ("Night Bus Home", "Dara Quill", 355_000), ("No Signal", "Fen & Holloway", 381_000),
            ("Paper Moons", "Ilsa Wren", 404_000), ("Glasshouse", "Remi Stroud", 339_000),
            ("Off The Grid", "Cass Bellamy", 368_000), ("Undertow", "Niko Haas", 417_000),
            ("Static Bloom", "Ada Merrow, Kito", 351_000), ("Soft Machines", "Lio Tanner", 375_000),
            ("Late Checkout", "Bea Rossi", 362_000), ("Long Exposure", "Hal Okafor", 393_000),
        ];
        let lining = bq == 1;
        let found_upto = if lining { 5 } else { chart.len() };
        let tracks: Vec<Track> = chart[1..found_upto]
            .iter()
            .enumerate()
            .map(|(k, (n, a, d))| Track {
                id: format!("bp{k}"),
                name: n.to_string(),
                artists: a.to_string(),
                artist_ids: vec![],
                album: String::new(),
                album_id: String::new(),
                duration_ms: *d,
                uri: format!("spotify:track:bp{k}"),
                album_art: String::new(),
                added_at: None,
                is_playable: true,
            })
            .collect();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let source: Vec<BeatportTrack> = chart
            .iter()
            .enumerate()
            .map(|(k, (n, a, _))| BeatportTrack {
                rank: k as u32 + 1,
                name: n.to_string(),
                artists: a.to_string(),
                label: String::new(),
                isrc: String::new(),
            })
            .collect();
        set_bp_panel(Some(BpPanel {
            seq_id: 1,
            active,
            title: "// beatport \u{00b7} top 100 \u{00b7} tech house".into(),
            found: if lining { 23 } else { 96 },
            total: 100,
            waiting: if lining { bp_waiting(&source, found_upto) } else { Vec::new() },
            next_index: found_upto,
            start: 0,
            done: !lining,
            missed: if lining { 0 } else { 4 },
        }));
        let mut rows = build_side_rows(&tracks, "", false);
        let (pending, strip, found, total, note) = bp_panel_rows();
        rows.extend(pending);
        app.set_sidebar_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        app.set_bp_strip_title(strip.into());
        app.set_bp_strip_found(found);
        app.set_bp_strip_total(total);
        app.set_bp_done_note(note.into());
        app.set_track_name(chart[0].0.into());
        app.set_track_artist(chart[0].1.into());
        app.set_sidebar_mode(1);
    }
    // `<variant>@<seconds>` also draws the ASCII backdrop at that time.
    let shot_local = SHOT_LOCAL.lock().ok().map(|g| g.clone()).filter(|v| !v.is_empty());
    let shot_lava = SHOT_LAVA.lock().ok().map(|g| g.clone()).filter(|v| !v.is_empty());
    let shot_bg_t = shot_local
        .as_ref()
        .or(shot_lava.as_ref())
        .and_then(|v| v.split_once('@'))
        .and_then(|(_, t)| t.parse::<f32>().ok());
    if let Some(v) = shot_lava.as_ref().map(|v| v.split('@').next().unwrap_or("").to_string()) {
        app.set_playing(true);
        match v.as_str() {
            "menu" => {
                let items = vec![CtxItem::new(tr!("ASCII lava screensaver"), Cmd::LavaSaver)];
                app.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(ctx_rows(&items)))));
                app.set_ctx_w(menu_width(&items));
                app.set_ctx_x(820.0);
                app.set_ctx_y(250.0);
                app.set_ctx_open(true);
            }
            "saver" => app.set_lava_saver(true),
            "settings" => app.set_settings_open(true),
            // Settings with the language list open.
            "settings-lang" => {
                app.set_settings_open(true);
                app.set_settings_language_open(true);
            }
            "off" => app.set_lava_on(false),
            _ => {}
        }
    }
    if let Some(v) = shot_local.as_ref().map(|v| v.split('@').next().unwrap_or("").to_string()) {
        local_ui::shot(&app, &v);
        // The cross-fade always animates (280 ms): start it, then let it finish.
        slint::platform::update_timers_and_animations();
        let _ = app.get_local_fade();
        std::thread::sleep(Duration::from_millis(350));
        slint::platform::update_timers_and_animations();
        eprintln!("local-fade {}", app.get_local_fade());
    }
    let np = SHOT_NAME_PROMPT.load(std::sync::atomic::Ordering::Relaxed);
    if np >= 0 {
        app.set_name_prompt_side(np);
        if np == 1 {
            app.set_sidebar_mode(1);
            app.set_name_prompt_note(name_note_queue(9).into());
        } else {
            app.set_name_prompt_note(name_note_selected(4).into());
        }
        app.set_name_prompt_open(true);
    }
    window.set_size(slint::PhysicalSize::new(w, h));
    slint::platform::update_timers_and_animations();
    // With a backdrop, render once first (the backdrop needs its laid-out panel size);
    // the real render below then repaints what changed into the same buffer.
    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    if let Some(t) = shot_bg_t {
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, w as usize);
        });
        slint::platform::update_timers_and_animations();
        local_backdrop::shot_frame(&app, t);
    }

    // 8 bits per channel, like the real window (the winit backend presents 32-bit
    // pixels). This used to render RGB565, whose 32 red/blue levels drew visible
    // rings into every gradient — the now-playing glow above all — that the live
    // app never showed, so screenshots misreported the UI.
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w as usize);
    });
    if let Some(t) = shot_bg_t {
        // What a live backdrop frame costs: draw the next frame and repaint over it.
        let mut work = buffer.clone();
        let start = std::time::Instant::now();
        for i in 1..=20 {
            local_backdrop::shot_frame(&app, t + i as f32 * 0.066);
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut work, w as usize);
            });
        }
        eprintln!("backdrop frame + repaint: {:.2} ms avg", start.elapsed().as_secs_f64() * 1000.0 / 20.0);
    }

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
    /// Every bundled catalog, checked against the template (LANGUAGE-PLAN.md, "Checks
    /// that keep it from breaking"): it parses, keeps every placeholder, has the plural
    /// forms its header asks for. For translators: `cargo test catalogs -- --nocapture`
    /// also prints how much of each language is translated.
    #[test]
    fn catalogs_are_safe_to_show() {
        let pot = include_str!("../translations/lightify-shell.pot");
        let mut problems = Vec::new();
        for l in super::LANGS {
            let r = lightify_i18n::check::catalog(l.code, l.po, pot);
            println!(
                "{}: {} translated, {} shown in English (no translation yet), {} due a re-translation (now a count, \
                 or no longer one), {} entries the template no longer has",
                l.code,
                r.translated,
                r.fallback.len(),
                r.stale.len(),
                r.obsolete.len()
            );
            problems.extend(r.problems);
        }
        assert!(problems.is_empty(), "\n{} problem(s) in the catalogs:\n  {}\n", problems.len(), problems.join("\n  "));
    }

    #[test]
    fn menu_width_measures_wide_characters() {
        use super::label_width;
        // Latin text: 7 px a character, exactly as before.
        assert_eq!(label_width("Download liked songs"), 140.0);
        // CJK and Hangul take about the font size (13 px) each.
        assert_eq!(label_width("播放队列"), 52.0);
        assert_eq!(label_width("대기열"), 39.0);
        assert_eq!(label_width("再生 3 曲"), 13.0 * 3.0 + 7.0 * 3.0);
    }

    #[test]
    fn titlebar_double_press_needs_time_and_place() {
        use super::is_double_press;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let lim = Duration::from_millis(500);
        let first = Some((t0, 100.0, 10.0));
        assert!(is_double_press(first, (t0 + Duration::from_millis(200), 101.0, 11.0), lim, 2.0));
        assert!(!is_double_press(first, (t0 + Duration::from_millis(700), 100.0, 10.0), lim, 2.0), "too slow");
        assert!(!is_double_press(first, (t0 + Duration::from_millis(200), 140.0, 10.0), lim, 2.0), "too far");
        assert!(!is_double_press(None, (t0, 100.0, 10.0), lim, 2.0), "a first press is never a double");
    }
    use super::{
        active_source, another_device_active, beatport_row_menu, beatport_selection_menu,
        build_dl_rows, cmp_display_text, command_match_score, context_playlist_id, download_row_menu, Cmd,
        QueueMirror, PersistedQueue, queue_restore_point, restart_up_next, streaming_account_problem, plan_is_premium,
        downloader, is_subsequence, menu_width, next_sidebar_mode,
        now_playing_menu, parse_rfc3339_utc, rate_limit_backoff, scoped_selection_uris,
        search_album_menu, search_artist_menu, search_playlist_menu, search_track_menu, sel_key,
        selection_menu, sidebar_recent_selection_menu, sidebar_row_menu, sort_playlist_view,
        sort_track_view, SearchAction, SEL_BEATPORT, SEL_NONE, SEL_SEARCH, SEL_SEARCH_DRILL,
        SEL_SIDEBAR, SEL_TRACKS,
        drag_row, drag_selection, search_row_tops, SearchRow, SEARCH_ROW_TOPS,
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
            ["Play", "Add to queue", "Show tracks", "Save playlist", "Download"]
        );
        assert_eq!(labels(&search_album_menu(0)), ["Play", "Add to queue", "Show tracks", "Save album"]);
        assert_eq!(labels(&search_artist_menu(0)), ["Show tracks", "Add top songs to queue"]);
        // Every "Add to queue" on a hit is the whole-hit command, not the track one.
        for menu in [search_playlist_menu(4), search_album_menu(4), search_artist_menu(4)] {
            assert!(menu.iter().any(|i| matches!(i.cmd, Cmd::QueueSearchHit(4))));
        }
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
        let on_disk = std::env::temp_dir().join(format!("lightify-dl-menu-{}.mp3", std::process::id()));
        std::fs::write(&on_disk, b"x").unwrap();
        let file = on_disk.to_string_lossy().into_owned();
        assert_eq!(
            labels(&download_row_menu(&dl("Downloaded", &file))),
            ["Open folder", "Delete file"]
        );
        assert_eq!(
            labels(&download_row_menu(&dl("Already Exists", &file))),
            ["Open folder", "Delete file"]
        );
        // "Deleted" keeps its (now stale) path but must not offer Delete again.
        assert_eq!(
            labels(&download_row_menu(&dl("Deleted", &file))),
            ["Open folder"]
        );
        // Only the destructive row reads as danger.
        let menu = download_row_menu(&dl("Downloaded", &file));
        let danger: Vec<&str> = menu.iter().filter(|i| i.danger).map(|i| i.label.as_str()).collect();
        assert_eq!(danger, ["Delete file"]);
        // D13: a file deleted or moved since it was downloaded can't be deleted again.
        std::fs::remove_file(&on_disk).unwrap();
        assert_eq!(
            labels(&download_row_menu(&dl("Downloaded", &file))),
            ["Open folder"]
        );
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

    #[test]
    fn queue_mirror_ignores_the_track_a_play_replaced() {
        // Clicked "a" while "c" was playing; a poll still reporting "c" right after is
        // stale. It must neither drop the mirror nor eat "b" by folding forward to "c".
        let mut m = mirror(&["b", "c", "d"], "a").replacing("spotify:track:c");
        assert!(m.advance("spotify:track:c"));
        assert_eq!(m.upcoming.len(), 3);
        // Same for a previous track that isn't in the list at all.
        let mut m = mirror(&["b"], "a").replacing("spotify:track:zzz");
        assert!(m.advance("spotify:track:zzz"));
        // Real progress still folds.
        assert!(m.advance("spotify:track:b"));
        assert!(m.upcoming.is_empty());
    }

    #[test]
    fn spotify_backed_mirror_refills_once_per_track_when_low() {
        let ups: Vec<Track> = (0..8).map(|i| menu_trk(&format!("t{i}"), true)).collect();
        let mut m = QueueMirror::from_spotify("spotify:track:now".into(), ups);
        assert!(!m.wants_refill(), "8 left is plenty");
        assert!(m.advance("spotify:track:t2"));
        assert_eq!(m.upcoming.len(), 5);
        assert!(m.wants_refill());
        m.refill_for = m.current_uri.clone();
        assert!(!m.wants_refill(), "only once per track");
        assert!(m.advance("spotify:track:t3"));
        assert!(m.wants_refill(), "asks again on the next track");
        // A list the user played locally is complete; it never asks Spotify.
        let local = mirror(&["b"], "a");
        assert!(!local.wants_refill());
    }

    // ── Queue persistence ───────────────────────────────────────────────────
    fn pq(current: &str, upcoming: &[&str]) -> PersistedQueue {
        PersistedQueue {
            current_uri: current.to_string(),
            upcoming: upcoming.iter().map(|s| s.to_string()).collect(),
            queued: None,
        }
    }

    #[test]
    fn queue_restore_point_on_the_exact_captured_track() {
        // Still on the track it was captured behind: the whole list applies.
        let q = pq("a", &["b", "c", "d"]);
        assert_eq!(queue_restore_point(&q, "a"), Some(&["b".to_string(), "c".to_string(), "d".to_string()][..]));
    }

    #[test]
    fn only_a_known_free_plan_gets_the_notes() {
        assert!(plan_is_premium("premium"));
        assert!(plan_is_premium("Premium"));
        assert!(!plan_is_premium("free"));
        assert!(!plan_is_premium("open"));
        // No answer (an older token, a hiccup): no note.
        assert!(plan_is_premium(""));
    }

    #[test]
    fn the_players_account_must_be_premium_and_lightifys() {
        // Premium and the same account: nothing to say.
        assert!(streaming_account_problem("jon", "premium", "premium", "jon").is_none());
        assert!(streaming_account_problem("Jon", "premium", "premium", "jon").is_none());
        // Signed in through the browser as somebody else.
        let other = streaming_account_problem("flatmate", "premium", "premium", "jon").expect("other");
        assert!(other.contains("flatmate") && other.contains("jon"), "{other}");
        // Details the session didn't give are never a problem.
        assert!(streaming_account_problem("", "", "", "jon").is_none());
        assert!(streaming_account_problem("jon", "", "", "").is_none());
    }

    #[test]
    fn a_restarted_engine_gets_the_station_back_from_the_saved_queue() {
        // The station cleared the mirror; its songs are only in the saved queue. Three
        // songs in, the engine restarts on the third: the rest must follow it.
        let saved = pq("seed", &["s1", "s2", "s3", "s4", "s5"]);
        assert_eq!(restart_up_next("s3", None, Some(&saved)), vec!["s4".to_string(), "s5".to_string()]);
        // Still on the song the station was started from: all of it.
        assert_eq!(restart_up_next("seed", None, Some(&saved)).len(), 5);
        // Something the saved queue never covered: nothing stale is queued behind it.
        assert!(restart_up_next("other", None, Some(&saved)).is_empty());
        assert!(restart_up_next("s3", None, None).is_empty());
    }

    #[test]
    fn a_restarted_engine_prefers_the_live_mirror_about_this_song() {
        let t = |uri: &str| Track { uri: uri.into(), ..trk(uri, "", None) };
        let saved = pq("seed", &["s1", "s2"]);
        let live = QueueMirror::new("now".into(), vec![t("m1"), t("m2")]);
        assert_eq!(restart_up_next("now", Some(&live), Some(&saved)), vec!["m1".to_string(), "m2".to_string()]);
        // A mirror about another song is stale: the saved queue answers instead.
        assert_eq!(restart_up_next("s1", Some(&live), Some(&saved)), vec!["s2".to_string()]);
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
    fn drag_row_maps_content_y_to_rows() {
        // Uniform lists: row = y / height, clamped to the list.
        assert_eq!(drag_row(SEL_TRACKS, 0.0, 10), 0);
        assert_eq!(drag_row(SEL_TRACKS, 46.0 * 3.0 + 1.0, 10), 3);
        assert_eq!(drag_row(SEL_TRACKS, -50.0, 10), 0);
        assert_eq!(drag_row(SEL_TRACKS, 9999.0, 10), 9);
        assert_eq!(drag_row(SEL_SIDEBAR, 48.0 * 2.0 + 5.0, 10), 2);
        assert_eq!(drag_row(SEL_BEATPORT, 46.0 * 7.0, 10), 7);
        // Search mixes heights: header 36, top 96, hits 52.
        let mk = |kind: &str| SearchRow { name: "".into(), sub: "".into(), kind: kind.into(), selected: false, art: "".into(), meta: "".into() };
        let rows = vec![mk("top"), mk("header"), mk("track"), mk("track"), mk("header"), mk("artist")];
        let tops = search_row_tops(&rows);
        assert_eq!(tops, vec![0.0, 96.0, 132.0, 184.0, 236.0, 272.0]);
        *SEARCH_ROW_TOPS.lock().unwrap() = tops;
        assert_eq!(drag_row(SEL_SEARCH, 140.0, rows.len()), 2);
        assert_eq!(drag_row(SEL_SEARCH, 200.0, rows.len()), 3);
        assert_eq!(drag_row(SEL_SEARCH, 1000.0, rows.len()), 5);
    }

    #[test]
    fn drags_add_to_the_selection_and_skip_unpickable_rows() {
        let view: Vec<usize> = (0..20).collect();
        let none = std::collections::BTreeSet::new();
        // First group 2..=4, dragged upward to prove order doesn't matter.
        let first = drag_selection(SEL_TRACKS, &none, 4, 2, &view, |_| true);
        assert_eq!(first.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
        // A second drag 8..=9 keeps the first group: gaps allowed.
        let both = drag_selection(SEL_TRACKS, &first, 8, 9, &view, |_| true);
        assert_eq!(both.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4, 8, 9]);
        // A drag that shrinks back only ever re-derives from its own base.
        let shrunk = drag_selection(SEL_TRACKS, &first, 8, 8, &view, |_| true);
        assert_eq!(shrunk.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4, 8]);
        // The sorted drilled list maps display rows to source indices.
        let sorted = vec![5usize, 3, 9];
        let s = drag_selection(SEL_TRACKS, &none, 0, 1, &sorted, |_| true);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), vec![3, 5]);
        // Search: headers/artists in the range are never picked.
        let s = drag_selection(SEL_SEARCH, &none, 0, 5, &view, |r| r == 2 || r == 3);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
    }

    fn np_trk(id: &str) -> Track {
        Track {
            id: id.into(),
            name: format!("Song {id}"),
            artists: "A".into(),
            artist_ids: vec![],
            album: String::new(),
            album_id: String::new(),
            duration_ms: 200_000,
            uri: format!("spotify:track:{id}"),
            album_art: String::new(),
            added_at: None,
            is_playable: true,
        }
    }

    #[test]
    fn selections_are_not_capped_at_50() {
        let tracks: Vec<Track> = (0..200).map(|i| np_trk(&format!("t{i}"))).collect();
        let view: Vec<usize> = (0..200).collect();
        let sel: std::collections::BTreeSet<usize> = (0..200).collect();
        assert_eq!(super::selection_uris(&sel, &view, &tracks, false).len(), 200);
        let side_sel: std::collections::BTreeSet<usize> = (0..120).collect();
        let n = super::scoped_selection_uris(SEL_SIDEBAR, &side_sel, &[], &[], &[], &tracks, &[], true).len();
        assert_eq!(n, 120);
    }

    #[test]
    fn chart_placeholders_follow_the_matcher_and_hide_once_abandoned() {
        let src: Vec<lightify_core::beatport::BeatportTrack> = (0..10)
            .map(|k| lightify_core::beatport::BeatportTrack {
                rank: k + 1,
                name: format!("Row {k}"),
                artists: "X".into(),
                label: String::new(),
                isrc: String::new(),
            })
            .collect();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(9));
        super::set_bp_panel(Some(super::BpPanel {
            seq_id: 9,
            active: active.clone(),
            title: "t".into(),
            found: 1,
            total: 10,
            waiting: super::bp_waiting(&src, 1),
            next_index: 1,
            start: 0,
            done: false,
            missed: 0,
        }));
        let (rows, strip, found, total, note) = super::bp_panel_rows();
        assert_eq!(rows.len(), 9);
        assert_eq!((rows[0].pending, rows[1].pending), (1, 2));
        assert_eq!((strip.as_str(), found, total, note.as_str()), ("t", 1, 10, ""));
        // Rows 1..6 looked up, 4 of them found: placeholders move past them.
        super::bp_panel_progress(9, 5, 6, false);
        let (rows, _, found, _, _) = super::bp_panel_rows();
        assert_eq!((rows.len(), rows[0].name.as_str(), found), (4, "Row 6", 5));
        // To the end: no placeholders, one summary line counting the misses.
        super::bp_panel_progress(9, 8, 10, true);
        let (rows, strip, _, _, note) = super::bp_panel_rows();
        assert!(rows.is_empty() && strip.is_empty());
        assert!(note.starts_with("8 of 10") && note.contains("2 not on"), "{note}");
        // The user played something else: nothing of the chart shows any more.
        active.store(0, std::sync::atomic::Ordering::SeqCst);
        let (rows, strip, _, _, note) = super::bp_panel_rows();
        assert!(rows.is_empty() && strip.is_empty() && note.is_empty());
        super::set_bp_panel(None);
    }

    #[test]
    fn a_lagging_read_of_our_device_keeps_the_engine_track() {
        let pb = |id: &str, dev: &str| PlaybackState {
            is_playing: true,
            progress_ms: 90_000,
            duration_ms: 200_000,
            shuffle_state: false,
            repeat_state: "off".into(),
            volume_percent: 50,
            device_name: "Lightify".into(),
            device_id: dev.into(),
            track: Some(np_trk(id)),
            context_uri: None,
        };
        let mut new = np_trk("new");
        new.duration_ms = 123_000;
        super::set_engine_now("ours", Some(new));
        let last = Some(PlaybackState { progress_ms: 4_000, ..pb("new", "ours") });
        // Still the previous song per the Web API: replaced, position kept from `last`.
        let mut stale = pb("old", "ours");
        assert!(super::engine_now_override(&mut stale, &last));
        assert_eq!(stale.track.as_ref().map(|t| t.id.as_str()), Some("new"));
        assert_eq!((stale.duration_ms, stale.progress_ms), (123_000, 4_000));
        // Caught up: untouched.
        let mut fresh = pb("new", "ours");
        assert!(!super::engine_now_override(&mut fresh, &last));
        assert_eq!(fresh.progress_ms, 90_000);
        // Another device is playing: the engine's track is not the truth there.
        let mut elsewhere = pb("other", "phone");
        assert!(!super::engine_now_override(&mut elsewhere, &last));
        assert_eq!(elsewhere.track.as_ref().map(|t| t.id.as_str()), Some("other"));
        super::set_engine_now("", None);
    }

    #[test]
    fn menu_is_at_least_the_css_min_width_and_grows() {
        // The menu's minimum (176px in the redesign) — short menus sit exactly there…
        assert_eq!(menu_width(&search_album_menu(0)), 176.0);
        // …and a long label pushes past it rather than eliding.
        assert!(menu_width(&selection_menu(3)) > 176.0);
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


    // ── Batch 6 (2026-11-12) ────────────────────────────────────────────────
    #[test]
    fn a_local_song_just_started_is_not_stopped_by_a_late_spotify_play() {
        use std::time::{Duration, Instant};
        assert!(super::spotify_play_stops_local(None));
        assert!(!super::spotify_play_stops_local(Some(Instant::now())));
        let long_ago = Instant::now().checked_sub(super::LOCAL_START_GRACE + Duration::from_millis(10));
        if let Some(t) = long_ago {
            assert!(super::spotify_play_stops_local(Some(t)));
        }
    }

    #[test]
    fn a_context_continues_in_source_order_only() {
        assert!(super::view_is_source_order(&[0, 1, 2, 3]));
        assert!(super::view_is_source_order(&[]));
        assert!(!super::view_is_source_order(&[2, 0, 1]), "re-sorted");
        assert!(!super::view_is_source_order(&[0, 2, 3]), "filtered");
    }

    #[test]
    fn a_failed_like_check_forgets_the_previous_song() {
        let mut liked = super::LikeState { id: "A".into(), on: true, failed: None };
        super::like_check_failed(&mut liked, "B".into(), std::time::Instant::now());
        assert!(liked.id.is_empty() && !liked.on);
        // A comes round again: asked about, not shown as un-liked from B's failure.
        assert!("A" != liked.id && super::like_check_due(&liked, "A"));
        assert!(!super::like_check_due(&liked, "B"), "B waits out the retry gap");
    }

    #[test]
    fn an_expired_options_hold_is_not_rearmed() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut slot = None;
        super::arm_options_hold(&mut slot, Some(true), None, t0);
        assert!(slot.as_ref().is_some_and(|h| h.shuffle == Some(true)));
        // Long after, a repeat press holds repeat only.
        let later = t0 + super::OPTIONS_HOLD_FOR + Duration::from_secs(1);
        super::arm_options_hold(&mut slot, None, Some("context".into()), later);
        let h = slot.as_ref().expect("a hold");
        assert_eq!(h.shuffle, None, "the stale shuffle hold is gone");
        assert_eq!(h.repeat.as_deref(), Some("context"));
        // Within the hold, both stand.
        super::arm_options_hold(&mut slot, Some(false), None, later);
        let h = slot.as_ref().expect("a hold");
        assert_eq!((h.shuffle, h.repeat.as_deref()), (Some(false), Some("context")));
    }

    #[test]
    fn shuffle_for_a_load_follows_what_plays() {
        let mut p = PlaybackState {
            is_playing: false,
            progress_ms: 0,
            duration_ms: 0,
            shuffle_state: true,
            repeat_state: "off".into(),
            volume_percent: 50,
            device_name: String::new(),
            device_id: String::new(),
            track: None,
            context_uri: None,
        };
        assert!(super::load_shuffle(&Some(p.clone())));
        p.shuffle_state = false;
        assert!(!super::load_shuffle(&Some(p)));
    }

    #[test]
    fn follower_counts_never_read_1000k() {
        assert_eq!(super::compact_count(999_949), "999.9K");
        assert_eq!(super::compact_count(999_950), "1M");
        assert_eq!(super::compact_count(999_999), "1M");
        assert_eq!(super::compact_count(45_600), "45.6K");
    }

    #[test]
    fn a_page_that_only_appends_keeps_the_rows() {
        assert!(super::rows_extend(&[1, 2], &[1, 2, 3]));
        assert!(super::rows_extend(&[1, 2], &[1, 2]));
        assert!(super::rows_extend::<i32>(&[], &[1]));
        assert!(!super::rows_extend(&[1, 2], &[2, 1, 3]), "moved");
        assert!(!super::rows_extend(&[1, 2, 3], &[1, 2]), "shorter");
    }

    #[test]
    fn marks_carry_over_to_the_same_rows_only() {
        let shown = vec![("a", true), ("b", false), ("c", true)];
        let mut rows = vec![("a", false), ("b", false), ("x", false), ("d", false)];
        super::carry_marks(&shown, &mut rows, |r| r.1, |r, on| r.1 = on, |a, b| a.0 == b.0);
        assert_eq!(rows, vec![("a", true), ("b", false), ("x", false), ("d", false)]);
    }

    #[test]
    fn a_new_play_lists_the_hand_queued_songs_first() {
        // QD6: H1 and H2 were queued by hand; a new list plays, and they still come first.
        let mut old = QueueMirror::new("spotify:track:a".into(), vec![trk("h1", "x", None), trk("h2", "x", None), trk("b", "x", None)]);
        old.queued = 2;
        let m = QueueMirror::new("spotify:track:new".into(), vec![trk("n1", "x", None)]).keeping_queued(Some(&old));
        let up: Vec<&str> = m.upcoming.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(up, ["h1", "h2", "n1"]);
        assert_eq!(m.queued, 2, "still counted as queued");
        // Nothing queued by hand: the new list alone.
        let plain = QueueMirror::new("spotify:track:new".into(), vec![trk("n1", "x", None)]).keeping_queued(None);
        assert_eq!((plain.upcoming.len(), plain.queued), (1, 0));
    }

    #[test]
    fn the_previous_track_is_stale_only_until_the_play_is_confirmed() {
        let mut m = QueueMirror::new("spotify:track:new".into(), vec![trk("next", "x", None)]).replacing("spotify:track:old");
        // Right after the play, a lagging read of the old track keeps the mirror.
        assert!(m.advance("spotify:track:old"));
        assert_eq!(m.upcoming.len(), 1);
        // Once the new track is confirmed, the old one playing again is a real change.
        assert!(m.advance("spotify:track:new"));
        assert!(!m.advance("spotify:track:old"));
    }

    #[test]
    fn being_offline_or_rate_limited_is_no_strike_against_a_chart_row() {
        // Next index 5 means row 4 just failed.
        assert_eq!(super::bp_retry_row_after(5, 0, "Rate limited by Spotify — retry in 30s"), (4, 0));
        assert_eq!(super::bp_retry_row_after(5, 2, "Rate limited by Spotify — retry in 30s"), (4, 2));
        // Anything else counts, and the third strike skips the row.
        assert_eq!(super::bp_retry_row_after(5, 0, "HTTP 500"), (4, 1));
        assert_eq!(super::bp_retry_row_after(5, 2, "HTTP 500"), (5, 0));
    }

    #[test]
    fn the_sign_in_page_still_drives_the_engine() {
        assert_eq!(super::signin_transport(&Cmd::TogglePlay, Some(true)), Some(r#"{"cmd":"pause"}"#));
        assert_eq!(super::signin_transport(&Cmd::TogglePlay, Some(false)), Some(r#"{"cmd":"play"}"#));
        assert_eq!(super::signin_transport(&Cmd::TogglePlay, None), Some(r#"{"cmd":"play"}"#));
        assert_eq!(super::signin_transport(&Cmd::Next, None), Some(r#"{"cmd":"next"}"#));
        assert_eq!(super::signin_transport(&Cmd::Prev, None), Some(r#"{"cmd":"prev"}"#));
        assert_eq!(super::signin_transport(&Cmd::Back, None), None);
    }

    #[test]
    fn retry_with_an_edited_client_id_starts_over() {
        let a = "0123456789abcdef0123456789abcdef";
        let b = "fedcba9876543210fedcba9876543210";
        assert!(super::signin_restarts(a, b));
        assert!(!super::signin_restarts(a, &a.to_uppercase()), "the same id, as typed");
        assert!(!super::signin_restarts(a, "not-an-id"), "a half-typed id keeps the request");
    }

    #[test]
    fn a_ready_update_is_not_replaced_by_a_check() {
        assert!(!super::update_result_applies("ready", "current"));
        assert!(!super::update_result_applies("ready", "available"));
        assert!(!super::update_result_applies("installing", "error"));
        assert!(super::update_result_applies("ready", "ready"));
        assert!(super::update_result_applies("checking", "current"));
    }

    #[test]
    fn the_palette_lists_playlists_a_library_filter_hides() {
        let pls = vec![pl("a", "Alpha"), pl("b", "Beta"), pl("c", "Gamma")];
        let all = super::playlist_order(&pls, "recent", "asc", "");
        assert_eq!(all, vec![0, 1, 2]);
        assert_eq!(super::playlist_order(&pls, "recent", "asc", "bet"), vec![1]);
        let cmds = super::build_palette_base(&pls, &all, false);
        let ids: Vec<String> = cmds
            .iter()
            .filter_map(|c| match &c.cmd {
                Cmd::DrillId(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
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
        // Another PC's Lightify shares our name but not our id: that is a takeover.
        let mut other_pc = dev("Lightify", true);
        other_pc.id = "another-pc".into();
        assert!(another_device_active(&[dev("Lightify", false), other_pc], "Lightify"));
    }

    #[test]
    fn launch_leaves_a_playing_device_alone() {
        let ours = dev("ours", false);
        let now = |playing: bool, device: &str| -> Result<Option<PlaybackState>, String> {
            Ok(Some(PlaybackState {
                is_playing: playing,
                progress_ms: 0,
                duration_ms: 0,
                shuffle_state: false,
                repeat_state: "off".into(),
                volume_percent: 50,
                device_name: device.into(),
                device_id: device.into(),
                track: None,
                context_uri: None,
            }))
        };
        // Music on the phone stays on the phone (Q2).
        assert!(!super::launch_takes_playback(&ours, &now(true, "phone")));
        // Nothing playing anywhere, or only paused elsewhere: Lightify takes over.
        assert!(super::launch_takes_playback(&ours, &Ok(None)));
        assert!(super::launch_takes_playback(&ours, &now(false, "phone")));
        // Not knowing is not a yes; already ours needs no transfer (it answers 500).
        assert!(!super::launch_takes_playback(&ours, &Err("Rate limited by Spotify — retry in 5s".into())));
        assert!(!super::launch_takes_playback(&dev("ours", true), &Ok(None)));
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

    #[test]
    fn an_engine_that_dies_right_after_ready_uses_up_the_budget() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut w = super::EngineWatchdog::new();
        // Ready, then dead a second later, again and again: the budget runs out.
        let mut at = t0;
        let mut restarts = 0;
        for _ in 0..10 {
            w.note_ready(at);
            at += Duration::from_secs(1);
            if !w.should_restart(at) {
                break;
            }
            restarts += 1;
        }
        assert_eq!(restarts, super::EngineWatchdog::MAX_RESTARTS);
        // One that stayed up a while is healthy again: a fresh budget.
        w.note_ready(at);
        assert!(w.should_restart(at + super::EngineWatchdog::HEALTHY_AFTER));
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
        use super::super::{
            accept_device_options, apply_options_hold, next_repeat_mode, release_options_hold, OptionsHold,
            OPTIONS_HOLD,
        };
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
                shuffle_confirmed: false,
                repeat_confirmed: false,
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
                shuffle_confirmed: false,
                repeat_confirmed: false,
            });
            let mut p = pb(false, "off");
            apply_options_hold(&mut p);
            assert!(!p.shuffle_state);
            release_options_hold();

            // ── Our engine's own reports (same global hold, so same test) ──
            let hold = |repeat: &str| {
                *OPTIONS_HOLD.lock().unwrap() = Some(OptionsHold {
                    shuffle: None,
                    repeat: Some(repeat.into()),
                    until: std::time::Instant::now() + std::time::Duration::from_secs(6),
                    shuffle_confirmed: false,
                    repeat_confirmed: false,
                });
            };
            // No hold: the device is the authority.
            assert_eq!(accept_device_options(None, Some("context".into())), (None, Some("context".into())));
            // Repeat-one → off: librespot reports "repeat-one off" (= repeat-all) on the
            // way. That step must not flip the button back to repeat-all...
            hold("off");
            assert_eq!(accept_device_options(None, Some("context".into())), (None, None));
            // ...the final report confirms the choice and is shown...
            assert_eq!(accept_device_options(None, Some("off".into())), (None, Some("off".into())));
            // ...and the hold survives it, so a lagging poll still can't undo it.
            let mut p = pb(false, "track");
            apply_options_hold(&mut p);
            assert_eq!(p.repeat_state, "off");
            // A different report after confirmation is a real change from elsewhere.
            hold("track");
            assert_eq!(accept_device_options(None, Some("track".into())), (None, Some("track".into())));
            assert_eq!(accept_device_options(None, Some("context".into())), (None, Some("context".into())));
            assert!(OPTIONS_HOLD.lock().unwrap().is_none());
            // A held repeat doesn't swallow a shuffle report.
            hold("context");
            assert_eq!(accept_device_options(Some(true), None), (Some(true), None));
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

    // ── DESKTOP-BUGS-PLAN Batch 6 (M1-M20) ──────────────────────────────────
    #[test]
    fn only_a_refused_refresh_token_needs_the_sign_in_page() {
        // M1: Spotify said `invalid_grant` — sign in again.
        let revoked = format!("{}Refresh token revoked", lightify_core::spotify::TOKEN_REVOKED_PREFIX);
        assert!(super::refresh_failure_notice(&revoked).is_some());
        // Offline, or the token service failing: start on what is cached and retry.
        let offline = format!("{}Token refresh: dns error", lightify_core::net::OFFLINE_PREFIX);
        assert!(super::refresh_failure_notice(&offline).is_none());
        assert!(super::refresh_failure_notice("Token refresh failed (503): upstream").is_none());
        // A revocation seen mid-session is acted on once.
        super::note_revoked_sign_in();
        assert!(super::take_revoked_sign_in());
        assert!(!super::take_revoked_sign_in());
    }

    #[test]
    fn engine_device_is_found_by_its_id_only() {
        // M2: the phone app and a previous run's session are "Lightify" too.
        let mut phone = dev("Lightify", true);
        phone.id = "phone-id".into();
        let mut ours = dev("Lightify", false);
        ours.id = "our-id".into();
        let devices = [phone, ours];
        assert_eq!(super::engine_device_in(&devices, "our-id").map(|d| d.id.as_str()), Some("our-id"));
        // Not listed yet: keep waiting rather than adopt the phone.
        assert!(super::engine_device_in(&devices[..1], "our-id").is_none());
        assert!(super::engine_device_in(&devices, "").is_none());
    }

    #[test]
    fn only_the_downloader_taking_over_is_reclaimed() {
        // M3: "OnTheSpot" active is our downloader sign-in; a phone is the user's choice.
        assert!(super::downloader_active(&[dev("Lightify", false), dev("OnTheSpot", true)]));
        assert!(!super::downloader_active(&[dev("Lightify", false), dev("Jon\u{2019}s iPhone", true)]));
        assert!(!super::downloader_active(&[dev("OnTheSpot", false)]));
    }

    #[test]
    fn shift_click_extends_from_the_same_track_after_a_re_sort() {
        // M5: the anchor is a track index; after a re-sort it is on another row.
        let view = vec![3, 0, 2, 1];
        assert_eq!(super::anchor_row(SEL_TRACKS, 2, &view), Some(2));
        let resorted = vec![2, 1, 0, 3];
        assert_eq!(super::anchor_row(SEL_TRACKS, 2, &resorted), Some(0));
        // Filtered out of view: no row (the click itself becomes the start).
        assert_eq!(super::anchor_row(SEL_TRACKS, 2, &[0, 1]), None);
        // Unsorted lists: the key is the row.
        assert_eq!(super::anchor_row(SEL_SEARCH, 5, &[]), Some(5));
        // M8: keys round-trip through sel_key.
        let k = sel_key(SEL_TRACKS, 1, &view).unwrap();
        assert_eq!(super::anchor_row(SEL_TRACKS, k, &view), Some(1));
    }

    #[test]
    fn restore_inside_liked_songs_only_puts_back_the_hand_queued_head() {
        // M9: Liked Songs carries its own next tracks, like a playlist does.
        let mut q = pq("a", &["h1", "h2", "c1", "c2"]);
        q.queued = Some(2);
        let liked = Some("spotify:user:jon:collection");
        assert_eq!(super::queue_to_restore(&q, "a", liked), &["h1".to_string(), "h2".to_string()][..]);
        assert_eq!(super::queue_to_restore(&q, "a", Some("spotify:playlist:p")).len(), 2);
        // A plain uri list has no next tracks of its own: everything goes back.
        assert_eq!(super::queue_to_restore(&q, "a", None).len(), 4);
        // Advanced past one hand-queued track: only the other one is left to restore.
        assert_eq!(super::queue_to_restore(&q, "h1", liked), &["h2".to_string()][..]);
        assert!(super::queue_to_restore(&q, "elsewhere", liked).is_empty());
    }

    #[test]
    fn a_restored_queue_keeps_its_hand_queued_count() {
        // M12: new additions land after the restored ones, and the window tops up.
        let t = vec![menu_trk("h1", true), menu_trk("h2", true), menu_trk("c1", true)];
        let mut m = super::restored_mirror("spotify:track:a".into(), t, 2);
        assert!(m.from_spotify);
        assert_eq!(m.queued, 2);
        m.enqueue(menu_trk("new", true));
        let order: Vec<&str> = m.upcoming.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, ["h1", "h2", "new", "c1"]);
        // More restored than Spotify listed: capped to what is there.
        assert_eq!(super::restored_mirror("a".into(), vec![menu_trk("h1", true)], 5).queued, 1);
    }

    #[test]
    fn hand_queued_rows_are_counted_from_the_front_of_spotifys_queue() {
        // M10: Spotify lists the user queue first, then the context.
        let q = vec![menu_trk("h1", true), menu_trk("h2", true), menu_trk("c1", true), menu_trk("h3", true)];
        let hand = |ids: &[&str]| ids.iter().map(|i| format!("spotify:track:{i}")).collect::<Vec<_>>();
        assert_eq!(super::queued_head(&hand(&["h2", "h1", "h3"]), &q), 2);
        // A context track in front: nothing ahead of the context was queued by hand.
        assert_eq!(super::queued_head(&hand(&["h1"]), &q[2..]), 0);
        // Each queued copy counts once: one "h1" queued, two listed.
        let twice = vec![menu_trk("h1", true), menu_trk("h1", true)];
        assert_eq!(super::queued_head(&hand(&["h1"]), &twice), 1);
    }

    #[test]
    fn hand_queued_rows_past_spotifys_window_are_kept() {
        // M10: 3 listed (all hand-queued), 2 more waiting behind them, unlisted.
        let hand = |ids: &[&str]| ids.iter().map(|i| format!("spotify:track:{i}")).collect::<Vec<_>>();
        let window = vec![menu_trk("h2", true), menu_trk("h3", true), menu_trk("h4", true)];
        let mut g = hand(&["h1", "h2", "h3", "h4", "h5", "h6"]);
        super::forget_unqueued(&mut g, &window);
        // h1 played; h5 and h6 stay for when the window reaches them.
        assert_eq!(g, hand(&["h2", "h3", "h4", "h5", "h6"]));
        let later = vec![menu_trk("h5", true), menu_trk("h6", true), menu_trk("c1", true)];
        super::forget_unqueued(&mut g, &later);
        assert_eq!(super::queued_head(&g, &later), 2);
        // A window that reaches the context lists every hand-queued row: the rest are gone.
        let mut g = hand(&["h1", "h2", "h9"]);
        let q = vec![menu_trk("h1", true), menu_trk("c1", true)];
        super::forget_unqueued(&mut g, &q);
        assert_eq!(g, hand(&["h1"]));
        // Nothing listed: nothing kept.
        let mut g = hand(&["h1"]);
        super::forget_unqueued(&mut g, &[]);
        assert!(g.is_empty());
    }

    #[test]
    fn the_exit_resume_point_runs_on_while_playing() {
        // M19: saved on exit at where the music got to, never past the end.
        let rp = super::ResumePoint {
            track_uri: "spotify:track:t1".into(),
            context_uri: None,
            position_ms: 10_000,
            duration_ms: 60_000,
            name: "t1".into(),
            artists: String::new(),
            album_art: String::new(),
        };
        let mut pb = super::resume_as_playback(&rp);
        assert_eq!(super::exit_resume_point(pb.clone(), std::time::Duration::from_secs(30)).progress_ms, 10_000);
        pb.is_playing = true;
        assert_eq!(super::exit_resume_point(pb.clone(), std::time::Duration::from_secs(30)).progress_ms, 40_000);
        assert_eq!(super::exit_resume_point(pb, std::time::Duration::from_secs(300)).progress_ms, 60_000);
    }

    #[test]
    fn a_failed_chart_row_is_retried_then_given_up() {
        // M25: `next_index` has moved past the failing row (row 4 here).
        assert_eq!(super::bp_retry_row(5, 0), (4, 1));
        assert_eq!(super::bp_retry_row(5, 1), (4, 2));
        // The third failure in a row moves on, and the next row starts afresh.
        assert_eq!(super::bp_retry_row(5, 2), (5, 0));
    }

    #[test]
    fn a_search_filter_resumes_its_own_paging() {
        // M27: tracks paged twice, then playlists, then back to tracks.
        let mut paged = std::collections::HashMap::new();
        paged.insert("track", (30u32, false));
        assert_eq!(super::resumed_search_pager(&paged, "track", (10, false)), (30, false));
        assert_eq!(super::resumed_search_pager(&paged, "playlist", (10, false)), (10, false));
        paged.insert("album", (20, true));
        assert_eq!(super::resumed_search_pager(&paged, "album", (10, false)), (20, true));
        // The All view never pages.
        assert_eq!(super::resumed_search_pager(&paged, "", (10, true)), (10, true));
    }

    #[test]
    fn the_filter_pill_says_how_much_is_loaded_while_paging() {
        // M28: matches come from the loaded pages only; say so until the list is whole.
        let mut p = super::TrackPager::start(super::PagerSource::Liked, "Liked Songs");
        p.total = 900;
        assert_eq!(super::filter_loaded_note(&p, 300), "300 / 900 loaded");
        p.total = 0;
        assert_eq!(super::filter_loaded_note(&p, 50), "50 loaded");
        p.done = true;
        assert!(super::filter_loaded_note(&p, 900).is_empty());
        assert!(super::filter_loaded_note(&super::TrackPager::default(), 0).is_empty());
    }

    #[test]
    fn a_crashed_engines_resume_note_expires() {
        // M14: a much later `Ready` must not start the old track again.
        assert!(super::restart_resume_due(std::time::Duration::from_secs(5)));
        assert!(!super::restart_resume_due(super::RESUME_AFTER_RESTART_FOR));
    }

    #[test]
    fn a_failed_like_check_is_retried_not_kept() {
        // M18: a failure is held back only briefly, and only for that track.
        let fresh = super::LikeState::default();
        assert!(super::like_check_due(&fresh, "t1"));
        let failed = super::LikeState { failed: Some(("t1".into(), std::time::Instant::now())), ..Default::default() };
        assert!(!super::like_check_due(&failed, "t1"));
        assert!(super::like_check_due(&failed, "t2"));
        let old = std::time::Instant::now() - super::LIKE_RETRY_AFTER;
        let expired = super::LikeState { failed: Some(("t1".into(), old)), ..Default::default() };
        assert!(super::like_check_due(&expired, "t1"));
    }

    #[test]
    fn the_resume_point_is_saved_at_the_bars_position() {
        // M19: between reads, the position the bar shows (not the last read's).
        let rp = super::ResumePoint {
            track_uri: "spotify:track:t1".into(),
            context_uri: None,
            position_ms: 1_000,
            duration_ms: 200_000,
            name: "t1".into(),
            artists: String::new(),
            album_art: String::new(),
        };
        let last = Some(super::resume_as_playback(&rp));
        let clock = super::ProgressClock {
            anchor_at: Some(tokio::time::Instant::now()),
            track: "t1".into(),
            pos_ms: 95_000,
            ..Default::default()
        };
        assert_eq!(clock.live(&last).map(|p| p.progress_ms), Some(95_000));
        // The clock on another track (or not running): the read's own position.
        let other = super::ProgressClock { track: "t2".into(), ..clock };
        assert_eq!(other.live(&last).map(|p| p.progress_ms), Some(1_000));
        assert!(super::ProgressClock::default().live(&None).is_none());
    }
}
