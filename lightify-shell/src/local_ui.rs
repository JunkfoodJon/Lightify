//! Local library mode: the controller between the `LocalView` UI (app.slint), the
//! library index (`local_library`) and the local player (`local_player`).
//!
//! Lives in the worker: every UI action arrives as `Cmd::Local(LocalCmd)` and is
//! handled by `handle`, which owns `LocalState`. Anything slow (scanning, finding
//! music, decoding a cover, the native file pickers) runs on a thread of its own and
//! reports back as another `LocalCmd`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::local_library::{self, Candidate, LocalPlaylist, LocalTrack};
use crate::local_player;
use crate::{CtxItem, Cmd, LocalCand, LocalNode, LocalRow, MainWindow};
use lightify_i18n::tr;
use slint::ComponentHandle;

#[derive(Clone)]
pub enum LocalCmd {
    ModeChanged(bool),
    Scanned(Vec<LocalTrack>),
    ScanProgress(usize),
    NodeClick(usize),
    /// Row clicks carry the track the clicked row showed (`UI_ROWS`), so a list rebuilt
    /// before the worker reads them (a scan finishing) can't turn them onto another
    /// track; `None` only when the UI had no row there.
    RowActivate(usize, Option<PathBuf>),
    RowSelect(usize, bool, bool, Option<PathBuf>),
    ClearSel,
    Sort(i32),
    Rescan,
    Filter(String),
    TogglePlay,
    Prev,
    Next,
    Seek(f32),
    Volume(f32),
    ToggleShuffle,
    CycleRepeat,
    CarryBegin(usize, Option<PathBuf>),
    CarryEnd(i32),
    // Hold-to-select drag over the track list (display rows).
    DragBegin(usize),
    DragTo(usize),
    DragEnd,
    // Library filters
    FltMode(i32),
    /// A length field was committed: (the "longest" field?, its text).
    FltLen(bool, String),
    FltReset,
    FltType(usize),
    // The Local queue (the Queue drawer in Local mode): rows of `queue_rows`.
    QueuePlay(usize),
    QueueRemove(usize),
    QueueClear,
    // RECENT in Local mode: rows of `local_recent_rows`; refresh when the drawer opens
    // (its "5m" times move on).
    RecentPlay(usize),
    RecentRefresh,
    /// Whether history entries outside the library still exist, looked up on a thread
    /// of its own (a dead network share must not stall the worker).
    RecentChecked(Vec<(PathBuf, bool)>),
    // Explorer drag-over / drop (only acted on in Local mode)
    DropHover(Vec<PathBuf>),
    DropCancel,
    Dropped(Vec<PathBuf>),
    // Find music on this PC
    Find,
    FindFound(Vec<Candidate>),
    FindToggle(usize),
    FindAddFolder,
    FindAddFiles,
    Picked(Vec<PathBuf>),
    FindConfirm,
    FindClose,
    // Context-menu actions. They run after the menu closes, by when a scan or a track
    // change may have rebuilt the rows, so each carries what was right-clicked (paths,
    // ids, positions checked against their path), never a bare row number.
    /// (the row right-clicked, the tracks the menu was for: the selection or that row)
    MenuPlay(usize, Vec<PathBuf>),
    MenuQueue(Vec<PathBuf>),
    MenuAddTo(Vec<PathBuf>, String),
    MenuNewPlaylist(Vec<PathBuf>),
    ShowInFolder(PathBuf),
    /// (playlist id, its entries by position, each with the path it held)
    MenuRemoveFromPlaylist(String, Vec<(usize, PathBuf)>),
    QueueItemPlay(QSrc, PathBuf),
    QueueItemRemove(QSrc, PathBuf),
    NodePlay(View),
    NodeRename(String),
    /// "Delete playlist": asks first (the confirm dialog Spotify's delete uses).
    NodeDelete(String),
    /// The confirm dialog's Yes for the delete it was opened for.
    DeleteConfirmed,
    NodeNewPlaylist,
    NodeRemoveFolder(PathBuf),
    NodeShowInFolder(PathBuf),
    /// The name prompt answered (`Some`) or was cancelled (`None`).
    Named(Option<String>),
    /// A player event, with the generation of the `play` it belongs to.
    Player(u64, local_player::Event),
    Cover(PathBuf, Option<(Vec<u8>, u32, u32, slint::Color)>),
    /// The language changed: everything this view built in Rust, said again.
    Relabel,
}

/// What the track list shows.
#[derive(Clone, PartialEq)]
pub enum View {
    All,
    Artist(String),
    Album(String, String),
    Genre(String),
    Folder(PathBuf),
    Recent,
    Playlist(String),
}

/// What a section of the tree is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Artists,
    Albums,
    Genres,
    Folders,
    Playlists,
}

/// One tree row, and what clicking it does.
#[derive(Clone)]
enum NodeAct {
    View(View),
    Toggle(Section),
    NewPlaylist,
}

/// Where a Queue drawer row comes from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum QSrc {
    Current,
    User(usize),
    Order(usize),
}

/// Where the file `set_next` last gave the player comes from, so the gapless handoff
/// is followed by position: a list holding one file twice, or a queued copy of the
/// playing file, then moves on instead of looping.
#[derive(Clone, Copy, PartialEq, Debug)]
enum NextSrc {
    /// The front of the user queue.
    Queue,
    /// This place in the order.
    Order(usize),
    /// Repeat one: the same file again.
    Same,
}

/// Unplayable files in a row skipped before playback gives up.
const MAX_FAILS: u32 = 5;

/// Library filters (saved): what the library views leave out. Playlists are the
/// user's own picks and always show everything.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Filters {
    /// 0 all, 1 music only (≥ 1:00), 2 long sets (≥ 20:00)
    #[serde(default)]
    mode: i32,
    /// The old "hide shorter than" chip (an index into `OLD_MIN_SECS`); read once to
    /// carry a saved choice over to `min_s`.
    #[serde(default, skip_serializing)]
    min: i32,
    /// Hide tracks shorter / longer than this many seconds (0 = no limit).
    #[serde(default)]
    min_s: u64,
    #[serde(default)]
    max_s: u64,
    /// File types switched off ("mp3", "flac", …)
    #[serde(default)]
    hidden_types: Vec<String>,
}

const OLD_MIN_SECS: [u64; 5] = [0, 5, 15, 30, 60];
/// The order type chips come in; anything else follows alphabetically.
const TYPE_ORDER: [&str; 11] = ["mp3", "flac", "wav", "aiff", "m4a", "aac", "ogg", "m4b", "caf", "mp2", "mp1"];

impl Filters {
    fn min_ms(&self) -> u64 {
        let mode = match self.mode {
            1 => 60_000,
            2 => 20 * 60_000,
            _ => 0,
        };
        (self.min_s * 1000).max(mode)
    }
    /// A track whose length couldn't be read (0) is never hidden for its length.
    fn passes(&self, t: &LocalTrack) -> bool {
        let (min, max) = (self.min_ms(), self.max_s * 1000);
        let len_ok = t.duration_ms == 0 || (t.duration_ms >= min && (max == 0 || t.duration_ms <= max));
        len_ok && !self.hidden_types.iter().any(|h| *h == ext_of(&t.path))
    }
    fn is_default(&self) -> bool {
        self.mode == 0 && self.min_s == 0 && self.max_s == 0 && self.hidden_types.is_empty()
    }
}

/// A file's type for the filter chips: its extension, with spellings of one format
/// folded together (.oga is OGG, .aif / .aifc are AIFF).
fn ext_of(p: &Path) -> String {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match e.as_str() {
        "oga" => "ogg".to_string(),
        "aif" | "aifc" => "aiff".to_string(),
        _ => e,
    }
}

/// A typed length: "90", "1:30", "1:02:00", "90s", "5m", "1h" (seconds). Empty is "no
/// limit" (0). `None` when it can't be read.
fn parse_len(text: &str) -> Option<u64> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return Some(0);
    }
    if t.contains(':') {
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() > 3 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
            return None;
        }
        let nums: Vec<u64> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
        // Minutes / seconds after the first group stay under 60.
        if nums[1..].iter().any(|&n| n >= 60) {
            return None;
        }
        return Some(nums.iter().fold(0, |acc, n| acc * 60 + n));
    }
    let (num, unit) = match t.char_indices().find(|(_, c)| !c.is_ascii_digit() && *c != '.') {
        Some((i, _)) => (&t[..i], t[i..].trim()),
        None => (t.as_str(), ""),
    };
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "" | "s" | "sec" | "secs" => 1.0,
        "m" | "min" | "mins" => 60.0,
        "h" | "hr" | "hrs" => 3600.0,
        _ => return None,
    };
    let secs = (n * mult).round();
    (secs >= 0.0 && secs < 1e7).then_some(secs as u64)
}

/// How a limit shows in its field: "0:30", "4:05", "1:02:00"; empty for none.
fn fmt_len(secs: u64) -> String {
    match secs {
        0 => String::new(),
        s if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        s => format!("{}:{:02}", s / 60, s % 60),
    }
}

fn filters_path() -> PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_local_filters.json")
}

fn load_filters() -> Filters {
    let mut f: Filters =
        std::fs::read_to_string(filters_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if f.min_s == 0 {
        f.min_s = OLD_MIN_SECS.get(f.min.max(0) as usize).copied().unwrap_or(0);
    }
    f.min = 0;
    f
}

fn save_filters(f: &Filters) {
    if let Ok(s) = serde_json::to_string_pretty(f) {
        let _ = lightify_core::config::write_atomic(&filters_path(), s.as_bytes());
    }
}

fn volume_path() -> PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_local_volume.txt")
}

/// The Local volume as last set (0.8 the first time).
fn load_volume() -> f32 {
    std::fs::read_to_string(volume_path())
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map_or(0.8, |v| v.clamp(0.0, 1.0))
}

fn save_volume(v: f32) {
    let _ = lightify_core::config::write_atomic(&volume_path(), format!("{v:.3}").as_bytes());
}

/// A name the prompt was opened for.
enum Pending {
    NewPlaylist(Vec<PathBuf>),
    Rename(String),
}

pub struct LocalState {
    on: bool,
    tracks: Vec<LocalTrack>,
    playlists: Vec<LocalPlaylist>,
    folders: Vec<PathBuf>,
    scanning: Option<usize>,
    /// A scan was asked for while one ran (the sources changed under it): run another
    /// once it ends, and don't show the one that ran.
    rescan: bool,
    scanned_once: bool,
    view: View,
    open: BTreeSet<Section>,
    filter: String,
    sort: (i32, bool),
    /// The track list as shown: indices into `tracks`.
    shown: Vec<usize>,
    /// For a playlist view, the playlist positions behind `shown` (same order).
    shown_pl: Vec<usize>,
    /// The tree as shown, and what each row does.
    nodes: Vec<(LocalNode, NodeAct, Option<String>)>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    /// A live hold-to-select drag: the selection before it, and the row it began on.
    drag: Option<(BTreeSet<usize>, usize)>,
    carry: Vec<PathBuf>,
    filters: Filters,
    /// What the Queue drawer shows, row by row.
    qview: Vec<(QSrc, PathBuf)>,
    // playback
    order: Vec<PathBuf>,
    /// The list playback was started from, in its own order (`order` is it shuffled).
    /// Shuffle off goes back to this, not to whatever list is on screen by then.
    source: Vec<PathBuf>,
    pos: usize,
    user_queue: VecDeque<PathBuf>,
    now: Option<PathBuf>,
    now_ms: u64,
    dur_ms: u64,
    playing: bool,
    shuffle: bool,
    repeat: i32,
    volume: f32,
    /// The generation of the last `local_player::play`: events of older ones are stale.
    play_gen: u64,
    /// What `set_next` last gave the player, and where it comes from.
    next_src: Option<(NextSrc, PathBuf)>,
    /// Unplayable files skipped in a row, and the first position reported since the
    /// last track started (a later one means audio is really playing).
    fails: u32,
    first_pos: Option<u64>,
    // find music
    cands: Vec<Candidate>,
    cand_on: Vec<bool>,
    /// The Find dialog is open (its results may come after it was closed).
    find_open: bool,
    pending: Option<Pending>,
    /// The playlist id the delete confirmation is open for.
    delete_pending: Option<String>,
    // RECENT: the local files played (newest first, with when), each added the moment it
    // starts — only from the local player, never Spotify. Read from disk on first use;
    // while the file is there but can't be read, plays are kept here and not saved
    // (`history_loaded` stays false), so the file is never written over with less.
    history: Vec<(PathBuf, String)>,
    history_loaded: bool,
    /// What the Recent drawer shows, row by row.
    rview: Vec<PathBuf>,
    /// History entries outside the library: whether each file still exists (checked off
    /// the worker, `RecentChecked`), and whether a check is running.
    recent_known: std::collections::HashMap<PathBuf, bool>,
    recent_checking: bool,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            on: false,
            tracks: Vec::new(),
            playlists: Vec::new(),
            folders: Vec::new(),
            scanning: None,
            rescan: false,
            scanned_once: false,
            view: View::All,
            open: BTreeSet::from([Section::Artists, Section::Playlists]),
            filter: String::new(),
            sort: (-1, false),
            shown: Vec::new(),
            shown_pl: Vec::new(),
            nodes: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            drag: None,
            carry: Vec::new(),
            filters: Filters::default(),
            qview: Vec::new(),
            order: Vec::new(),
            source: Vec::new(),
            pos: 0,
            user_queue: VecDeque::new(),
            now: None,
            now_ms: 0,
            dur_ms: 0,
            playing: false,
            shuffle: false,
            repeat: 0,
            volume: 0.8,
            play_gen: 0,
            next_src: None,
            fails: 0,
            first_pos: None,
            cands: Vec::new(),
            cand_on: Vec::new(),
            find_open: false,
            pending: None,
            delete_pending: None,
            history: Vec::new(),
            history_loaded: false,
            rview: Vec::new(),
            recent_known: std::collections::HashMap::new(),
            recent_checking: false,
        }
    }
}

type Tx = tokio::sync::mpsc::UnboundedSender<Cmd>;

thread_local! {
    /// The headless `--shot-local` render: UI writes apply at once to this window
    /// (there is no event loop to post them to).
    static SHOT: std::cell::RefCell<Option<MainWindow>> = const { std::cell::RefCell::new(None) };
    /// UI thread: the track behind each row of the list and of the Queue drawer, as
    /// they are on screen (set in the same UI write that puts the rows there). A click
    /// is sent on with the track it hit, not just its row number.
    static UI_ROWS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    static UI_QUEUE: std::cell::RefCell<Vec<(QSrc, PathBuf)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn ui_row(i: i32) -> Option<PathBuf> {
    UI_ROWS.with(|r| usize::try_from(i).ok().and_then(|i| r.borrow().get(i).cloned()))
}

fn ui_queue(i: i32) -> Option<(QSrc, PathBuf)> {
    UI_QUEUE.with(|r| usize::try_from(i).ok().and_then(|i| r.borrow().get(i).cloned()))
}

/// Apply a UI write on the UI thread.
fn ui(weak: &slint::Weak<MainWindow>, f: impl FnOnce(MainWindow) + Send + 'static) {
    if let Some(a) = SHOT.with(|s| s.borrow().as_ref().map(|a| a.clone_strong())) {
        f(a);
        return;
    }
    let _ = weak.upgrade_in_event_loop(f);
}

fn send(tx: &Tx, c: LocalCmd) {
    let _ = tx.send(Cmd::Local(c));
}

// ── Mode persistence ─────────────────────────────────────────────────────────

fn mode_path() -> PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_mode.txt")
}

/// Was Local mode on when the app last closed?
pub fn load_mode() -> bool {
    std::fs::read_to_string(mode_path()).map(|s| s.trim() == "local").unwrap_or(false)
}

fn save_mode(on: bool) {
    let _ = lightify_core::config::write_atomic(&mode_path(), if on { "local" } else { "spotify" }.as_bytes());
}

// ── Wiring (UI thread) ───────────────────────────────────────────────────────

/// Connect every `LocalView` callback to the worker, and the player's events.
pub fn install(app: &MainWindow, tx: Tx) {
    // The worker reads the saved mode itself (it owns the state); the UI just starts
    // on the right side without a fade.
    app.set_local_mode(load_mode());
    let t = tx.clone();
    local_player::set_listener(move |gen, ev| send(&t, LocalCmd::Player(gen, ev)));
    let _ = DRAG_TX.set(tx.clone());
    macro_rules! wire {
        ($setter:ident, |$($a:ident),*| $cmd:expr) => {{
            let tx = tx.clone();
            app.$setter(move |$($a),*| send(&tx, $cmd));
        }};
    }
    wire!(on_local_mode_changed, |on| LocalCmd::ModeChanged(on));
    wire!(on_local_node_click, |i| LocalCmd::NodeClick(i.max(0) as usize));
    wire!(on_local_row_activate, |i| LocalCmd::RowActivate(i.max(0) as usize, ui_row(i)));
    wire!(on_local_row_select, |i, c, s| LocalCmd::RowSelect(i.max(0) as usize, c, s, ui_row(i)));
    wire!(on_local_row_clear_sel, | | LocalCmd::ClearSel);
    wire!(on_local_sort, |c| LocalCmd::Sort(c));
    wire!(on_local_find, | | LocalCmd::Find);
    wire!(on_local_rescan, | | LocalCmd::Rescan);
    wire!(on_local_filter_edited, |t| LocalCmd::Filter(t.to_string()));
    wire!(on_local_toggle_play, | | LocalCmd::TogglePlay);
    wire!(on_local_prev, | | LocalCmd::Prev);
    wire!(on_local_next, | | LocalCmd::Next);
    wire!(on_local_seek, |v| LocalCmd::Seek(v));
    wire!(on_local_set_volume, |v| LocalCmd::Volume(v));
    wire!(on_local_toggle_shuffle, | | LocalCmd::ToggleShuffle);
    wire!(on_local_cycle_repeat, | | LocalCmd::CycleRepeat);
    wire!(on_local_carry_begin, |i| LocalCmd::CarryBegin(i.max(0) as usize, ui_row(i)));
    wire!(on_local_carry_end, |t| LocalCmd::CarryEnd(t));
    wire!(on_local_find_toggle, |i| LocalCmd::FindToggle(i.max(0) as usize));
    wire!(on_local_find_add_folder, | | LocalCmd::FindAddFolder);
    wire!(on_local_find_add_files, | | LocalCmd::FindAddFiles);
    wire!(on_local_find_confirm_add, | | LocalCmd::FindConfirm);
    wire!(on_local_find_close, | | LocalCmd::FindClose);
    wire!(on_local_drag_begin, |i| LocalCmd::DragBegin(i.max(0) as usize));
    wire!(on_local_drag_to, |i| LocalCmd::DragTo(i.max(0) as usize));
    wire!(on_local_drag_end, | | LocalCmd::DragEnd);
    wire!(on_local_flt_set_mode, |m| LocalCmd::FltMode(m));
    wire!(on_local_flt_set_len, |longest, text| LocalCmd::FltLen(longest, text.to_string()));
    wire!(on_local_flt_reset, | | LocalCmd::FltReset);
    wire!(on_local_flt_toggle_type, |i| LocalCmd::FltType(i.max(0) as usize));
    // Queue rows go as the item they showed (where it sits now is `queue_item`'s job).
    wire!(on_local_queue_play, |i| match ui_queue(i) {
        Some((src, p)) => LocalCmd::QueueItemPlay(src, p),
        None => LocalCmd::QueuePlay(i.max(0) as usize),
    });
    wire!(on_local_queue_remove, |i| match ui_queue(i) {
        Some((src, p)) => LocalCmd::QueueItemRemove(src, p),
        None => LocalCmd::QueueRemove(i.max(0) as usize),
    });
    wire!(on_local_queue_clear, | | LocalCmd::QueueClear);
    wire!(on_local_recent_play, |i| LocalCmd::RecentPlay(i.max(0) as usize));
    wire!(on_local_confirm_accept, | | LocalCmd::DeleteConfirmed);
}

/// Should play/pause, next and previous (media keys, tray, palette) drive local
/// playback? While local audio holds the output and Local mode is up, or it plays.
pub fn owns_transport(st: &LocalState) -> bool {
    local_player::active() && (st.on || st.playing)
}

// ── Explorer drag-and-drop (UI thread) ───────────────────────────────────────

static DRAG_TX: std::sync::OnceLock<Tx> = std::sync::OnceLock::new();

pub enum Drag {
    Hover(PathBuf),
    Cancel,
    Drop(PathBuf),
}

thread_local! {
    static DRAG: std::cell::RefCell<(Vec<PathBuf>, Vec<PathBuf>)> = const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    static DRAG_FLUSH: slint::Timer = slint::Timer::default();
}

/// One winit file-drag event. winit sends one per file, back to back, so hovers and
/// drops are gathered and sent on as one list a moment after the last one.
pub fn drag_event(d: Drag) {
    let Some(tx) = DRAG_TX.get().cloned() else { return };
    DRAG.with(|g| {
        let mut g = g.borrow_mut();
        match d {
            Drag::Hover(p) => {
                g.1.clear();
                g.0.push(p);
            }
            Drag::Cancel => {
                g.0.clear();
                send(&tx, LocalCmd::DropCancel);
                return;
            }
            Drag::Drop(p) => g.1.push(p),
        }
    });
    DRAG_FLUSH.with(|t| {
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(40), move || {
            DRAG.with(|g| {
                let mut g = g.borrow_mut();
                if !g.1.is_empty() {
                    g.0.clear();
                    send(&tx, LocalCmd::Dropped(std::mem::take(&mut g.1)));
                } else if !g.0.is_empty() {
                    send(&tx, LocalCmd::DropHover(g.0.clone()));
                }
            });
        });
    });
}

// ── The controller (worker) ──────────────────────────────────────────────────

pub fn handle(st: &mut LocalState, cmd: LocalCmd, weak: &slint::Weak<MainWindow>, tx: &Tx) {
    match cmd {
        LocalCmd::ModeChanged(on) => {
            st.on = on;
            save_mode(on);
            if on {
                if !st.scanned_once && st.tracks.is_empty() {
                    st.filters = load_filters();
                    push_len_fields(st, weak, None);
                    st.volume = load_volume();
                    local_player::set_volume(st.volume);
                    st.tracks = local_library::cached();
                    st.playlists = local_library::playlists();
                    refresh(st, weak);
                }
                if st.scanning.is_none() {
                    start_scan(st, weak, tx);
                }
            }
        }
        LocalCmd::Rescan => start_scan(st, weak, tx),
        LocalCmd::ScanProgress(n) => {
            if st.scanning.is_some() {
                st.scanning = Some(n);
                push_kicker(st, weak);
            }
        }
        LocalCmd::Scanned(tracks) => {
            st.scanning = None;
            // The sources changed while it ran (a folder added or removed): what it found
            // is already out of date, and showing it could bring a removed folder back.
            if std::mem::take(&mut st.rescan) {
                start_scan(st, weak, tx);
                return;
            }
            st.scanned_once = true;
            st.tracks = tracks;
            st.playlists = local_library::playlists();
            refresh(st, weak);
        }
        LocalCmd::NodeClick(i) => {
            let Some((_, act, _)) = st.nodes.get(i).cloned() else { return };
            match act {
                NodeAct::View(v) => {
                    st.view = v;
                    st.sort = (-1, false);
                    clear_selection(st);
                    refresh(st, weak);
                    ui(weak, |a| a.set_local_now_open(false));
                }
                NodeAct::Toggle(sec) => {
                    if !st.open.remove(&sec) {
                        st.open.insert(sec);
                    }
                    push_tree(st, weak);
                }
                NodeAct::NewPlaylist => ask_name(st, weak, Pending::NewPlaylist(Vec::new())),
            }
        }
        LocalCmd::RowActivate(row, path) => {
            if let Some(row) = clicked_row(st, row, path.as_deref()) {
                play_from(st, row, weak);
            }
        }
        LocalCmd::RowSelect(row, ctrl, shift, path) => {
            let Some(row) = clicked_row(st, row, path.as_deref()) else { return };
            if shift {
                let a = st.anchor.unwrap_or(row);
                let (lo, hi) = (a.min(row), a.max(row));
                if !ctrl {
                    st.selected.clear();
                }
                st.selected.extend(lo..=hi);
            } else if !st.selected.remove(&row) {
                st.selected.insert(row);
            }
            st.anchor = Some(row);
            push_sel(st, weak);
        }
        LocalCmd::ClearSel => {
            clear_selection(st);
            push_sel(st, weak);
        }
        LocalCmd::DragBegin(row) => {
            // Each drag ADDS to what is already selected (groups with gaps between).
            let base = st.selected.clone();
            st.selected.insert(row);
            st.drag = Some((base, row));
            st.anchor = Some(row);
            push_sel(st, weak);
        }
        LocalCmd::DragTo(row) => {
            if let Some((base, from)) = st.drag.clone() {
                let row = row.min(st.shown.len().saturating_sub(1));
                st.selected = base;
                st.selected.extend(from.min(row)..=from.max(row));
                st.anchor = Some(row);
                push_sel(st, weak);
            }
        }
        LocalCmd::DragEnd => st.drag = None,
        LocalCmd::FltMode(m) => {
            st.filters.mode = m.clamp(0, 2);
            filters_changed(st, weak);
        }
        LocalCmd::FltLen(longest, text) => match parse_len(&text) {
            Some(secs) => {
                let slot = if longest { &mut st.filters.max_s } else { &mut st.filters.min_s };
                let changed = *slot != secs;
                *slot = secs;
                if changed {
                    filters_changed(st, weak);
                }
                push_len_fields(st, weak, None);
            }
            None => push_len_fields(st, weak, Some(longest)),
        },
        LocalCmd::FltReset => {
            st.filters = Filters::default();
            filters_changed(st, weak);
            push_len_fields(st, weak, None);
        }
        LocalCmd::FltType(i) => {
            if let Some(t) = present_types(st).get(i).cloned() {
                if let Some(at) = st.filters.hidden_types.iter().position(|h| *h == t) {
                    st.filters.hidden_types.remove(at);
                } else {
                    st.filters.hidden_types.push(t);
                }
                filters_changed(st, weak);
            }
        }
        // A Recent row plays, with the rest of Recent following it.
        LocalCmd::RecentPlay(i) => {
            if i < st.rview.len() {
                let list = st.rview.clone();
                play_list(st, list, i, weak);
            }
        }
        LocalCmd::RecentRefresh => {
            push_recent(st, weak, None);
            // The drawer opened: look again at the files outside the library (one may
            // have been deleted, or a share come back), off the worker.
            check_recent(st, tx, true);
        }
        LocalCmd::RecentChecked(found) => {
            st.recent_checking = false;
            st.recent_known.extend(found);
            push_recent(st, weak, Some(tx));
        }
        LocalCmd::QueuePlay(i) => {
            if let Some((src, p)) = st.qview.get(i).cloned() {
                queue_play(st, src, &p, weak);
            }
        }
        LocalCmd::QueueItemPlay(src, p) => queue_play(st, src, &p, weak),
        LocalCmd::QueueRemove(i) => {
            if let Some((src, p)) = st.qview.get(i).cloned() {
                queue_remove(st, src, &p, weak);
            }
        }
        LocalCmd::QueueItemRemove(src, p) => queue_remove(st, src, &p, weak),
        LocalCmd::QueueClear => {
            st.user_queue.clear();
            st.order.truncate(st.pos + 1);
            st.source = st.order.clone();
            set_next(st);
            push_now(st, weak);
        }
        LocalCmd::ShowInFolder(p) => show_in_folder(&p),
        LocalCmd::Sort(c) => {
            st.sort = if st.sort.0 == c { (c, !st.sort.1) } else { (c, false) };
            clear_selection(st);
            build_shown(st);
            push_rows(st, weak);
        }
        LocalCmd::Filter(t) => {
            st.filter = t.trim().to_lowercase();
            clear_selection(st);
            refresh(st, weak);
        }
        LocalCmd::TogglePlay => {
            if !local_player::active() {
                st.fails = 0;
                if let Some(p) = st.now.clone() {
                    let ms = st.now_ms;
                    play_path(st, p, ms);
                } else if !st.shown.is_empty() {
                    play_from(st, 0, weak);
                }
            } else if st.playing {
                local_player::pause();
            } else {
                pause_spotify();
                local_player::resume();
            }
        }
        LocalCmd::Next => {
            st.fails = 0;
            advance(st, weak, 1);
        }
        LocalCmd::Prev => {
            st.fails = 0;
            if st.now_ms > 3000 {
                local_player::seek(0);
            } else {
                advance(st, weak, -1);
            }
        }
        LocalCmd::Seek(f) => {
            if st.dur_ms > 0 {
                local_player::seek((f.clamp(0.0, 1.0) as f64 * st.dur_ms as f64) as u64);
            }
        }
        LocalCmd::Volume(v) => {
            st.volume = v.clamp(0.0, 1.0);
            local_player::set_volume(st.volume);
            // The bar sends one value per release, not one per pixel dragged.
            save_volume(st.volume);
        }
        LocalCmd::ToggleShuffle => {
            st.shuffle = !st.shuffle;
            rebuild_order_keep_current(st);
            push_now(st, weak);
        }
        LocalCmd::CycleRepeat => {
            st.repeat = (st.repeat + 1) % 3;
            set_next(st);
            push_now(st, weak);
        }
        LocalCmd::CarryBegin(row, path) => {
            st.carry = clicked_row(st, row, path.as_deref()).map(|row| rows_or_selection(st, row)).unwrap_or_default();
            let n = st.carry.len();
            let what = plural(n);
            ui(weak, move |a| a.set_local_carry_what(what.into()));
        }
        LocalCmd::CarryEnd(target) => {
            let paths = std::mem::take(&mut st.carry);
            if let Some((_, _, Some(id))) = usize::try_from(target).ok().and_then(|t| st.nodes.get(t)) {
                let id = id.clone();
                if !paths.is_empty() {
                    add_to_playlist(st, &paths, &id, weak);
                }
            }
        }
        LocalCmd::DropHover(paths) => {
            if !st.on {
                return;
            }
            let (audio, folders, skipped) = classify(&paths);
            let mut parts = Vec::new();
            if audio > 0 {
                parts.push(lightify_i18n::trn!("{n} audio file", "{n} audio files", audio));
            }
            if folders > 0 {
                parts.push(lightify_i18n::trn!("{n} folder", "{n} folders", folders));
            }
            if skipped > 0 {
                parts.push(lightify_i18n::trn!("{n} skipped (not audio)", "{n} skipped (not audio)", skipped));
            }
            let detail = parts.join(" \u{00b7} ");
            ui(weak, move |a| {
                a.set_local_drop_detail(detail.into());
                a.set_local_drop_active(true);
            });
        }
        LocalCmd::DropCancel => {
            ui(weak, |a| a.set_local_drop_active(false));
        }
        LocalCmd::Dropped(paths) => {
            ui(weak, |a| a.set_local_drop_active(false));
            if !st.on {
                return;
            }
            add_and_rescan(st, &paths, weak, tx);
        }
        LocalCmd::Find => {
            st.cands.clear();
            st.cand_on.clear();
            st.find_open = true;
            push_find(st, weak, true, true);
            let tx = tx.clone();
            std::thread::spawn(move || send(&tx, LocalCmd::FindFound(local_library::find_candidates())));
        }
        LocalCmd::FindFound(c) => {
            // Closed while it was looking: the answer doesn't open it again.
            if !st.find_open {
                return;
            }
            st.cand_on = c.iter().map(|c| !c.already_added).collect();
            st.cands = c;
            push_find(st, weak, true, false);
        }
        LocalCmd::FindToggle(i) => {
            if let Some(on) = st.cand_on.get_mut(i) {
                *on = !*on;
                push_find(st, weak, st.find_open, false);
            }
        }
        LocalCmd::FindAddFolder => {
            let tx = tx.clone();
            std::thread::spawn(move || send(&tx, LocalCmd::Picked(pick(true))));
        }
        LocalCmd::FindAddFiles => {
            let tx = tx.clone();
            std::thread::spawn(move || send(&tx, LocalCmd::Picked(pick(false))));
        }
        LocalCmd::Picked(paths) => {
            if !paths.is_empty() {
                st.find_open = false;
                push_find(st, weak, false, false);
                add_and_rescan(st, &paths, weak, tx);
            }
        }
        LocalCmd::FindConfirm => {
            let paths: Vec<PathBuf> = st
                .cands
                .iter()
                .zip(&st.cand_on)
                .filter(|(c, on)| **on && !c.already_added)
                .map(|(c, _)| c.path.clone())
                .collect();
            st.find_open = false;
            push_find(st, weak, false, false);
            if !paths.is_empty() {
                add_and_rescan(st, &paths, weak, tx);
            }
        }
        LocalCmd::FindClose => {
            st.find_open = false;
            push_find(st, weak, false, false);
        }
        LocalCmd::MenuPlay(row, paths) => {
            match paths.as_slice() {
                [] => {}
                // One track: the list on screen plays from it, if it is still there.
                [one] => match row_of(st, row, one) {
                    Some(r) => play_from(st, r, weak),
                    None => play_list(st, paths, 0, weak),
                },
                _ => play_list(st, paths, 0, weak),
            }
        }
        LocalCmd::MenuQueue(paths) => {
            let n = paths.len();
            st.user_queue.extend(paths);
            set_next(st);
            push_queue(st, weak);
            crate::set_status(weak, lightify_i18n::trn!("Added {n} track to the queue", "Added {n} tracks to the queue", n));
        }
        LocalCmd::MenuAddTo(paths, id) => add_to_playlist(st, &paths, &id, weak),
        LocalCmd::MenuNewPlaylist(paths) => ask_name(st, weak, Pending::NewPlaylist(paths)),
        LocalCmd::MenuRemoveFromPlaylist(id, entries) => {
            if !local_library::remove_from_playlist(&id, &entries) {
                playlists_not_saved(weak);
            }
            st.playlists = local_library::playlists();
            clear_selection(st);
            refresh(st, weak);
        }
        LocalCmd::NodePlay(v) => {
            st.view = v;
            st.sort = (-1, false);
            clear_selection(st);
            refresh(st, weak);
            play_from(st, 0, weak);
        }
        LocalCmd::NodeRename(id) => ask_name(st, weak, Pending::Rename(id)),
        LocalCmd::NodeDelete(id) => ask_delete(st, weak, id),
        LocalCmd::DeleteConfirmed => {
            let Some(id) = st.delete_pending.take() else { return };
            if !local_library::delete_playlist(&id) {
                playlists_not_saved(weak);
            }
            st.playlists = local_library::playlists();
            if !st.playlists.iter().any(|p| p.id == id) && st.view == View::Playlist(id) {
                st.view = View::All;
                clear_selection(st);
            }
            refresh(st, weak);
        }
        LocalCmd::NodeNewPlaylist => ask_name(st, weak, Pending::NewPlaylist(Vec::new())),
        LocalCmd::NodeRemoveFolder(p) => {
            local_library::remove_folder(&p);
            if st.view == View::Folder(p) {
                st.view = View::All;
                clear_selection(st);
                refresh(st, weak);
            }
            start_scan(st, weak, tx);
        }
        LocalCmd::NodeShowInFolder(p) => open_path(&p),
        LocalCmd::Named(name) => {
            let Some(p) = st.pending.take() else { return };
            let Some(name) = name.map(|n| n.trim().to_string()) else { return };
            if name.is_empty() {
                // Only spaces: the prompt stays up asking for a name (as Spotify's does),
                // instead of closing on nothing.
                st.pending = Some(p);
                ui(weak, |a| {
                    a.set_name_prompt_note(tr!("Give the playlist a name").into());
                    a.set_name_prompt_open(true);
                });
                return;
            }
            match p {
                Pending::NewPlaylist(paths) => match local_library::create_playlist(&name, &paths) {
                    Some(id) => {
                        st.playlists = local_library::playlists();
                        st.open.insert(Section::Playlists);
                        if paths.is_empty() {
                            st.view = View::Playlist(id);
                            clear_selection(st);
                        }
                        crate::set_status(weak, tr!("Created \u{201c}{}\u{201d}", name));
                    }
                    None => playlists_not_saved(weak),
                },
                Pending::Rename(id) => {
                    if !local_library::rename_playlist(&id, &name) {
                        playlists_not_saved(weak);
                    }
                    st.playlists = local_library::playlists();
                }
            }
            refresh(st, weak);
        }
        LocalCmd::Relabel => relabel(st, weak),
        LocalCmd::Player(gen, ev) => {
            // A `play` started since: what an older one says is about a file no longer
            // playing (it may have been queued before that `play` and read only now).
            if gen == st.play_gen {
                on_player(st, ev, weak, tx);
            }
        }
        LocalCmd::Cover(path, px) => {
            if st.now.as_deref() != Some(path.as_path()) {
                return;
            }
            ui(weak, move |a| {
                let (img, glow) = match px {
                    Some((rgba, w, h, glow)) => {
                        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                        (slint::Image::from_rgba8(buf), glow)
                    }
                    None => (slint::Image::default(), slint::Color::from_argb_u8(0x55, 0x34, 0xd3, 0x99)),
                };
                a.set_local_now_cover(img);
                a.set_local_now_glow(glow);
            });
        }
    }
}

/// Is a name prompt open for Local (so `PlaylistNamed` belongs here)?
pub fn wants_name(st: &LocalState) -> bool {
    st.pending.is_some()
}

/// Spotify started playing: local playback gives way.
pub fn spotify_started(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    if local_player::active() {
        local_player::stop();
        st.playing = false;
        push_now(st, weak);
    }
}

// ── Context menus ────────────────────────────────────────────────────────────

pub fn row_menu(st: &LocalState, row: usize) -> Vec<CtxItem> {
    if row >= st.shown.len() {
        return Vec::new();
    }
    let many = st.selected.contains(&row) && st.selected.len() > 1;
    let n = if many { st.selected.len() } else { 1 };
    // Resolved now, while the rows are the ones the user right-clicked.
    let paths = rows_or_selection(st, row);
    let mut items = vec![
        CtxItem::new(
            if many { lightify_i18n::trn!("Play {n} track", "Play {n} tracks", n) } else { tr!("Play") },
            Cmd::Local(LocalCmd::MenuPlay(row, paths.clone())),
        ),
        CtxItem::new(tr!("Add to queue"), Cmd::Local(LocalCmd::MenuQueue(paths.clone()))),
    ];
    for p in &st.playlists {
        items.push(CtxItem::new(tr!("Add to \u{201c}{}\u{201d}", p.name), Cmd::Local(LocalCmd::MenuAddTo(paths.clone(), p.id.clone()))));
    }
    items.push(CtxItem::new(
        if many {
            lightify_i18n::trn!("New playlist from {n} track", "New playlist from {n} tracks", n)
        } else {
            tr!("New playlist from track")
        },
        Cmd::Local(LocalCmd::MenuNewPlaylist(paths.clone())),
    ));
    if !many {
        if let Some(p) = paths.first() {
            items.push(CtxItem::new(tr!("Show in folder"), Cmd::Local(LocalCmd::ShowInFolder(p.clone()))));
        }
    }
    if let View::Playlist(id) = &st.view {
        let rows: Vec<usize> = if st.selected.contains(&row) { st.selected.iter().copied().collect() } else { vec![row] };
        let entries: Vec<(usize, PathBuf)> = rows
            .iter()
            .filter_map(|&r| Some((*st.shown_pl.get(r)?, st.tracks[*st.shown.get(r)?].path.clone())))
            .collect();
        items.push(CtxItem::danger(
            if many { lightify_i18n::trn!("Remove {n} from playlist", "Remove {n} from playlist", n) } else { tr!("Remove from playlist") },
            Cmd::Local(LocalCmd::MenuRemoveFromPlaylist(id.clone(), entries)),
        ));
    }
    items
}

pub fn queue_menu(st: &LocalState, i: usize) -> Vec<CtxItem> {
    let Some((src, path)) = st.qview.get(i) else { return Vec::new() };
    let mut items = Vec::new();
    if *src != QSrc::Current {
        items.push(CtxItem::new(tr!("Play now"), Cmd::Local(LocalCmd::QueueItemPlay(*src, path.clone()))));
    }
    items.push(CtxItem::new(tr!("Show in folder"), Cmd::Local(LocalCmd::ShowInFolder(path.clone()))));
    if *src != QSrc::Current {
        items.push(CtxItem::danger(tr!("Remove from queue"), Cmd::Local(LocalCmd::QueueItemRemove(*src, path.clone()))));
    }
    items
}

pub fn node_menu(st: &LocalState, i: usize) -> Vec<CtxItem> {
    let Some((node, act, pl)) = st.nodes.get(i) else { return Vec::new() };
    let mut items = Vec::new();
    if let NodeAct::View(v) = act {
        items.push(CtxItem::new(tr!("Play"), Cmd::Local(LocalCmd::NodePlay(v.clone()))));
        if let Some(id) = pl {
            items.push(CtxItem::new(tr!("Rename"), Cmd::Local(LocalCmd::NodeRename(id.clone()))));
            items.push(CtxItem::danger(tr!("Delete playlist"), Cmd::Local(LocalCmd::NodeDelete(id.clone()))));
        }
        if let View::Folder(p) = v {
            items.push(CtxItem::new(tr!("Show in folder"), Cmd::Local(LocalCmd::NodeShowInFolder(p.clone()))));
            items.push(CtxItem::danger(tr!("Remove folder from library"), Cmd::Local(LocalCmd::NodeRemoveFolder(p.clone()))));
        }
    } else if node.kind == 0 && matches!(act, NodeAct::Toggle(Section::Playlists)) {
        items.push(CtxItem::new(tr!("New playlist"), Cmd::Local(LocalCmd::NodeNewPlaylist)));
    }
    items
}

// ── Library views ────────────────────────────────────────────────────────────

/// Scan the library. One scan at a time: asked for during one, another follows it (two
/// at once could finish out of order, an older one bringing a removed folder back).
fn start_scan(st: &mut LocalState, weak: &slint::Weak<MainWindow>, tx: &Tx) {
    st.folders = local_library::sources().folders;
    if st.scanning.is_some() {
        st.rescan = true;
        return;
    }
    st.scanning = Some(0);
    push_kicker(st, weak);
    let tx = tx.clone();
    std::thread::spawn(move || {
        let t2 = tx.clone();
        let last = std::cell::Cell::new(std::time::Instant::now());
        let tracks = local_library::scan(&|n| {
            if last.get().elapsed() > std::time::Duration::from_millis(250) {
                last.set(std::time::Instant::now());
                send(&t2, LocalCmd::ScanProgress(n));
            }
        });
        send(&tx, LocalCmd::Scanned(tracks));
    });
}

fn add_and_rescan(st: &mut LocalState, paths: &[PathBuf], weak: &slint::Weak<MainWindow>, tx: &Tx) {
    let r = local_library::add_paths(paths);
    let folders = || lightify_i18n::trn!("{n} folder", "{n} folders", r.folders);
    let msg = match (r.audio_files > 0, r.folders > 0) {
        (false, false) => tr!("Nothing to add \u{2014} only audio files and folders are taken"),
        (true, true) => tr!("Added {0} and {1} \u{2014} scanning\u{2026}", plural(r.audio_files), folders()),
        (true, false) => tr!("Added {} \u{2014} scanning\u{2026}", plural(r.audio_files)),
        (false, true) => tr!("Added {} \u{2014} scanning\u{2026}", folders()),
    };
    crate::set_status(weak, msg);
    start_scan(st, weak, tx);
}

fn refresh(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    let keys = selection_keys(st);
    build_shown(st);
    remap_selection(st, keys);
    push_tree(st, weak);
    push_rows(st, weak);
    push_now(st, weak);
}

/// What a shown row is, independent of where it sits: its path, and in a playlist its
/// position there (a playlist may hold one file twice).
type RowKey = (PathBuf, Option<usize>);

fn row_key(st: &LocalState, row: usize) -> Option<RowKey> {
    let path = st.tracks.get(*st.shown.get(row)?)?.path.clone();
    let pl = matches!(st.view, View::Playlist(_)).then(|| st.shown_pl.get(row).copied()).flatten();
    Some((path, pl))
}

/// The selection and its anchor, as rows' identities.
fn selection_keys(st: &LocalState) -> (Vec<RowKey>, Option<RowKey>) {
    let sel = st.selected.iter().filter_map(|&r| row_key(st, r)).collect();
    (sel, st.anchor.and_then(|a| row_key(st, a)))
}

/// After the rows were rebuilt (a scan, an edit), select the same tracks again where
/// they now are, so a later action can't land on whatever moved into the old rows.
fn remap_selection(st: &mut LocalState, (sel, anchor): (Vec<RowKey>, Option<RowKey>)) {
    if sel.is_empty() && anchor.is_none() {
        return;
    }
    let at: std::collections::HashMap<RowKey, usize> =
        (0..st.shown.len()).filter_map(|r| Some((row_key(st, r)?, r))).collect();
    let before = std::mem::take(&mut st.selected);
    st.selected = sel.iter().filter_map(|k| at.get(k).copied()).collect();
    st.anchor = anchor.and_then(|k| at.get(&k).copied());
    if st.selected != before {
        // A drag in progress holds rows of the old list.
        st.drag = None;
    }
}

/// Where the track `path` the user picked at display row `row` is now: that row while
/// it still shows it, else wherever it moved; `None` when it is gone from the list.
fn row_of(st: &LocalState, row: usize, path: &Path) -> Option<usize> {
    let at = |r: usize| st.shown.get(r).and_then(|&k| st.tracks.get(k)).is_some_and(|t| t.path == path);
    if at(row) {
        return Some(row);
    }
    (0..st.shown.len()).find(|&r| at(r))
}

fn add_to_playlist(st: &mut LocalState, paths: &[PathBuf], id: &str, weak: &slint::Weak<MainWindow>) {
    if local_library::add_to_playlist(id, paths) {
        st.playlists = local_library::playlists();
        let name = st.playlists.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_default();
        crate::set_status(weak, added_to(paths.len(), &name));
    } else {
        playlists_not_saved(weak);
    }
    refresh(st, weak);
}

/// A playlist edit wasn't saved (the playlists file couldn't be read or written).
fn playlists_not_saved(weak: &slint::Weak<MainWindow>) {
    crate::set_status(weak, tr!("Couldn\u{2019}t save local playlists \u{2014} their file can\u{2019}t be read or written right now"));
}

fn track_matches(t: &LocalTrack, f: &str) -> bool {
    f.is_empty()
        || t.title.to_lowercase().contains(f)
        || local_library::shown(&t.artist).to_lowercase().contains(f)
        || local_library::shown(&t.album).to_lowercase().contains(f)
}

fn build_shown(st: &mut LocalState) {
    let f = st.filter.clone();
    st.shown_pl.clear();
    let mut shown: Vec<usize> = match &st.view {
        View::Playlist(id) => {
            let by_path: BTreeMap<&Path, usize> = st.tracks.iter().enumerate().map(|(i, t)| (t.path.as_path(), i)).collect();
            let mut v = Vec::new();
            if let Some(p) = st.playlists.iter().find(|p| &p.id == id) {
                for (pi, path) in p.tracks.iter().enumerate() {
                    if let Some(&k) = by_path.get(path.as_path()) {
                        if track_matches(&st.tracks[k], &f) {
                            v.push(k);
                            st.shown_pl.push(pi);
                        }
                    }
                }
            }
            v
        }
        view => {
            let mut v: Vec<usize> = (0..st.tracks.len())
                .filter(|&i| {
                    let t = &st.tracks[i];
                    st.filters.passes(t)
                        && track_matches(t, &f)
                        && match view {
                            View::All | View::Recent => true,
                            View::Artist(a) => &t.album_artist == a || &t.artist == a,
                            View::Album(al, aa) => &t.album == al && &t.album_artist == aa,
                            View::Genre(g) => &t.genre == g,
                            View::Folder(p) => t.path.starts_with(p),
                            View::Playlist(_) => false,
                        }
                })
                .collect();
            if *view == View::Recent {
                v.sort_by(|a, b| st.tracks[*b].added_at.cmp(&st.tracks[*a].added_at));
                v.truncate(200);
            }
            v
        }
    };
    let (col, desc) = st.sort;
    if col >= 0 {
        let t = &st.tracks;
        let key = |i: &usize| -> String {
            let x = &t[*i];
            match col {
                1 => x.title.to_lowercase(),
                2 => x.artist.to_lowercase(),
                3 => format!("{}\u{1f}{:03}\u{1f}{:04}", x.album.to_lowercase(), x.disc_no.unwrap_or(0), x.track_no.unwrap_or(0)),
                4 => format!("{:012}", x.duration_ms),
                _ => String::new(),
            }
        };
        if col > 0 {
            let pl = !st.shown_pl.is_empty();
            let mut pairs: Vec<(usize, usize)> = shown.iter().copied().zip(if pl { st.shown_pl.clone() } else { vec![0; shown.len()] }).collect();
            pairs.sort_by_key(|(i, _)| key(i));
            if desc {
                pairs.reverse();
            }
            shown = pairs.iter().map(|p| p.0).collect();
            if pl {
                st.shown_pl = pairs.iter().map(|p| p.1).collect();
            }
        } else if desc {
            shown.reverse();
            st.shown_pl.reverse();
        }
    }
    st.shown = shown;
}

fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn fmt_total(ms: u64) -> String {
    let m = ms / 60_000;
    if m >= 60 { tr!("{} h {} m", m / 60, format!("{:02}", m % 60)) } else { tr!("{} m", m) }
}

fn plural(n: usize) -> String {
    lightify_i18n::trn!("{n} track", "{n} tracks", n)
}

/// "Added 3 tracks to “Focus”".
fn added_to(n: usize, playlist: &str) -> String {
    lightify_i18n::trn!("Added {n} track to \u{201c}{0}\u{201d}", "Added {n} tracks to \u{201c}{0}\u{201d}", n, playlist)
}

fn push_kicker(st: &LocalState, weak: &slint::Weak<MainWindow>) {
    let n = st.tracks.len();
    let hidden = st.tracks.iter().filter(|t| !st.filters.passes(t)).count();
    let k = match st.scanning {
        Some(done) if done > 0 => tr!("// local \u{00b7} scanning {}\u{2026}", done),
        Some(_) => tr!("// local \u{00b7} scanning\u{2026}"),
        None if hidden > 0 => tr!("// local \u{00b7} {} of {}", thousands(n - hidden), thousands(n)),
        None => tr!("// local \u{00b7} {}", thousands(n)),
    };
    let note = if hidden > 0 {
        lightify_i18n::trn!("{0} of {1} hidden", "{0} of {1} hidden", hidden, thousands(hidden), thousands(n))
    } else {
        tr!("nothing hidden")
    };
    let flags: Vec<crate::LocalFlag> = present_types(st)
        .iter()
        .map(|t| crate::LocalFlag { label: t.to_uppercase().into(), on: !st.filters.hidden_types.contains(t) })
        .collect();
    let (mode, active, custom) = (st.filters.mode, hidden > 0, !st.filters.is_default());
    ui(weak, move |a| {
        a.set_local_kicker(k.into());
        a.set_local_flt_note(note.into());
        a.set_local_flt_types(slint::ModelRc::from(Rc::new(slint::VecModel::from(flags))));
        a.set_local_flt_mode(mode);
        a.set_local_flt_active(active);
        a.set_local_flt_custom(custom);
    });
}

/// The length fields' texts, normalised ("90" → "1:30"). Only on a commit or a load —
/// never while scans push the kicker, which would overwrite what is being typed.
/// `bad`: that field (the "longest" one?) could not be read; it keeps its text.
fn push_len_fields(st: &LocalState, weak: &slint::Weak<MainWindow>, bad: Option<bool>) {
    let (min, max) = (fmt_len(st.filters.min_s), fmt_len(st.filters.max_s));
    ui(weak, move |a| {
        if bad != Some(false) {
            a.set_local_flt_min_text(min.into());
        }
        if bad != Some(true) {
            a.set_local_flt_max_text(max.into());
        }
        a.set_local_flt_min_bad(bad == Some(false));
        a.set_local_flt_max_bad(bad == Some(true));
    });
}

/// Every file type the library has, plus any switched off: the common ones first, the
/// rest alphabetically.
fn present_types(st: &LocalState) -> Vec<String> {
    let mut have: BTreeSet<String> = st.tracks.iter().map(|t| ext_of(&t.path)).filter(|e| !e.is_empty()).collect();
    have.extend(st.filters.hidden_types.iter().cloned());
    let rank = |t: &String| TYPE_ORDER.iter().position(|o| o == t).unwrap_or(TYPE_ORDER.len());
    let mut v: Vec<String> = have.into_iter().collect();
    v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    v
}

fn filters_changed(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    save_filters(&st.filters);
    clear_selection(st);
    refresh(st, weak);
}

/// Selection marks only, edited in place (a drag sends one per row crossed).
fn push_sel(st: &LocalState, weak: &slint::Weak<MainWindow>) {
    let sel: Vec<bool> = (0..st.shown.len()).map(|i| st.selected.contains(&i)).collect();
    let n = st.selected.len() as i32;
    ui(weak, move |a| {
        use slint::Model;
        let m = a.get_local_rows();
        for (i, on) in sel.iter().enumerate() {
            if let Some(mut r) = m.row_data(i) {
                if r.selected != *on {
                    r.selected = *on;
                    m.set_row_data(i, r);
                }
            }
        }
        a.set_local_sel_count(n);
    });
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn push_tree(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    let mut nodes: Vec<(LocalNode, NodeAct, Option<String>)> = Vec::new();
    // Section labels are translated here, where they're shown.
    let head = |label: &str, count: usize, kind: i32, open: bool, sel: bool| LocalNode {
        label: lightify_i18n::translate(label).into(),
        count: thousands(count).into(),
        kind,
        open,
        selected: sel,
        droppable: false,
    };
    let sub = |label: &str, count: usize, sel: bool, drop: bool| LocalNode {
        label: label.into(),
        count: thousands(count).into(),
        kind: 1,
        open: false,
        selected: sel,
        droppable: drop,
    };
    let flt = st.filters.clone();
    let lib: Vec<&LocalTrack> = st.tracks.iter().filter(|t| flt.passes(t)).collect();
    nodes.push((head(lightify_i18n::tr_noop!("All music"), lib.len(), 3, false, st.view == View::All), NodeAct::View(View::All), None));

    let mut artists: BTreeMap<String, usize> = BTreeMap::new();
    let mut albums: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut genres: BTreeMap<String, usize> = BTreeMap::new();
    for t in &lib {
        *artists.entry(t.album_artist.clone()).or_default() += 1;
        *albums.entry((t.album.clone(), t.album_artist.clone())).or_default() += 1;
        if !t.genre.is_empty() {
            *genres.entry(t.genre.clone()).or_default() += 1;
        }
    }
    // An artist's list (`View::Artist`) also has the tracks they play on under another
    // album artist (a compilation): counted here too, so the number is the list's.
    for t in &lib {
        if t.artist != t.album_artist {
            if let Some(n) = artists.get_mut(&t.artist) {
                *n += 1;
            }
        }
    }
    let ci = |s: &String| s.to_lowercase();
    let mut a: Vec<(String, usize)> = artists.into_iter().collect();
    a.sort_by_key(|x| ci(&x.0));
    let open = st.open.contains(&Section::Artists);
    nodes.push((head(lightify_i18n::tr_noop!("Artists"), a.len(), 0, open, false), NodeAct::Toggle(Section::Artists), None));
    if open {
        for (name, n) in &a {
            let v = View::Artist(name.clone());
            nodes.push((sub(&local_library::shown(name), *n, st.view == v, false), NodeAct::View(v), None));
        }
    }
    let mut al: Vec<((String, String), usize)> = albums.into_iter().collect();
    al.sort_by_key(|x| ci(&x.0 .0));
    let open = st.open.contains(&Section::Albums);
    nodes.push((head(lightify_i18n::tr_noop!("Albums"), al.len(), 0, open, false), NodeAct::Toggle(Section::Albums), None));
    if open {
        for ((album, aa), n) in &al {
            let v = View::Album(album.clone(), aa.clone());
            nodes.push((sub(&format!("{} \u{00b7} {}", local_library::shown(album), local_library::shown(aa)), *n, st.view == v, false), NodeAct::View(v), None));
        }
    }
    let mut g: Vec<(String, usize)> = genres.into_iter().collect();
    g.sort_by_key(|x| ci(&x.0));
    let open = st.open.contains(&Section::Genres);
    nodes.push((head(lightify_i18n::tr_noop!("Genres"), g.len(), 0, open, false), NodeAct::Toggle(Section::Genres), None));
    if open {
        for (name, n) in &g {
            let v = View::Genre(name.clone());
            nodes.push((sub(name, *n, st.view == v, false), NodeAct::View(v), None));
        }
    }
    let folders = st.folders.clone();
    let open = st.open.contains(&Section::Folders);
    nodes.push((head(lightify_i18n::tr_noop!("Folders"), folders.len(), 0, open, false), NodeAct::Toggle(Section::Folders), None));
    if open {
        for f in &folders {
            let n = lib.iter().filter(|t| t.path.starts_with(f)).count();
            let label = f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| f.display().to_string());
            let v = View::Folder(f.clone());
            nodes.push((sub(&label, n, st.view == v, false), NodeAct::View(v), None));
        }
    }
    let recent = lib.len().min(200);
    nodes.push((head(lightify_i18n::tr_noop!("Recently added"), recent, 3, false, st.view == View::Recent), NodeAct::View(View::Recent), None));
    let open = st.open.contains(&Section::Playlists);
    nodes.push((head(lightify_i18n::tr_noop!("Local playlists"), st.playlists.len(), 0, open, false), NodeAct::Toggle(Section::Playlists), None));
    if open {
        for p in &st.playlists {
            let v = View::Playlist(p.id.clone());
            nodes.push((sub(&p.name, p.tracks.len(), st.view == v, true), NodeAct::View(v), Some(p.id.clone())));
        }
        let mut add = head(lightify_i18n::tr_noop!("New playlist"), 0, 2, false, false);
        add.count = "".into();
        nodes.push((add, NodeAct::NewPlaylist, None));
    }
    let rows: Vec<LocalNode> = nodes.iter().map(|n| n.0.clone()).collect();
    st.nodes = nodes;
    ui(weak, move |a| {
        a.set_local_nodes(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    });
    push_kicker(st, weak);
}

fn view_title(st: &LocalState) -> String {
    match &st.view {
        View::All => tr!("All music"),
        View::Artist(a) => local_library::shown(a),
        View::Album(al, _) => local_library::shown(al),
        View::Genre(g) => g.clone(),
        View::Folder(p) => p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string()),
        View::Recent => tr!("Recently added"),
        View::Playlist(id) => st.playlists.iter().find(|p| &p.id == id).map(|p| p.name.clone()).unwrap_or_default(),
    }
}

fn push_rows(st: &LocalState, weak: &slint::Weak<MainWindow>) {
    let now = st.now.clone();
    let rows: Vec<LocalRow> = st
        .shown
        .iter()
        .enumerate()
        .map(|(i, &k)| {
            let t = &st.tracks[k];
            let playing = now.as_deref() == Some(t.path.as_path());
            LocalRow {
                num: if playing { "\u{25B6}".into() } else { format!("{:02}", i + 1).into() },
                title: t.title.clone().into(),
                artist: local_library::shown(&t.artist).into(),
                album: local_library::shown(&t.album).into(),
                time: fmt_time(t.duration_ms).into(),
                playing,
                selected: st.selected.contains(&i),
            }
        })
        .collect();
    let total: u64 = st.shown.iter().map(|&k| st.tracks[k].duration_ms).sum();
    let albums: BTreeSet<&str> = st.shown.iter().map(|&k| st.tracks[k].album.as_str()).collect();
    let stats = if st.shown.is_empty() {
        String::new()
    } else if matches!(st.view, View::Artist(_)) {
        format!(
            "{} \u{00b7} {} \u{00b7} {}",
            lightify_i18n::trn!("{n} album", "{n} albums", albums.len()),
            plural(st.shown.len()),
            fmt_total(total)
        )
    } else {
        format!("{} \u{00b7} {}", plural(st.shown.len()), fmt_total(total))
    };
    let title = view_title(st);
    let empty = if st.tracks.is_empty() {
        if st.scanning.is_some() {
            tr!("Looking for music\u{2026}")
        } else {
            tr!("No music yet. Click + (top left) to find music on this PC, or drop audio files and folders here.")
        }
    } else if !st.filter.is_empty() {
        tr!("Nothing here matches the filter.")
    } else if matches!(st.view, View::Playlist(_)) {
        tr!("This playlist is empty \u{2014} drag tracks onto it in the list on the left, or right-click a track.")
    } else {
        tr!("Nothing here.")
    };
    let (sc, sd, nsel) = (st.sort.0, st.sort.1, st.selected.len() as i32);
    let paths: Vec<PathBuf> = st.shown.iter().map(|&k| st.tracks[k].path.clone()).collect();
    ui(weak, move |a| {
        UI_ROWS.with(|r| *r.borrow_mut() = paths);
        apply_rows(&a, rows);
        a.set_local_title(title.into());
        a.set_local_stats(stats.into());
        a.set_local_empty(empty.into());
        a.set_local_sort_col(sc);
        a.set_local_sort_desc(sd);
        a.set_local_sel_count(nsel);
    });
}

/// Put rows in the list. The same number of rows (a track started, a mark changed) is
/// edited in place: a new model makes the ListView re-place its viewport, and near the
/// end of a long list that scrolled it by a row — under the pointer that just clicked.
pub(crate) fn apply_rows(a: &MainWindow, rows: Vec<LocalRow>) {
    use slint::Model;
    let m = a.get_local_rows();
    if m.row_count() == rows.len() && m.as_any().is::<slint::VecModel<LocalRow>>() {
        for (i, r) in rows.into_iter().enumerate() {
            if m.row_data(i).as_ref() != Some(&r) {
                m.set_row_data(i, r);
            }
        }
    } else {
        a.set_local_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    }
}

fn push_find(st: &LocalState, weak: &slint::Weak<MainWindow>, open: bool, busy: bool) {
    let (cands, confirm) = find_rows(st);
    ui(weak, move |a| {
        a.set_local_cands(slint::ModelRc::from(Rc::new(slint::VecModel::from(cands))));
        a.set_local_find_busy(busy);
        a.set_local_find_confirm(confirm.into());
        a.set_local_find_open(open);
    });
}

/// The Find dialog's rows and its Add button's label.
fn find_rows(st: &LocalState) -> (Vec<LocalCand>, String) {
    let cands: Vec<LocalCand> = st
        .cands
        .iter()
        .zip(&st.cand_on)
        .map(|(c, on)| LocalCand {
            name: lightify_i18n::translate(&c.name).into(),
            path: c.path.display().to_string().into(),
            count: lightify_i18n::trn!("{}{} tracks", "{}{} tracks", c.count, thousands(c.count), if c.capped { "+" } else { "" })
                .into(),
            on: *on,
            added: c.already_added,
        })
        .collect();
    let (nf, nt) = st
        .cands
        .iter()
        .zip(&st.cand_on)
        .filter(|(c, on)| **on && !c.already_added)
        .fold((0usize, 0usize), |(f, t), (c, _)| (f + 1, t + c.count));
    let confirm = if nf == 0 {
        String::new()
    } else {
        tr!("Add {} \u{00b7} {}", lightify_i18n::trn!("{n} folder", "{n} folders", nf), plural(nt))
    };
    (cands, confirm)
}

/// After a language switch: the tree, the list's title, stats and empty line, the
/// kicker, Recent's times, and an open Find dialog or name prompt, in the new language.
fn relabel(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    if st.on || st.scanned_once || !st.tracks.is_empty() {
        push_tree(st, weak);
        push_rows(st, weak);
    }
    if st.history_loaded {
        push_recent(st, weak, None);
    }
    // An open delete confirmation of ours (Spotify's is said again by main).
    if let Some(name) = st.delete_pending.as_ref().and_then(|id| st.playlists.iter().find(|p| &p.id == id)).map(|p| p.name.clone()) {
        let msg = delete_message(&name);
        ui(weak, move |a| {
            if a.get_confirm_open() && a.get_confirm_local() {
                a.set_confirm_title(tr!("DELETE PLAYLIST").into());
                a.set_confirm_message(msg.into());
            }
        });
    }
    if !st.cands.is_empty() {
        let (cands, confirm) = find_rows(st);
        ui(weak, move |a| {
            if a.get_local_find_open() {
                a.set_local_cands(slint::ModelRc::from(Rc::new(slint::VecModel::from(cands))));
                a.set_local_find_confirm(confirm.into());
            }
        });
    }
    if let Some(note) = st.pending.as_ref().map(pending_note) {
        ui(weak, move |a| {
            if a.get_name_prompt_open() {
                a.set_name_prompt_note(note.into());
            }
        });
    }
}

fn clear_selection(st: &mut LocalState) {
    st.selected.clear();
    st.anchor = None;
}

/// "Delete playlist": open the confirm dialog (the one Spotify's delete uses) for it;
/// its Yes comes back as `DeleteConfirmed`.
fn ask_delete(st: &mut LocalState, weak: &slint::Weak<MainWindow>, id: String) {
    let Some(name) = st.playlists.iter().find(|p| p.id == id).map(|p| p.name.clone()) else { return };
    st.delete_pending = Some(id);
    let msg = delete_message(&name);
    ui(weak, move |a| {
        a.set_confirm_title(tr!("DELETE PLAYLIST").into());
        a.set_confirm_message(msg.into());
        a.set_confirm_local(true);
        a.set_confirm_open(true);
    });
}

/// The delete confirmation's message for a local playlist.
fn delete_message(name: &str) -> String {
    tr!("Delete \u{201c}{}\u{201d} from your local playlists?", name)
}

/// The row a click on display row `row` meant: where the track it showed (`path`, from
/// `UI_ROWS`) is now. `None` when that track has left the list; a click the UI had no
/// track for keeps its row number.
fn clicked_row(st: &LocalState, row: usize, path: Option<&Path>) -> Option<usize> {
    match path {
        Some(p) => row_of(st, row, p),
        None => (row < st.shown.len()).then_some(row),
    }
}

/// The selection when `row` is part of it, else just `row` — as paths, in view order.
fn rows_or_selection(st: &LocalState, row: usize) -> Vec<PathBuf> {
    let rows: Vec<usize> = if st.selected.contains(&row) { st.selected.iter().copied().collect() } else { vec![row] };
    rows.iter().filter_map(|r| st.shown.get(*r)).map(|&k| st.tracks[k].path.clone()).collect()
}

fn classify(paths: &[PathBuf]) -> (usize, usize, usize) {
    let (mut audio, mut folders, mut skipped) = (0, 0, 0);
    for p in paths {
        if p.is_dir() {
            folders += 1;
        } else if local_library::is_audio(p) {
            audio += 1;
        } else {
            skipped += 1;
        }
    }
    (audio, folders, skipped)
}

/// What the name prompt says above the field for `p`.
fn pending_note(p: &Pending) -> String {
    match p {
        Pending::NewPlaylist(paths) if paths.is_empty() => tr!("A new local playlist"),
        Pending::NewPlaylist(paths) => {
            lightify_i18n::trn!("{n} track for a new local playlist", "{n} tracks for a new local playlist", paths.len())
        }
        Pending::Rename(_) => tr!("Rename the playlist"),
    }
}

fn ask_name(st: &mut LocalState, weak: &slint::Weak<MainWindow>, p: Pending) {
    let note = pending_note(&p);
    st.pending = Some(p);
    ui(weak, move |a| {
        a.set_name_prompt_side(0);
        a.set_name_prompt_note(note.into());
        a.set_name_prompt_open(true);
    });
}

// ── Playback ─────────────────────────────────────────────────────────────────

fn pause_spotify() {
    if crate::engine::running() {
        crate::engine::send(r#"{"cmd":"pause"}"#);
    }
}

fn shuffled(mut v: Vec<PathBuf>, first: Option<&Path>) -> Vec<PathBuf> {
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
    for i in (1..v.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        v.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    if let Some(f) = first {
        if let Some(at) = v.iter().position(|p| p == f) {
            let p = v.remove(at);
            v.insert(0, p);
        }
    }
    v
}

/// Play the shown list from display row `row` (shuffled behind it when shuffle is on).
fn play_from(st: &mut LocalState, row: usize, weak: &slint::Weak<MainWindow>) {
    let list: Vec<PathBuf> = st.shown.iter().map(|&k| st.tracks[k].path.clone()).collect();
    play_list(st, list, row, weak);
}

/// Play `list` from `row`, the rest following (shuffled when shuffle is on).
fn play_list(st: &mut LocalState, list: Vec<PathBuf>, row: usize, weak: &slint::Weak<MainWindow>) {
    if list.is_empty() {
        return;
    }
    // The user picked something: a new run of skipped files starts from here.
    st.fails = 0;
    let row = row.min(list.len() - 1);
    st.source = list.clone();
    if st.shuffle {
        let first = list[row].clone();
        st.order = shuffled(list, Some(&first));
        st.pos = 0;
    } else {
        st.order = list;
        st.pos = row;
    }
    start_current(st, weak);
}

/// Every local `play` goes through here: Spotify paused, the volume applied (the
/// output's own level is 100% until told otherwise), the generation noted so older
/// players' events are told apart. `play` clears the player's next file; `set_next`
/// runs again when this file's `Track` comes.
fn play_path(st: &mut LocalState, path: PathBuf, ms: u64) {
    pause_spotify();
    local_player::set_volume(st.volume);
    st.play_gen = local_player::play(&path, ms);
    st.next_src = None;
    st.now = Some(path);
    st.now_ms = ms;
}

fn start_current(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    let Some(p) = st.order.get(st.pos).cloned() else { return };
    play_path(st, p, 0);
    push_rows(st, weak);
    push_now(st, weak);
}

fn rebuild_order_keep_current(st: &mut LocalState) {
    let Some(cur) = st.now.clone() else { return };
    // A song from the user queue isn't in the order; the order's own place is where
    // the list resumes after it.
    let anchor = if st.order.contains(&cur) { Some(cur) } else { st.order.get(st.pos).cloned() };
    if st.shuffle {
        st.order = shuffled(st.order.clone(), anchor.as_deref());
        st.pos = 0;
    } else {
        // Back to the list that was playing, not whatever list is on screen now.
        st.pos = anchor.and_then(|a| st.source.iter().position(|p| *p == a)).unwrap_or(0);
        st.order = st.source.clone();
    }
    set_next(st);
}

/// The file after the current one, by queue, repeat and order (without moving), and
/// where it comes from.
fn peek_next(st: &LocalState) -> Option<(NextSrc, PathBuf)> {
    if let Some(q) = st.user_queue.front() {
        return Some((NextSrc::Queue, q.clone()));
    }
    if st.repeat == 2 {
        return st.now.clone().map(|p| (NextSrc::Same, p));
    }
    match st.order.get(st.pos + 1) {
        Some(p) => Some((NextSrc::Order(st.pos + 1), p.clone())),
        None if st.repeat == 1 => st.order.first().map(|p| (NextSrc::Order(0), p.clone())),
        None => None,
    }
}

fn set_next(st: &mut LocalState) {
    st.next_src = peek_next(st);
    local_player::set_next(st.next_src.as_ref().map(|(_, p)| p.clone()));
}

/// The player went on to `path` by itself (the gapless next file started, or failed
/// to): step the queue or the order the way `set_next` said, by position — searching
/// for the path instead loops a list that holds it twice, and leaves a queued copy of
/// the playing file at the front of the queue for good.
fn follow_next(st: &mut LocalState, path: &Path) {
    match st.next_src.take() {
        Some((src, p)) if p == path => match src {
            NextSrc::Queue => {
                st.user_queue.pop_front();
            }
            NextSrc::Order(i) => st.pos = i,
            NextSrc::Same => {}
        },
        // Not what was asked last (that `set_next` reached the player too late): find it,
        // the queue first, then the order from just after the current place.
        _ => {
            if st.user_queue.front().is_some_and(|q| q == path) {
                st.user_queue.pop_front();
            } else if let Some(i) = (st.pos + 1..st.order.len()).chain(0..=st.pos).find(|&i| st.order.get(i).is_some_and(|p| p == path)) {
                st.pos = i;
            }
        }
    }
    st.now = Some(path.to_path_buf());
}

/// Step `dir` (+1 / -1) through the order (the user queue first, going forward).
/// Whether something started (or, at the very start, the track restarted).
fn advance(st: &mut LocalState, weak: &slint::Weak<MainWindow>, dir: i32) -> bool {
    if dir > 0 {
        if let Some(q) = st.user_queue.pop_front() {
            play_path(st, q, 0);
            push_rows(st, weak);
            push_now(st, weak);
            return true;
        }
        if st.pos + 1 < st.order.len() {
            st.pos += 1;
        } else if st.repeat == 1 && !st.order.is_empty() {
            st.pos = 0;
        } else {
            return false;
        }
    } else if st.pos > 0 {
        st.pos -= 1;
    } else if st.repeat == 1 && !st.order.is_empty() {
        st.pos = st.order.len() - 1;
    } else {
        local_player::seek(0);
        return true;
    }
    start_current(st, weak);
    true
}

/// Where a Queue drawer item is now: `src` while it still holds `path`, else where
/// `path` moved to in the same part of the queue (rows are rebuilt on every track
/// change, so a menu or a click can be a step behind). `None` when it is gone.
fn queue_item(st: &LocalState, src: QSrc, path: &Path) -> Option<QSrc> {
    match src {
        QSrc::Current => (st.now.as_deref() == Some(path)).then_some(QSrc::Current),
        QSrc::User(k) => {
            if st.user_queue.get(k).is_some_and(|p| p == path) {
                return Some(src);
            }
            st.user_queue.iter().position(|p| p == path).map(QSrc::User)
        }
        QSrc::Order(k) => {
            if k > st.pos && st.order.get(k).is_some_and(|p| p == path) {
                return Some(src);
            }
            (st.pos + 1..st.order.len()).find(|&i| st.order[i] == path).map(QSrc::Order)
        }
    }
}

fn queue_play(st: &mut LocalState, src: QSrc, path: &Path, weak: &slint::Weak<MainWindow>) {
    st.fails = 0;
    match queue_item(st, src, path) {
        Some(QSrc::Current) => local_player::seek(0),
        Some(QSrc::User(k)) => {
            if let Some(p) = st.user_queue.remove(k) {
                play_path(st, p, 0);
                push_rows(st, weak);
                push_now(st, weak);
            }
        }
        Some(QSrc::Order(k)) => {
            st.pos = k;
            start_current(st, weak);
        }
        None => {}
    }
}

fn queue_remove(st: &mut LocalState, src: QSrc, path: &Path, weak: &slint::Weak<MainWindow>) {
    match queue_item(st, src, path) {
        Some(QSrc::User(k)) => {
            st.user_queue.remove(k);
        }
        Some(QSrc::Order(k)) => {
            let gone = st.order.remove(k);
            // ...and out of the list shuffle-off goes back to, so it stays gone.
            if let Some(at) = st.source.iter().position(|p| *p == gone) {
                st.source.remove(at);
            }
        }
        _ => return,
    }
    set_next(st);
    push_now(st, weak);
}

fn on_player(st: &mut LocalState, ev: local_player::Event, weak: &slint::Weak<MainWindow>, tx: &Tx) {
    match ev {
        local_player::Event::Track { path, duration_ms, next } => {
            // A gapless handoff: follow it in the order / queue. (A `play`'s own file is
            // already `now`.)
            if next {
                follow_next(st, &path);
            }
            st.dur_ms = duration_ms;
            st.now_ms = 0;
            st.first_pos = None;
            st.playing = true;
            add_local_play(st, path.clone(), weak, tx);
            set_next(st);
            push_rows(st, weak);
            push_now(st, weak);
            ui(weak, |a| a.set_local_now_cover(slint::Image::default()));
            let tx = tx.clone();
            std::thread::spawn(move || {
                // Sized for the now-playing view (up to 440 px, ×1.5 for high DPI).
                let px = local_library::cover(&path).and_then(|b| image::load_from_memory(&b).ok()).map(|img| {
                    let img = img.thumbnail(660, 660);
                    let glow = crate::dominant_color(&img);
                    let img = img.to_rgba8();
                    let (w, h) = img.dimensions();
                    (img.into_raw(), w, h, glow)
                });
                send(&tx, LocalCmd::Cover(path, px));
            });
        }
        local_player::Event::Position { ms } => {
            st.now_ms = ms;
            // The clock moved on from where this file started: it really plays.
            match st.first_pos {
                None => st.first_pos = Some(ms),
                Some(first) if ms > first => st.fails = 0,
                Some(_) => {}
            }
            let (e, p) = (fmt_time(ms), if st.dur_ms > 0 { (ms as f32 / st.dur_ms as f32).clamp(0.0, 1.0) } else { 0.0 });
            ui(weak, move |a| {
                a.set_local_now_elapsed(e.into());
                a.set_local_now_progress(p);
            });
        }
        local_player::Event::Playing { playing } => {
            st.playing = playing;
            ui(weak, move |a| a.set_local_now_playing(playing));
        }
        local_player::Event::Ended => {
            st.playing = false;
            st.now_ms = 0;
            // A next file was asked for but came after the player had finished: play it.
            if let Some((_, p)) = st.next_src.clone() {
                follow_next(st, &p);
                play_path(st, p, 0);
                push_rows(st, weak);
            }
            push_now(st, weak);
        }
        local_player::Event::Error { path, msg } => {
            st.playing = false;
            st.fails += 1;
            let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            // The next file failed to start: it takes its place in the order, so the
            // list moves on past it, not back to it.
            if st.now.as_deref() != Some(path.as_path()) {
                follow_next(st, &path);
            }
            let msg = if st.fails >= MAX_FAILS {
                lightify_i18n::trn!(
                    "Can\u{2019}t play \u{201c}{0}\u{201d} \u{2014} {1}. Stopped: {n} file in a row couldn\u{2019}t be played",
                    "Can\u{2019}t play \u{201c}{0}\u{201d} \u{2014} {1}. Stopped: {n} files in a row couldn\u{2019}t be played",
                    st.fails as usize,
                    name,
                    msg
                )
            } else if advance(st, weak, 1) {
                tr!("Can\u{2019}t play \u{201c}{}\u{201d} \u{2014} {}. Skipped it", name, msg)
            } else {
                tr!("Can\u{2019}t play \u{201c}{}\u{201d} \u{2014} {}", name, msg)
            };
            crate::set_status(weak, msg);
            push_rows(st, weak);
            push_now(st, weak);
        }
    }
}

// ── Recent (Local mode) ──────────────────────────────────────────────────────

const HISTORY_KEEP: usize = 100;

fn history_path() -> PathBuf {
    lightify_core::config::data_dir().join("lightify_shell_local_history.json")
}

/// Read the history file once it can be. A file that is there but can't be read now
/// leaves `history_loaded` false (tried again next time) and the plays since kept in
/// memory; once read, those plays go on top of what it held.
fn ensure_history(st: &mut LocalState) {
    ensure_history_from(st, &history_path());
}

fn ensure_history_from(st: &mut LocalState, file: &Path) {
    if st.history_loaded {
        return;
    }
    let Ok(saved) = crate::play_history::read_file::<Vec<(PathBuf, String)>>(file) else { return };
    st.history_loaded = true;
    st.history.extend(saved.unwrap_or_default());
    st.history.truncate(HISTORY_KEEP);
}

/// A local file just started playing (`local_player::Event::Track`): to the top of
/// Local's Recent, at once.
fn add_local_play(st: &mut LocalState, path: PathBuf, weak: &slint::Weak<MainWindow>, tx: &Tx) {
    add_local_play_to(st, path, &history_path());
    push_recent(st, weak, Some(tx));
}

fn add_local_play_to(st: &mut LocalState, path: PathBuf, file: &Path) {
    ensure_history_from(st, file);
    // Lightify reopened and resumed the file already at the top: the same listen.
    if st.history.first().is_some_and(|(p, at)| *p == path && crate::play_history::continues(at, st.dur_ms)) {
        return;
    }
    // It is playing, so it is there.
    st.recent_known.insert(path.clone(), true);
    st.history.insert(0, (path, crate::play_history::now_iso()));
    st.history.truncate(HISTORY_KEEP);
    // Not written over a file that couldn't be read (it would lose what it holds).
    if st.history_loaded {
        if let Ok(b) = serde_json::to_vec(&st.history) {
            let _ = lightify_core::config::write_atomic(file, &b);
        }
    }
}

/// Look up, on a thread of its own, whether the history's files outside the library
/// still exist: those not known yet, or (`all`) every one. One lookup at a time.
fn check_recent(st: &mut LocalState, tx: &Tx, all: bool) {
    if st.recent_checking {
        return;
    }
    let lib: std::collections::HashSet<&Path> = st.tracks.iter().map(|t| t.path.as_path()).collect();
    let mut paths: Vec<PathBuf> = st
        .history
        .iter()
        .map(|(p, _)| p)
        .filter(|p| !lib.contains(p.as_path()) && (all || !st.recent_known.contains_key(*p)))
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return;
    }
    st.recent_checking = true;
    let tx = tx.clone();
    std::thread::spawn(move || {
        let found = paths.into_iter().map(|p| {
            let ok = p.is_file();
            (p, ok)
        });
        send(&tx, LocalCmd::RecentChecked(found.collect()));
    });
}

/// The Recent drawer in Local mode: the files played, newest first. A file outside the
/// library shows once it is known to exist (looked up off the worker: `check_recent`,
/// when `tx` is given); gone ones are left out.
fn push_recent(st: &mut LocalState, weak: &slint::Weak<MainWindow>, tx: Option<&Tx>) {
    ensure_history(st);
    if let Some(tx) = tx {
        check_recent(st, tx, false);
    }
    let by_path: std::collections::HashMap<&Path, &LocalTrack> = st.tracks.iter().map(|t| (t.path.as_path(), t)).collect();
    let mut view = Vec::new();
    let mut rows = Vec::new();
    for (p, at) in &st.history {
        let t = by_path.get(p.as_path());
        if t.is_none() && st.recent_known.get(p) != Some(&true) {
            continue;
        }
        view.push(p.clone());
        rows.push(crate::SideRow {
            name: t.map(|t| t.title.clone()).unwrap_or_else(|| p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()).into(),
            artist: t.map(|t| local_library::shown(&t.artist)).unwrap_or_default().into(),
            time: crate::rel_time(at).into(),
            playing: st.playing && st.now.as_deref() == Some(p.as_path()),
            selected: false,
            pending: 0,
        });
    }
    st.rview = view;
    ui(weak, move |a| a.set_local_recent_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows)))));
}

/// The Queue drawer in Local mode: now playing, then the user queue, then the rest of
/// the play order.
fn push_queue(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    let mut q: Vec<(QSrc, PathBuf)> = Vec::new();
    if let Some(p) = &st.now {
        q.push((QSrc::Current, p.clone()));
    }
    q.extend(st.user_queue.iter().enumerate().map(|(k, p)| (QSrc::User(k), p.clone())));
    q.extend(st.order.iter().enumerate().skip(st.pos + 1).take(500).map(|(k, p)| (QSrc::Order(k), p.clone())));
    let by_path: std::collections::HashMap<&Path, &LocalTrack> = st.tracks.iter().map(|t| (t.path.as_path(), t)).collect();
    let rows: Vec<crate::SideRow> = q
        .iter()
        .map(|(src, p)| {
            let t = by_path.get(p.as_path());
            crate::SideRow {
                name: t.map(|t| t.title.clone()).unwrap_or_else(|| p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()).into(),
                artist: t.map(|t| local_library::shown(&t.artist)).unwrap_or_default().into(),
                time: t.map(|t| fmt_time(t.duration_ms)).unwrap_or_default().into(),
                playing: *src == QSrc::Current,
                selected: false,
                pending: 0,
            }
        })
        .collect();
    st.qview = q.clone();
    ui(weak, move |a| {
        UI_QUEUE.with(|r| *r.borrow_mut() = q);
        a.set_local_queue_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    });
}

fn push_now(st: &mut LocalState, weak: &slint::Weak<MainWindow>) {
    push_queue(st, weak);
    let t = st.now.as_ref().and_then(|p| st.tracks.iter().find(|t| &t.path == p).cloned());
    let has = st.now.is_some();
    let title = t
        .as_ref()
        .map(|t| t.title.clone())
        .or_else(|| st.now.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_default();
    let artist = t.as_ref().map(|t| local_library::shown(&t.artist)).unwrap_or_default();
    let album = t.as_ref().map(|t| local_library::shown(&t.album)).unwrap_or_default();
    let dur = if st.dur_ms > 0 { st.dur_ms } else { t.as_ref().map(|t| t.duration_ms).unwrap_or(0) };
    let (elapsed, duration) = (fmt_time(st.now_ms), fmt_time(dur));
    let progress = if dur > 0 { (st.now_ms as f32 / dur as f32).clamp(0.0, 1.0) } else { 0.0 };
    let (playing, shuffle, repeat, volume) = (st.playing, st.shuffle, st.repeat, st.volume);
    ui(weak, move |a| {
        a.set_local_now_has(has);
        a.set_local_now_title(title.into());
        a.set_local_now_artist(artist.into());
        a.set_local_now_album(album.into());
        a.set_local_now_elapsed(if has { elapsed.into() } else { "".into() });
        a.set_local_now_duration(if has { duration.into() } else { "".into() });
        a.set_local_now_progress(progress);
        a.set_local_now_playing(playing);
        a.set_local_shuffle(shuffle);
        a.set_local_repeat(repeat);
        a.set_local_volume(volume);
    });
}

// ── Windows shell helpers ────────────────────────────────────────────────────

fn open_path(p: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer").raw_arg(quoted_arg(p)).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(not(windows))]
    let _ = p;
}

fn show_in_folder(p: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer").raw_arg(select_arg(p)).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(not(windows))]
    let _ = p;
}

/// A folder for Explorer to open, quoted and passed raw: Explorer splits its command
/// line at commas, and the standard quoting leaves a path without spaces bare, so
/// "Earth,Wind" opened Explorer's default folder instead (as `select_arg`).
#[cfg(windows)]
fn quoted_arg(p: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from("\"");
    arg.push(p.as_os_str());
    arg.push("\"");
    arg
}

/// Explorer's `/select,"<path>"`, passed raw. Explorer splits its own command line at
/// commas, and the standard quoting wraps the whole `/select,…` argument (or, with no
/// space in the path, nothing at all), so a path with a comma selected nothing. A
/// Windows path can't contain `"`, so the quotes need no escaping.
#[cfg(windows)]
fn select_arg(p: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from("/select,\"");
    arg.push(p.as_os_str());
    arg.push("\"");
    arg
}

#[cfg(all(test, windows))]
#[test]
fn a_folder_to_open_is_quoted_whole_commas_and_all() {
    assert_eq!(quoted_arg(Path::new(r"D:\Music\Earth,Wind")), std::ffi::OsString::from(r#""D:\Music\Earth,Wind""#));
    assert_eq!(quoted_arg(Path::new(r"C:\A, B\C")), std::ffi::OsString::from(r#""C:\A, B\C""#));
}

#[cfg(all(test, windows))]
#[test]
fn select_arg_quotes_paths_with_commas_and_spaces() {
    let p = Path::new(r"C:\Music\Earth, Wind & Fire\01 September.flac");
    assert_eq!(select_arg(p), std::ffi::OsString::from(r#"/select,"C:\Music\Earth, Wind & Fire\01 September.flac""#));
}

/// The native picker: a folder (`folder`) or audio files. Empty when cancelled.
#[cfg(windows)]
fn pick(folder: bool) -> Vec<PathBuf> {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    let mut out = Vec::new();
    // The dialog's own text in the active language (runtime strings, kept alive until
    // it closes); the file pattern is not text.
    let (title, kind) = if folder {
        (HSTRING::from(tr!("Add a music folder to Lightify")), HSTRING::new())
    } else {
        (HSTRING::from(tr!("Add music files to Lightify")), HSTRING::from(tr!("Audio files")))
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(dlg) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) else { return out };
        let Ok(mut opts) = dlg.GetOptions() else { return out };
        opts |= FOS_FORCEFILESYSTEM;
        let _ = dlg.SetTitle(&title);
        if folder {
            opts |= FOS_PICKFOLDERS | FOS_ALLOWMULTISELECT;
        } else {
            opts |= FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST;
            let spec = [COMDLG_FILTERSPEC {
                pszName: PCWSTR(kind.as_ptr()),
                pszSpec: w!("*.mp3;*.flac;*.wav;*.aif;*.aiff;*.aifc;*.m4a;*.m4b;*.aac;*.ogg;*.oga;*.caf;*.mp2;*.mp1"),
            }];
            let _ = dlg.SetFileTypes(&spec);
        }
        let _ = dlg.SetOptions(opts);
        if dlg.Show(None).is_err() {
            return out;
        }
        let Ok(items) = dlg.GetResults() else { return out };
        let n = items.GetCount().unwrap_or(0);
        for i in 0..n {
            if let Ok(item) = items.GetItemAt(i) {
                if let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                    let s = PCWSTR(name.0).to_string().unwrap_or_default();
                    CoTaskMemFree(Some(name.0 as *const _));
                    if !s.is_empty() {
                        out.push(PathBuf::from(s));
                    }
                }
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn pick(_folder: bool) -> Vec<PathBuf> {
    Vec::new()
}

// ── Headless render (`--shot-local`) ─────────────────────────────────────────

/// Fill the Local view with sample data for a render: `library` (an artist open, a
/// track playing), `find` (the Find dialog), `drop` (an Explorer drag over it),
/// `carry` (tracks carried onto a playlist), `now` (the now-playing view), `filters`
/// (the filter panel, Music only + WAV off), `queue` (the Queue drawer), `filtered`
/// (the library filtered by "Ellis", the box showing it), `menu` (a playlist row's
/// menu with 24 playlists in it; `menu-short` with the usual two), `confirm` (deleting a local playlist), `find-many`
/// (the Find dialog with more folders than fit) or `empty`.
pub fn shot(app: &MainWindow, variant: &str) {
    SHOT.with(|s| *s.borrow_mut() = Some(app.clone_strong()));
    let weak = app.as_weak();
    let mut st = LocalState { on: true, ..Default::default() };
    if variant != "empty" {
        let albums: [(&str, &str, &str, &[(&str, u64)]); 4] = [
            ("Quiet Harbour", "Ellis Marrow", "Ambient", &[("Low Light", 251_000), ("Tidewater", 318_000), ("Harbour Lamps", 204_000), ("Night Ferry", 389_000), ("Salt Air", 276_000), ("Moorings", 341_000)]),
            ("Paper Rooms", "Ellis Marrow", "Ambient", &[("Paper Rooms", 296_000), ("Draft", 188_000), ("Long Hall", 412_000), ("Ink", 233_000)]),
            ("Static Bloom", "Ada Merrow", "Electronic", &[("Static Bloom", 302_000), ("Soft Machines", 275_000), ("Late Checkout", 344_000)]),
            ("Field Notes", "Tess Ardent", "Folk", &[("Basement Hymn", 241_000), ("Cold Pressed", 199_000), ("Glasshouse", 262_000)]),
        ];
        let root = PathBuf::from(r"C:\Users\you\Music");
        for (ai, (album, artist, genre, songs)) in albums.iter().enumerate() {
            for (k, (title, ms)) in songs.iter().enumerate() {
                st.tracks.push(LocalTrack {
                    path: root.join(artist).join(album).join(format!("{:02} {title}.flac", k + 1)),
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    album_artist: artist.to_string(),
                    genre: genre.to_string(),
                    track_no: Some(k as u32 + 1),
                    disc_no: Some(1),
                    duration_ms: *ms,
                    added_at: 1_760_000_000 + (ai * 10 + k) as u64,
                });
            }
        }
        // A sound designer's one-shots: what the filters are for.
        for (k, (name, ms)) in [("Kick 01", 640u64), ("Snare Room", 1_150), ("Riser 8 bar", 14_800)].iter().enumerate() {
            st.tracks.push(LocalTrack {
                path: root.join("Samples").join(format!("{name}.wav")),
                title: name.to_string(),
                artist: "Samples".into(),
                album: "One-shots".into(),
                album_artist: "Samples".into(),
                genre: String::new(),
                track_no: None,
                disc_no: None,
                duration_ms: *ms,
                added_at: 1_759_000_000 + k as u64,
            });
        }
        st.folders = vec![root.clone(), PathBuf::from(r"D:\Downloads\Lightify")];
        let pl = |id: &str, name: &str, idx: &[usize]| LocalPlaylist {
            id: id.into(),
            name: name.into(),
            tracks: idx.iter().map(|&i| st.tracks[i].path.clone()).collect(),
        };
        st.playlists = vec![pl("lp1", "Late night", &[1, 3, 8, 11]), pl("lp2", "Focus", &[0, 6, 7])];
        if variant == "menu" {
            // Enough playlists that the row menu is taller than the window.
            for i in 1..=22 {
                st.playlists.push(pl(&format!("lpm{i}"), &format!("Mix {i:02}"), &[i % 12]));
            }
        }
        st.view = View::Artist("Ellis Marrow".into());
        st.selected = BTreeSet::from([4, 5]);
        st.now = Some(st.tracks[1].path.clone());
        st.dur_ms = st.tracks[1].duration_ms;
        st.now_ms = 97_000;
        st.playing = true;
        if variant == "filters" {
            st.filters = Filters { mode: 1, min: 0, min_s: 30, max_s: 15 * 60, hidden_types: vec!["wav".into()] };
            // A mixed-format library, so the type chips fill more than one row.
            for (t, ext) in st.tracks.iter_mut().zip(["mp3", "aiff", "m4a", "ogg", "mp2", "mp3", "caf"]) {
                t.path.set_extension(ext);
            }
            st.view = View::All;
            st.selected.clear();
        }
        if variant.starts_with("menu") {
            st.view = View::Playlist("lp1".into());
            st.selected.clear();
        }
        if variant == "filtered" {
            // The filter box shows the filter that is on (its text lives on the window).
            st.filter = "ellis".into();
            st.view = View::All;
            st.selected.clear();
            app.set_local_filter_text("Ellis".into());
        }
        build_shown(&mut st);
        st.order = st.shown.iter().map(|&k| st.tracks[k].path.clone()).collect();
        st.pos = 1;
        st.user_queue = VecDeque::from([st.tracks[12].path.clone()]);
    }
    refresh(&mut st, &weak);
    match variant {
        "find" => {
            let c = |name: &str, path: &str, count: usize, capped: bool, added: bool| Candidate {
                name: name.into(),
                path: path.into(),
                count,
                capped,
                already_added: added,
            };
            st.cands = vec![
                c("Music", r"C:\Users\you\Music", 1_284, false, true),
                c("Downloads", r"C:\Users\you\Downloads", 37, false, false),
                c("Lightify", r"D:\Downloads\Lightify", 412, false, false),
                c("Archive", r"E:\Archive\Audio", 5_000, true, false),
            ];
            st.cand_on = vec![false, true, true, false];
            push_find(&st, &weak, true, false);
        }
        // More suggestions than the dialog has room for: the list scrolls, the buttons stay.
        "find-many" => {
            st.cands = (0..9)
                .map(|i| Candidate {
                    name: ["Music", "Downloads", "Desktop", "OneDrive \u{2014} Music", "Lightify downloads"]
                        .get(i)
                        .map_or_else(|| format!(r"{}:\Music", (b'D' + i as u8 - 5) as char), |s| s.to_string()),
                    path: PathBuf::from(format!(r"C:\Users\you\Folder{i}")),
                    count: 40 + i * 37,
                    capped: false,
                    already_added: i == 0,
                })
                .collect();
            st.cand_on = (0..9).map(|i| i % 2 == 1).collect();
            push_find(&st, &weak, true, false);
        }
        "menu" | "menu-short" => {
            let items = row_menu(&st, 1);
            app.set_ctx_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(crate::ctx_rows(&items)))));
            app.set_ctx_w(crate::menu_width(&items));
            app.set_ctx_x(420.0);
            app.set_ctx_y(150.0);
            app.set_ctx_open(true);
        }
        "confirm" => ask_delete(&mut st, &weak, "lp1".into()),
        "drop" => {
            let detail = [
                lightify_i18n::trn!("{n} audio file", "{n} audio files", 3),
                lightify_i18n::trn!("{n} folder", "{n} folders", 1),
                lightify_i18n::trn!("{n} skipped (not audio)", "{n} skipped (not audio)", 1),
            ];
            app.set_local_drop_detail(detail.join(" \u{00b7} ").into());
            app.set_local_drop_active(true);
        }
        "now" => app.set_local_now_open(true),
        "filters" => {
            push_len_fields(&st, &weak, None);
            app.set_local_filters_open(true);
        }
        "queue" => app.set_sidebar_mode(1),
        "recent" => {
            // Sample plays (never the user's history file): 2 min, 40 min, 3 h, 2 days ago.
            let ago = |secs: i64| {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                crate::play_history::iso_from_unix(now - secs)
            };
            st.history_loaded = true;
            st.history = [(3, 120), (1, 2_400), (6, 10_800), (9, 172_800)]
                .iter()
                .map(|&(i, secs)| (st.tracks[i].path.clone(), ago(secs)))
                .collect();
            push_recent(&mut st, &weak, None);
            app.set_sidebar_mode(2);
        }
        // Both histories present, in the Spotify view: RECENT must show only Spotify's.
        "recent-spotify" => {
            st.history_loaded = true;
            st.history = vec![(st.tracks[3].path.clone(), crate::play_history::now_iso())];
            push_recent(&mut st, &weak, None);
            let row = |n: &str, a: &str, t: &str| crate::SideRow { name: n.into(), artist: a.into(), time: t.into(), playing: false, selected: false, pending: 0 };
            app.set_sidebar_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
                row("Spotify song one", "Spotify artist", "just now"),
                row("Spotify song two", "Spotify artist", "3m"),
            ]))));
            app.set_sidebar_mode(2);
        }
        "collapsed" => app.set_left_collapsed(true),
        "mini" => app.set_mini_mode("square".into()),
        "minibar" => app.set_mini_mode("bar".into()),
        "carry" => {
            app.set_local_carry_what(plural(2).into());
            app.set_local_carry_active(true);
            app.set_local_carry_x(118.0);
            app.set_local_carry_y(420.0);
            let target = st.nodes.iter().position(|n| n.2.as_deref() == Some("lp1")).map(|i| i as i32).unwrap_or(-1);
            app.set_local_drop_target(target);
        }
        _ => {}
    }
    // A cover tint unlike the default teal, so a shot shows the accent following it.
    app.set_local_now_glow(slint::Color::from_argb_u8(0x55, 0xe0, 0x60, 0x7a));
    // Every Local shot is in Local mode — except the one checking the Spotify view.
    app.set_local_mode(variant != "recent-spotify");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(name: &str, ms: u64) -> LocalTrack {
        LocalTrack {
            path: PathBuf::from(format!("C:/m/{name}")),
            title: name.into(),
            artist: String::new(),
            album: String::new(),
            album_artist: String::new(),
            genre: String::new(),
            track_no: None,
            disc_no: None,
            duration_ms: ms,
            added_at: 0,
        }
    }

    #[test]
    fn filters_hide_by_length_and_type() {
        let clip = track("kick.wav", 900);
        let song = track("song.flac", 215_000);
        let set = track("set.mp3", 3_600_000);
        let unknown = track("odd.ogg", 0);
        let mut f = Filters::default();
        assert!([&clip, &song, &set, &unknown].iter().all(|t| f.passes(t)), "nothing hidden by default");
        f.mode = 1; // music only
        assert!(!f.passes(&clip) && f.passes(&song) && f.passes(&set), "music only hides clips, keeps songs and sets");
        assert!(f.passes(&unknown), "an unread length is never hidden for it");
        f.mode = 2; // long sets
        assert!(!f.passes(&song) && f.passes(&set), "long sets keeps 20:00+ only");
        f.mode = 0;
        f.min_s = 60;
        assert!(!f.passes(&clip) && f.passes(&song));
        f.min_s = 0;
        f.max_s = 10 * 60; // nothing longer than 10:00
        assert!(f.passes(&song) && !f.passes(&set), "the longest limit hides the set");
        assert!(f.passes(&unknown), "an unread length is never hidden for it");
        f.max_s = 0;
        f.hidden_types = vec!["wav".into()];
        assert!(!f.passes(&clip) && f.passes(&song));
        f.hidden_types = vec!["ogg".into()];
        assert!(!f.passes(&track("x.oga", 200_000)), ".oga counts as OGG");
    }

    #[test]
    fn lengths_parse_and_format() {
        for (t, want) in [
            ("", Some(0)),
            ("90", Some(90)),
            ("1:30", Some(90)),
            (" 0:05 ", Some(5)),
            ("1:02:00", Some(3720)),
            ("90s", Some(90)),
            ("5m", Some(300)),
            ("1.5 min", Some(90)),
            ("1h", Some(3600)),
            ("1:75", None),
            ("abc", None),
            ("1::2", None),
            ("5 parsecs", None),
        ] {
            assert_eq!(parse_len(t), want, "{t:?}");
        }
        assert_eq!(fmt_len(0), "");
        assert_eq!(fmt_len(30), "0:30");
        assert_eq!(fmt_len(245), "4:05");
        assert_eq!(fmt_len(3720), "1:02:00");
        assert_eq!(ext_of(Path::new("a.AIF")), "aiff");
        assert_eq!(ext_of(Path::new("a.oga")), "ogg");
    }

    #[test]
    fn queue_is_now_then_user_queue_then_order() {
        let mut st = LocalState::default();
        let p = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        st.order = vec![p("a"), p("b"), p("c"), p("d")];
        st.pos = 1;
        st.now = Some(p("b"));
        st.user_queue = VecDeque::from([p("q")]);
        let weak = slint::Weak::<MainWindow>::default();
        push_queue(&mut st, &weak);
        let got: Vec<(QSrc, PathBuf)> = st.qview.clone();
        assert_eq!(
            got,
            vec![(QSrc::Current, p("b")), (QSrc::User(0), p("q")), (QSrc::Order(2), p("c")), (QSrc::Order(3), p("d"))]
        );
        // What plays after "b" is the user's queue first.
        assert_eq!(peek_next(&st), Some((NextSrc::Queue, p("q"))));
    }

    /// What the player's gapless handoffs do to the state: `set_next`'s choice, then
    /// the `Track` for it (without the player, the history file or a cover thread).
    fn hand_off(st: &mut LocalState) -> Option<PathBuf> {
        st.next_src = peek_next(st);
        let (_, p) = st.next_src.clone()?;
        follow_next(st, &p);
        Some(p)
    }

    #[test]
    fn gapless_follows_a_playlist_with_repeats_by_position() {
        let mut st = LocalState::default();
        let p = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        st.order = vec![p("a"), p("b"), p("a"), p("c")];
        st.pos = 0;
        st.now = Some(p("a"));
        let played: Vec<PathBuf> = std::iter::from_fn(|| hand_off(&mut st)).take(10).collect();
        // Searching for the path went a, b, a(=0), b, a, ... for ever.
        assert_eq!(played, [p("b"), p("a"), p("c")]);
        assert_eq!(st.pos, 3);
    }

    #[test]
    fn a_queued_copy_of_the_playing_track_plays_once() {
        let mut st = LocalState::default();
        let p = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        st.order = vec![p("a"), p("b")];
        st.now = Some(p("a"));
        st.user_queue = VecDeque::from([p("a")]);
        let played: Vec<PathBuf> = std::iter::from_fn(|| hand_off(&mut st)).take(10).collect();
        assert_eq!(played, [p("a"), p("b")]);
        assert!(st.user_queue.is_empty());
        // Repeat one plays the same file again without moving.
        st.repeat = 2;
        assert_eq!(hand_off(&mut st), Some(p("b")));
        assert_eq!(st.pos, 1);
    }

    #[test]
    fn a_handoff_the_player_got_too_late_is_found_after_the_current_place() {
        let mut st = LocalState::default();
        let p = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        st.order = vec![p("x"), p("a"), p("x"), p("y")];
        st.pos = 1;
        st.now = Some(p("a"));
        // The player went on to "x" while the list had asked for something else.
        st.next_src = Some((NextSrc::Queue, p("q")));
        follow_next(&mut st, &p("x"));
        assert_eq!(st.pos, 2, "the x after the current place, not the first one");
    }

    fn lib(names: &[&str]) -> Vec<LocalTrack> {
        names.iter().map(|n| track(n, 1000)).collect()
    }

    #[test]
    fn menu_actions_carry_what_was_right_clicked() {
        let mut st = LocalState::default();
        st.tracks = lib(&["a", "b", "c", "d"]);
        let id = "lp1".to_string();
        st.playlists = vec![LocalPlaylist {
            id: id.clone(),
            name: "Mix".into(),
            tracks: ["c", "a", "c"].iter().map(|n| PathBuf::from(format!("C:/m/{n}"))).collect(),
        }];
        st.view = View::Playlist(id.clone());
        build_shown(&mut st);
        st.selected = BTreeSet::from([1, 2]);
        let items = row_menu(&st, 2);
        let path = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        let find = |pick: &dyn Fn(&LocalCmd) -> bool| items.iter().find(|i| matches!(&i.cmd, Cmd::Local(c) if pick(c))).map(|i| i.cmd.clone());
        let Some(Cmd::Local(LocalCmd::MenuQueue(paths))) = find(&|c| matches!(c, LocalCmd::MenuQueue(_))) else { panic!("no queue item") };
        assert_eq!(paths, [path("a"), path("c")]);
        let Some(Cmd::Local(LocalCmd::MenuRemoveFromPlaylist(pid, entries))) = find(&|c| matches!(c, LocalCmd::MenuRemoveFromPlaylist(..))) else {
            panic!("no remove item")
        };
        assert_eq!((pid, entries), (id.clone(), vec![(1, path("a")), (2, path("c"))]));
        // The tree's items name the playlist, not a row.
        push_tree(&mut st, &slint::Weak::default());
        let at = st.nodes.iter().position(|n| n.2.as_deref() == Some("lp1")).unwrap();
        assert!(node_menu(&st, at).iter().any(|i| matches!(&i.cmd, Cmd::Local(LocalCmd::NodeDelete(d)) if *d == id)));
    }

    #[test]
    fn a_rebuilt_list_keeps_the_selection_on_the_same_tracks() {
        let mut st = LocalState::default();
        st.tracks = lib(&["b", "c", "d"]);
        build_shown(&mut st);
        st.selected = BTreeSet::from([1]); // "c"
        st.anchor = Some(1);
        let keys = selection_keys(&st);
        // A scan found "a", which sorts first: every row moves down one.
        st.tracks = lib(&["a", "b", "c", "d"]);
        build_shown(&mut st);
        remap_selection(&mut st, keys);
        assert_eq!((st.selected.iter().copied().collect::<Vec<_>>(), st.anchor), (vec![2], Some(2)));
        assert_eq!(row_of(&st, 1, Path::new("C:/m/c")), Some(2), "a menu's row follows its track");
        // The track gone: nothing selected in its place.
        let keys = selection_keys(&st);
        st.tracks = lib(&["a", "b", "d"]);
        build_shown(&mut st);
        remap_selection(&mut st, keys);
        assert!(st.selected.is_empty() && st.anchor.is_none());
        assert_eq!(row_of(&st, 2, Path::new("C:/m/c")), None);
    }

    #[test]
    fn queue_items_are_found_where_they_moved() {
        let mut st = LocalState::default();
        let p = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        st.order = vec![p("a"), p("b"), p("c")];
        st.user_queue = VecDeque::from([p("q1"), p("q2")]);
        st.now = Some(p("a"));
        // The menu was opened on q2 at User(1); q1 has played since.
        st.user_queue.pop_front();
        assert_eq!(queue_item(&st, QSrc::User(1), &p("q2")), Some(QSrc::User(0)));
        // Opened on "c" at Order(2); the list moved on to "b".
        st.pos = 1;
        st.now = Some(p("b"));
        assert_eq!(queue_item(&st, QSrc::Order(2), &p("c")), Some(QSrc::Order(2)));
        assert_eq!(queue_item(&st, QSrc::Order(1), &p("b")), None, "now playing, not in the queue part");
        assert_eq!(queue_item(&st, QSrc::Current, &p("a")), None);
    }

    fn tx() -> (Tx, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        tokio::sync::mpsc::unbounded_channel()
    }

    #[test]
    fn a_row_click_follows_the_track_it_hit_not_the_row_number() {
        let mut st = LocalState::default();
        st.tracks = lib(&["b", "c", "d"]);
        build_shown(&mut st);
        let c = PathBuf::from("C:/m/c");
        assert_eq!(clicked_row(&st, 1, Some(&c)), Some(1));
        // A scan put "a" first before the worker read the click: "c" is row 2 now.
        st.tracks = lib(&["a", "b", "c", "d"]);
        build_shown(&mut st);
        assert_eq!(clicked_row(&st, 1, Some(&c)), Some(2));
        // Selecting by that click selects "c", not "b".
        let (t, _rx) = tx();
        handle(&mut st, LocalCmd::RowSelect(1, false, false, Some(c.clone())), &slint::Weak::default(), &t);
        assert_eq!(st.selected.iter().copied().collect::<Vec<_>>(), [2]);
        // Gone from the list: the click does nothing.
        st.tracks = lib(&["a", "b", "d"]);
        build_shown(&mut st);
        assert_eq!(clicked_row(&st, 1, Some(&c)), None);
        assert_eq!(clicked_row(&st, 1, None), Some(1), "no track known: the row as before");
    }

    #[test]
    fn artist_counts_are_the_lists_they_open() {
        let mut st = LocalState::default();
        let t = |name: &str, artist: &str, album_artist: &str| LocalTrack {
            artist: artist.into(),
            album_artist: album_artist.into(),
            ..track(name, 1000)
        };
        st.tracks = vec![t("a", "Ada", "Ada"), t("b", "Ada", "Ada"), t("c", "Ada", "Various"), t("d", "Bo", "Various"), t("e", "Cy", "Various")];
        push_tree(&mut st, &slint::Weak::default());
        for (i, (node, act, _)) in st.nodes.clone().into_iter().enumerate() {
            if let NodeAct::View(v @ View::Artist(_)) = act {
                st.view = v;
                build_shown(&mut st);
                assert_eq!(node.count.as_str(), st.shown.len().to_string(), "row {i}: {}", node.label);
            }
        }
        // Ada: her two albums' tracks and the one on the compilation.
        assert!(st.nodes.iter().any(|n| n.0.label.as_str() == "Ada" && n.0.count.as_str() == "3"));
    }

    #[test]
    fn a_blank_name_keeps_the_prompt_asking() {
        let mut st = LocalState::default();
        let (t, _rx) = tx();
        st.pending = Some(Pending::NewPlaylist(Vec::new()));
        handle(&mut st, LocalCmd::Named(Some("   ".into())), &slint::Weak::default(), &t);
        assert!(matches!(st.pending, Some(Pending::NewPlaylist(_))), "still waiting for a name");
        handle(&mut st, LocalCmd::Named(None), &slint::Weak::default(), &t);
        assert!(st.pending.is_none(), "cancel ends it");
    }

    #[test]
    fn delete_playlist_asks_first() {
        let mut st = LocalState::default();
        let (t, _rx) = tx();
        st.playlists = vec![LocalPlaylist { id: "lpx".into(), name: "Keep?".into(), tracks: Vec::new() }];
        handle(&mut st, LocalCmd::NodeDelete("lpx".into()), &slint::Weak::default(), &t);
        assert_eq!(st.delete_pending.as_deref(), Some("lpx"));
        assert_eq!(st.playlists.len(), 1, "nothing deleted before Yes");
        // An id that isn't a playlist opens nothing.
        st.delete_pending = None;
        handle(&mut st, LocalCmd::NodeDelete("nope".into()), &slint::Weak::default(), &t);
        assert!(st.delete_pending.is_none());
        // Yes with nothing asked: nothing happens.
        handle(&mut st, LocalCmd::DeleteConfirmed, &slint::Weak::default(), &t);
        assert_eq!(st.playlists.len(), 1);
    }

    #[test]
    fn recent_looks_up_files_outside_the_library_off_the_worker() {
        let mut st = LocalState::default();
        st.history_loaded = true;
        st.tracks = lib(&["in-lib"]);
        let gone = PathBuf::from("Z:/nowhere/gone.flac");
        let elsewhere = std::env::current_exe().unwrap();
        st.history = vec![
            (PathBuf::from("C:/m/in-lib"), crate::play_history::now_iso()),
            (gone.clone(), crate::play_history::now_iso()),
            (elsewhere.clone(), crate::play_history::now_iso()),
        ];
        let (t, mut rx) = tx();
        // Not known yet: left out for now, and looked up on another thread.
        push_recent(&mut st, &slint::Weak::default(), Some(&t));
        assert_eq!(st.rview, [PathBuf::from("C:/m/in-lib")]);
        assert!(st.recent_checking);
        let Some(Cmd::Local(LocalCmd::RecentChecked(found))) = rx.blocking_recv() else { panic!("no lookup result") };
        assert_eq!(found.len(), 2);
        handle(&mut st, LocalCmd::RecentChecked(found), &slint::Weak::default(), &t);
        assert_eq!(st.rview, [PathBuf::from("C:/m/in-lib"), elsewhere], "the one that exists shows, the gone one doesn't");
        assert!(!st.recent_checking && rx.try_recv().is_err(), "nothing left to look up");
    }

    #[test]
    fn a_local_history_that_cant_be_read_is_not_written_over() {
        let dir = std::env::temp_dir().join(format!("lightify-local-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A folder where the file should be: there, but can't be read as one.
        let file = dir.join("lightify_shell_local_history.json");
        std::fs::create_dir(&file).unwrap();
        let mut st = LocalState::default();
        add_local_play_to(&mut st, PathBuf::from("C:/m/a"), &file);
        assert!(!st.history_loaded && file.is_dir(), "not written over");
        assert_eq!(st.history.len(), 1, "the play is kept meanwhile");
        // Readable again: what it held goes under the play since.
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, serde_json::to_vec(&vec![(PathBuf::from("C:/m/old"), "2026-11-01T10:00:00Z")]).unwrap()).unwrap();
        add_local_play_to(&mut st, PathBuf::from("C:/m/b"), &file);
        let saved: Vec<(PathBuf, String)> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let paths: Vec<PathBuf> = saved.into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, [PathBuf::from("C:/m/b"), PathBuf::from("C:/m/a"), PathBuf::from("C:/m/old")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shuffle_off_returns_to_the_list_that_was_playing() {
        let mut st = LocalState::default();
        let p = |n: &str| PathBuf::from(format!("C:/m/{n}"));
        st.source = vec![p("a"), p("b"), p("c"), p("d")];
        st.shuffle = true;
        st.order = vec![p("c"), p("a"), p("d"), p("b")];
        st.pos = 1;
        st.now = Some(p("a"));
        // Browsing elsewhere since: the shown list must not become the queue.
        st.shown.clear();
        st.shuffle = false;
        rebuild_order_keep_current(&mut st);
        assert_eq!(st.order, st.source);
        assert_eq!((st.pos, peek_next(&st)), (0, Some((NextSrc::Order(1), p("b")))));

        // Playing a hand-queued song: the list resumes after the order's own place.
        st.shuffle = true;
        st.order = vec![p("c"), p("a"), p("d"), p("b")];
        st.pos = 0;
        st.now = Some(p("q"));
        st.shuffle = false;
        rebuild_order_keep_current(&mut st);
        assert_eq!((st.pos, peek_next(&st)), (2, Some((NextSrc::Order(3), p("d")))));
    }
}
