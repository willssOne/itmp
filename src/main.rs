mod cache;
mod cover;
mod downloader;
mod lyrics;
mod player;
mod spotify;

use cache::{
    add_track_to_playlist, audio_dir, fmt_mb, fmt_time, has_lrc, load_and_sync_library, now_ts,
    prune_lru, remove_track_files, save_library, total_cache_bytes, Library, Playlist, TrackEntry,
    DEFAULT_MAX_MB,
};
use cover::{CoverArt, ThumbCache};
use downloader::{
    download_and_cache_track, expand_local_path, fetch_spotify_metadata, import_local_music,
    parse_spotify_export_file, search_youtube, SearchResult, SpotifyTrackSpec,
};
use lyrics::{load_lrc, AudioEnvelope, LrcLine};
use player::{MpvPlayer, PlaybackState};
use spotify::{
    clear_session as clear_spotify_session, fetch_collection_tracks, fetch_collections,
    load_session as load_spotify_session, login_pkce, open_url_in_browser,
    save_session as save_spotify_session, saved_links_as_collections, SavedSpotifyLink,
    SpotifyCollection, SpotifyCollectionKind, SpotifySession, REDIRECT_URI as SPOTIFY_REDIRECT_URI,
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::{
    collections::VecDeque,
    env, io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Main,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    Search,
    Spotify,
    AllTracks,
    Liked,
    Queue,
    Playlist(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    fn next(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
    fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        }
    }
}

enum InputMode {
    Normal,
    Search,
    Import,
    SpotifyClientId,
    SpotifyAddUrl,
    NewPlaylist,
    AddToPlaylist(String),
    SearchResults,
}

enum WorkerMsg {
    Progress(String),
    SearchDone(Result<Vec<SearchResult>, String>),
    DownloadDone(Result<(String, String), String>, bool), // (filename, msg), auto_play
    ImportDone(Result<String, String>),
    LyricsAligned(String, Vec<LrcLine>),
    SpotifyLoginDone(Result<SpotifySession, String>),
    SpotifyCollectionsDone(Result<Vec<SpotifyCollection>, String>),
    SpotifyTracksDone(Result<(SpotifyCollection, Vec<SpotifyTrackSpec>), String>),
    SpotifyPublicUrlAdded(Result<(SavedSpotifyLink, Vec<SpotifyTrackSpec>), String>),
}

#[derive(Debug, Clone, Copy)]
enum ButtonAction {
    Prev,
    PlayPause,
    Next,
    Shuffle,
    Repeat,
    Like,
    VolDown,
    VolUp,
    Import,
    Search,
    ToggleLyrics,
}

#[derive(Default)]
struct HitAreas {
    search_box: Rect,
    sidebar: Rect,
    track_list: Rect,
    progress_bar: Rect,
    controls_bar: Rect,
    buttons: Vec<(Rect, ButtonAction)>,
}

struct App {
    dir: PathBuf,
    max_bytes: u64,
    lib: Library,
    player: MpvPlayer,
    pb: PlaybackState,
    smooth_pos: f64,
    last_frame: Instant,
    last_pb_poll: Instant,
    was_playing_file: bool,

    focus: Focus,
    sidebar_state: ListState,
    view: View,
    track_list: Vec<String>,
    track_state: ListState,
    show_lyrics_pane: bool,

    queue: VecDeque<String>,
    history: Vec<String>,
    shuffle: bool,
    repeat: RepeatMode,
    rng_seed: u64,

    lrc_lines: Vec<LrcLine>,
    lrc_file: Option<String>,
    lrc_audio_synced: bool,
    cover_art: Option<CoverArt>,
    cover_size: (u16, u16),
    thumbs: ThumbCache,

    input_mode: InputMode,
    input_buf: String,
    last_search_query: String,
    search_results: Vec<SearchResult>,
    search_state: ListState,
    playlist_pick_state: ListState,

    spotify: SpotifySession,
    spotify_cols: Vec<SpotifyCollection>,
    spotify_col_state: ListState,
    spotify_open_col: Option<SpotifyCollection>,
    spotify_tracks: Vec<SpotifyTrackSpec>,
    spotify_track_state: ListState,

    hits: HitAreas,
    last_click: Option<(usize, Instant)>,

    status: String,
    busy: bool,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
}

impl App {
    fn new(max_mb: u64) -> Self {
        let dir = audio_dir();
        let lib = load_and_sync_library(&dir);
        let (tx, rx) = mpsc::channel();
        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(2)); // Start on All Tracks (index 2, below / Search & Spotify)
        let spotify = load_spotify_session();
        let initial_sp_cols = saved_links_as_collections(&spotify);
        let mut initial_sp_state = ListState::default();
        if !initial_sp_cols.is_empty() {
            initial_sp_state.select(Some(0));
        }
        let now = Instant::now();
        let mut app = Self {
            dir,
            max_bytes: max_mb * 1_048_576,
            lib,
            player: MpvPlayer::new(),
            pb: PlaybackState::default(),
            smooth_pos: 0.0,
            last_frame: now,
            last_pb_poll: now - Duration::from_secs(1),
            was_playing_file: false,
            focus: Focus::Main,
            sidebar_state,
            view: View::AllTracks,
            track_list: Vec::new(),
            track_state: ListState::default(),
            show_lyrics_pane: true,
            queue: VecDeque::new(),
            history: Vec::new(),
            shuffle: false,
            repeat: RepeatMode::All,
            rng_seed: now_ts() ^ 0x9e37_79b9_7f4a_7c15,
            lrc_lines: Vec::new(),
            lrc_file: None,
            lrc_audio_synced: false,
            cover_art: None,
            cover_size: (0, 0),
            thumbs: ThumbCache::default(),
            input_mode: InputMode::Normal,
            input_buf: String::new(),
            last_search_query: String::new(),
            search_results: Vec::new(),
            search_state: ListState::default(),
            playlist_pick_state: ListState::default(),
            spotify,
            spotify_cols: initial_sp_cols,
            spotify_col_state: initial_sp_state,
            spotify_open_col: None,
            spotify_tracks: Vec::new(),
            spotify_track_state: ListState::default(),
            hits: HitAreas::default(),
            last_click: None,
            status: "Space play/pause | Enter play | / search | i import | z shuffle | r repeat | l like | L lyrics | q quit".into(),
            busy: false,
            tx,
            rx,
        };
        app.rebuild_track_list();
        app
    }

    fn next_rand(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        let mut x = self.rng_seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_seed = x;
        (x as usize) % bound
    }

    fn sidebar_items(&self) -> Vec<(String, View)> {
        let liked_count = self.lib.entries.values().filter(|e| e.liked).count();
        let sp_label = if self.spotify.is_logged_in() {
            "Spotify (*) ".to_string()
        } else {
            "Spotify Login".to_string()
        };
        let mut items = vec![
            ("/ Search".to_string(), View::Search),
            (sp_label, View::Spotify),
            (format!("All Tracks ({})", self.lib.entries.len()), View::AllTracks),
            (format!("* Liked ({})", liked_count), View::Liked),
            (format!("Queue ({})", self.queue.len()), View::Queue),
        ];
        for (idx, pl) in self.lib.playlists.iter().enumerate() {
            items.push((format!("- {} ({})", pl.name, pl.tracks.len()), View::Playlist(idx)));
        }
        items
    }

    fn sync_view_from_sidebar(&mut self) {
        let items = self.sidebar_items();
        if let Some(i) = self.sidebar_state.selected() {
            if let Some((_, v)) = items.get(i) {
                self.view = v.clone();
                if matches!(self.view, View::Search) {
                    if self.search_results.is_empty() {
                        self.input_mode = InputMode::Search;
                    } else {
                        self.input_mode = InputMode::SearchResults;
                    }
                } else if matches!(self.view, View::Spotify) {
                    if matches!(self.input_mode, InputMode::Search | InputMode::SearchResults) {
                        self.input_mode = InputMode::Normal;
                    }
                    if self.spotify.is_logged_in() && self.spotify_cols.is_empty() && !self.busy {
                        self.start_spotify_load_collections();
                    }
                } else {
                    if matches!(self.input_mode, InputMode::Search | InputMode::SearchResults) {
                        self.input_mode = InputMode::Normal;
                    }
                    self.rebuild_track_list();
                }
            }
        }
    }

    fn rebuild_track_list(&mut self) {
        let list: Vec<String> = match &self.view {
            View::Search | View::Spotify => Vec::new(),
            View::AllTracks => {
                let mut keys: Vec<String> = self.lib.entries.keys().cloned().collect();
                keys.sort_by(|a, b| {
                    let ea = &self.lib.entries[a];
                    let eb = &self.lib.entries[b];
                    eb.liked.cmp(&ea.liked).then_with(|| a.cmp(b))
                });
                keys
            }
            View::Liked => {
                let mut keys: Vec<String> = self
                    .lib
                    .entries
                    .values()
                    .filter(|e| e.liked)
                    .map(|e| e.filename.clone())
                    .collect();
                keys.sort();
                keys
            }
            View::Queue => self.queue.iter().cloned().collect(),
            View::Playlist(idx) => self
                .lib
                .playlists
                .get(*idx)
                .map(|p| p.tracks.clone())
                .unwrap_or_default(),
        };
        self.track_list = list;
        if self.track_list.is_empty() {
            self.track_state.select(None);
        } else {
            let cur = self
                .track_state
                .selected()
                .unwrap_or(0)
                .min(self.track_list.len() - 1);
            self.track_state.select(Some(cur));
        }
    }

    fn reload_library(&mut self) {
        self.lib = load_and_sync_library(&self.dir);
        if let View::Playlist(idx) = self.view {
            if idx >= self.lib.playlists.len() {
                self.view = View::AllTracks;
                self.sidebar_state.select(Some(2));
            }
        }
        self.rebuild_track_list();
    }

    fn selected_track(&self) -> Option<String> {
        self.track_state
            .selected()
            .and_then(|i| self.track_list.get(i))
            .cloned()
    }

    fn cover_path_for(&self, fname: &str) -> Option<PathBuf> {
        self.lib
            .entries
            .get(fname)
            .and_then(|e| e.cover.as_deref())
            .map(|c| self.dir.join(c))
            .filter(|p| p.is_file())
    }

    fn play_filename(&mut self, fname: &str, record_history: bool) {
        let full = self.dir.join(fname);
        if !full.is_file() {
            self.status = format!("File missing: {fname}");
            return;
        }
        if record_history {
            if let Some(prev) = self.player.current_file.clone() {
                if prev != fname {
                    self.history.push(prev);
                    if self.history.len() > 64 {
                        self.history.remove(0);
                    }
                }
            }
        }
        match self.player.play_file(&full, fname) {
            Ok(()) => {
                self.was_playing_file = true;
                self.smooth_pos = 0.0;
                if let Some(e) = self.lib.entries.get_mut(fname) {
                    e.last_used = now_ts();
                    let label = if e.artist.is_empty() {
                        e.title.clone()
                    } else {
                        format!("{} - {}", e.title, e.artist)
                    };
                    self.status = format!("Playing: {label}");
                }
                save_library(&self.dir, &self.lib);
                self.refresh_lyrics_for(fname);
                self.cover_art = None;
            }
            Err(e) => {
                self.status = format!("Playback error: {e}");
            }
        }
    }

    fn toggle_play_or_start(&mut self) {
        if self.pb.active {
            self.player.toggle_pause();
        } else if let Some(cur) = self.player.current_file.clone() {
            self.play_filename(&cur, false);
        } else if let Some(sel) = self.selected_track() {
            self.play_filename(&sel, true);
        } else if let Some(first) = self.track_list.first().cloned() {
            self.play_filename(&first, true);
        }
    }

    fn toggle_like(&mut self) {
        let target = if self.focus == Focus::Main {
            self.selected_track()
                .or_else(|| self.player.current_file.clone())
        } else {
            self.player.current_file.clone().or_else(|| self.selected_track())
        };
        if let Some(fname) = target {
            if let Some(e) = self.lib.entries.get_mut(&fname) {
                e.liked = !e.liked;
                let st = if e.liked {
                    "Liked (pinned from 500 MB LRU eviction)"
                } else {
                    "Unliked"
                };
                let t = e.title.clone();
                save_library(&self.dir, &self.lib);
                self.rebuild_track_list();
                self.status = format!("{st}: {t}");
            }
        }
    }

    fn refresh_lyrics_for(&mut self, fname: &str) {
        if self.lrc_file.as_deref() == Some(fname) {
            return;
        }
        // Instant initial load (syllable-paced + parallel same-line adlibs)
        self.lrc_lines = load_lrc(&self.dir, fname, None);
        self.lrc_file = Some(fname.to_string());
        self.lrc_audio_synced = false;

        // Background audio-envelope alignment (~300ms): detects exact vocal bursts,
        // breath pauses, and adlib tails from the real audio waveform.
        if !self.lrc_lines.is_empty() {
            let dir = self.dir.clone();
            let fname_owned = fname.to_string();
            let full = dir.join(fname);
            let tx = self.tx.clone();
            thread::spawn(move || {
                if let Some(env) = AudioEnvelope::extract(&full) {
                    let aligned = load_lrc(&dir, &fname_owned, Some(&env));
                    if !aligned.is_empty() {
                        let _ = tx.send(WorkerMsg::LyricsAligned(fname_owned, aligned));
                    }
                }
            });
        }
    }

    fn adjust_lrc_offset(&mut self, delta_ms: Option<i32>) {
        let Some(fname) = self.player.current_file.clone() else {
            return;
        };
        if let Some(e) = self.lib.entries.get_mut(&fname) {
            e.lrc_offset_ms = match delta_ms {
                Some(d) => (e.lrc_offset_ms + d).clamp(-5000, 5000),
                None => 0,
            };
            let off = e.lrc_offset_ms;
            save_library(&self.dir, &self.lib);
            self.status = if off == 0 {
                "Lyrics sync offset reset to 0.00s".into()
            } else {
                format!("Lyrics sync offset: {:+.2}s (saved)", off as f64 / 1000.0)
            };
        }
    }

    fn ensure_cover_art(&mut self, cols: u16, rows: u16) {
        if cols < 4 || rows < 2 {
            return;
        }
        let target_fname = self
            .player
            .current_file
            .clone()
            .or_else(|| self.selected_track());
        let cover_path = target_fname
            .as_deref()
            .and_then(|f| self.cover_path_for(f));

        let Some(path) = cover_path else {
            self.cover_art = None;
            return;
        };
        let key = path.to_string_lossy().to_string();
        if let Some(existing) = &self.cover_art {
            if existing.path_key == key && self.cover_size == (cols, rows) {
                return;
            }
        }
        self.cover_art = CoverArt::load(&path, cols, rows);
        self.cover_size = (cols, rows);
    }

    fn play_next(&mut self) {
        if let Some(q) = self.queue.pop_front() {
            if matches!(self.view, View::Queue) {
                self.rebuild_track_list();
            }
            self.play_filename(&q, true);
            return;
        }
        let pool: Vec<String> = if !self.track_list.is_empty() && !matches!(self.view, View::Queue) {
            self.track_list.clone()
        } else {
            let mut k: Vec<String> = self.lib.entries.keys().cloned().collect();
            k.sort();
            k
        };
        if pool.is_empty() {
            return;
        }
        if self.repeat == RepeatMode::One {
            if let Some(cur) = self.player.current_file.clone() {
                self.play_filename(&cur, false);
                return;
            }
        }
        if self.shuffle {
            let idx = self.next_rand(pool.len());
            let choice = pool[idx].clone();
            self.play_filename(&choice, true);
            return;
        }
        if let Some(cur) = &self.player.current_file {
            if let Some(pos) = pool.iter().position(|x| x == cur) {
                if pos + 1 < pool.len() {
                    let next = pool[pos + 1].clone();
                    self.play_filename(&next, true);
                } else if self.repeat == RepeatMode::All {
                    let first = pool[0].clone();
                    self.play_filename(&first, true);
                } else {
                    self.was_playing_file = false;
                }
                return;
            }
        }
        let first = pool[0].clone();
        self.play_filename(&first, true);
    }

    fn play_prev(&mut self) {
        if self.pb.pos > 4.0 {
            self.player.seek_rel(-self.pb.pos);
            self.smooth_pos = 0.0;
            return;
        }
        if let Some(prev) = self.history.pop() {
            self.play_filename(&prev, false);
        }
    }

    fn tick_playback(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f64();
        self.last_frame = now;
        if self.pb.active && !self.pb.paused {
            self.smooth_pos += dt;
            if self.pb.dur > 0.0 {
                self.smooth_pos = self.smooth_pos.min(self.pb.dur);
            }
        }

        if self.last_pb_poll.elapsed() < Duration::from_millis(75) {
            return;
        }
        self.last_pb_poll = now;
        self.pb = self.player.poll_state();
        if !self.pb.active || self.pb.paused || (self.smooth_pos - self.pb.pos).abs() > 0.10 {
            self.smooth_pos = self.pb.pos;
        } else {
            self.smooth_pos = self.smooth_pos * 0.65 + self.pb.pos * 0.35;
        }
        if let Some(cur) = self.player.current_file.clone() {
            if self.pb.dur > 1.0 {
                if let Some(e) = self.lib.entries.get_mut(&cur) {
                    if e.duration_sec == 0 {
                        e.duration_sec = self.pb.dur as u64;
                    }
                }
            }
        }
        if self.was_playing_file && self.pb.idle {
            self.was_playing_file = false;
            self.play_next();
        }
    }

    fn poll_worker(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Progress(p) => self.status = p,
                WorkerMsg::SearchDone(Ok(list)) => {
                    self.busy = false;
                    if list.is_empty() {
                        self.status = "No results found. Press / to search again.".into();
                        self.input_mode = InputMode::Normal;
                    } else {
                        self.search_results = list;
                        self.search_state.select(Some(0));
                        self.view = View::Search;
                        self.sidebar_state.select(Some(0));
                        self.focus = Focus::Main;
                        self.input_mode = InputMode::SearchResults;
                        self.status = "Enter/Click: download & play | d: download only | /: new search | Esc: back".into();
                    }
                }
                WorkerMsg::SearchDone(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Search failed: {e}");
                    self.input_mode = InputMode::Normal;
                }
                WorkerMsg::DownloadDone(Ok((fname, info)), auto_play) => {
                    self.busy = false;
                    self.reload_library();
                    self.status = info;
                    if auto_play {
                        self.play_filename(&fname, true);
                    }
                }
                WorkerMsg::ImportDone(Ok(info)) => {
                    self.busy = false;
                    self.reload_library();
                    self.status = info;
                }
                WorkerMsg::DownloadDone(Err(e), _) => {
                    self.busy = false;
                    self.status = format!("Download failed: {e}");
                }
                WorkerMsg::ImportDone(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Import failed: {e}");
                }
                WorkerMsg::LyricsAligned(fname, lines) => {
                    if self.lrc_file.as_deref() == Some(fname.as_str()) {
                        self.lrc_lines = lines;
                        self.lrc_audio_synced = true;
                    }
                }
                WorkerMsg::SpotifyLoginDone(Ok(sess)) => {
                    self.busy = false;
                    let who = sess.display_name.clone();
                    self.spotify = sess;
                    self.spotify_open_col = None;
                    self.spotify_tracks.clear();
                    self.status = format!("Logged in to Spotify as {who} -- loading your library...");
                    self.start_spotify_load_collections();
                }
                WorkerMsg::SpotifyLoginDone(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Spotify login failed: {e}");
                }
                WorkerMsg::SpotifyCollectionsDone(Ok(cols)) => {
                    self.busy = false;
                    let n = cols.len();
                    self.spotify_cols = cols;
                    self.spotify_col_state.select(if n > 0 { Some(0) } else { None });
                    self.status = format!(
                        "Loaded {n} Spotify collections (Enter/Click to open, R refresh, X logout)"
                    );
                }
                WorkerMsg::SpotifyCollectionsDone(Err(e)) => {
                    self.busy = false;
                    // Keep any saved public links accessible even if Developer API returns 403 Premium required
                    self.spotify_cols = saved_links_as_collections(&self.spotify);
                    if !self.spotify_cols.is_empty() {
                        self.spotify_col_state.select(Some(0));
                    }
                    self.status = e;
                }
                WorkerMsg::SpotifyTracksDone(Ok((col, tracks))) => {
                    self.busy = false;
                    let n = tracks.len();
                    let name = col.name.clone();
                    self.spotify_open_col = Some(col);
                    self.spotify_tracks = tracks;
                    self.spotify_track_state.select(if n > 0 { Some(0) } else { None });
                    self.status = format!(
                        "Spotify '{name}' ({n} tracks): Enter/Click play & save | d download | a import all | Esc back"
                    );
                }
                WorkerMsg::SpotifyTracksDone(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Spotify tracks error: {e}");
                }
                WorkerMsg::SpotifyPublicUrlAdded(Ok((link, tracks))) => {
                    self.busy = false;
                    if !self.spotify.saved_links.iter().any(|l| l.url == link.url) {
                        self.spotify.saved_links.insert(0, link.clone());
                        let _ = save_spotify_session(&self.spotify);
                    }
                    self.spotify_cols = saved_links_as_collections(&self.spotify);
                    self.spotify_col_state.select(Some(0));
                    let col = SpotifyCollection {
                        name: link.name.clone(),
                        subtitle: format!("Spotify {} (no Premium needed)", link.kind),
                        track_count: tracks.len(),
                        cover_url: link.cover_url.clone(),
                        kind: SpotifyCollectionKind::PublicUrl { url: link.url },
                    };
                    let n = tracks.len();
                    self.spotify_open_col = Some(col);
                    self.spotify_tracks = tracks;
                    self.spotify_track_state.select(if n > 0 { Some(0) } else { None });
                    self.status = format!(
                        "Added '{}' ({n} tracks)! Enter/Click play & save | d download | a import all | Esc back",
                        link.name
                    );
                }
                WorkerMsg::SpotifyPublicUrlAdded(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Spotify link error: {e}");
                }
            }
        }
    }

    fn open_spotify_view(&mut self) {
        self.view = View::Spotify;
        self.sidebar_state.select(Some(1));
        self.focus = Focus::Main;
        if matches!(self.input_mode, InputMode::Search | InputMode::SearchResults) {
            self.input_mode = InputMode::Normal;
        }
        if self.spotify.is_logged_in() && self.spotify_cols.is_empty() && !self.busy {
            self.start_spotify_load_collections();
        }
    }

    fn start_spotify_login(&mut self, client_id: String) {
        let cid = client_id.trim().to_string();
        if cid.is_empty() {
            self.input_mode = InputMode::Normal;
            return;
        }
        self.busy = true;
        self.input_mode = InputMode::Normal;
        self.status = "Starting Spotify OAuth login in your browser...".into();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let tx_p = tx.clone();
            let res = login_pkce(&cid, |msg| {
                let _ = tx_p.send(WorkerMsg::Progress(msg.to_string()));
            });
            let _ = tx.send(WorkerMsg::SpotifyLoginDone(res));
        });
    }

    fn start_spotify_load_collections(&mut self) {
        if self.busy || !self.spotify.is_logged_in() {
            return;
        }
        self.busy = true;
        self.status = "Loading your Spotify Liked Songs, playlists & albums...".into();
        let mut sess = self.spotify.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = fetch_collections(&mut sess);
            let _ = tx.send(WorkerMsg::SpotifyCollectionsDone(res));
        });
    }

    fn start_spotify_open_collection(&mut self, idx: usize) {
        if self.busy {
            return;
        }
        let Some(col) = self.spotify_cols.get(idx).cloned() else {
            return;
        };
        if !matches!(col.kind, SpotifyCollectionKind::PublicUrl { .. }) && !self.spotify.is_logged_in() {
            return;
        }
        self.busy = true;
        self.status = format!("Loading tracks from Spotify '{}'...", col.name);
        let mut sess = self.spotify.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = fetch_collection_tracks(&mut sess, &col).map(|tracks| (col, tracks));
            let _ = tx.send(WorkerMsg::SpotifyTracksDone(res));
        });
    }

    fn start_spotify_add_public_url(&mut self, raw: String) {
        let input = raw.trim().to_string();
        if input.is_empty() {
            self.input_mode = InputMode::Normal;
            return;
        }
        let local = expand_local_path(&input);
        if local.is_file() {
            if let Some(bundle) = parse_spotify_export_file(&local) {
                let col = SpotifyCollection {
                    name: bundle.collection_name.clone(),
                    subtitle: "Spotify Export (CSV/JSON)".into(),
                    track_count: bundle.tracks.len(),
                    cover_url: bundle.cover_url,
                    kind: SpotifyCollectionKind::PublicUrl {
                        url: local.to_string_lossy().to_string(),
                    },
                };
                let n = bundle.tracks.len();
                self.spotify_open_col = Some(col);
                self.spotify_tracks = bundle.tracks;
                self.spotify_track_state.select(if n > 0 { Some(0) } else { None });
                self.input_mode = InputMode::Normal;
                self.status = format!(
                    "Loaded '{}' ({n} tracks from file)! Enter/Click play & save | a import all",
                    bundle.collection_name
                );
                return;
            }
        }
        self.busy = true;
        self.input_mode = InputMode::Normal;
        self.status = "Loading Spotify playlist/album/track link (no Premium needed)...".into();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = fetch_spotify_metadata(&input).map(|b| {
                let link = SavedSpotifyLink {
                    url: input,
                    name: b.collection_name,
                    kind: b.kind,
                    track_count: b.tracks.len(),
                    cover_url: b.cover_url,
                };
                (link, b.tracks)
            });
            let _ = tx.send(WorkerMsg::SpotifyPublicUrlAdded(res));
        });
    }

    fn start_spotify_download_track(&mut self, idx: usize, auto_play: bool) {
        if self.busy {
            return;
        }
        let Some(tr) = self.spotify_tracks.get(idx).cloned() else {
            return;
        };
        let col = self.spotify_open_col.clone();
        let is_liked_col = matches!(
            col.as_ref().map(|c| &c.kind),
            Some(SpotifyCollectionKind::LikedSongs)
        );
        let pl_name = col.as_ref().and_then(|c| match &c.kind {
            SpotifyCollectionKind::Playlist { .. }
            | SpotifyCollectionKind::Album { .. }
            | SpotifyCollectionKind::PublicUrl { .. } => Some(c.name.clone()),
            _ => None,
        });
        let fallback_cover = col.and_then(|c| c.cover_url);
        self.busy = true;
        self.status = format!(
            "Downloading '{} - {}'{}...",
            tr.artist,
            tr.title,
            if auto_play { " (will play when ready)" } else { "" }
        );
        let dir = self.dir.clone();
        let max_bytes = self.max_bytes;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let q = format!("ytsearch1:{} {} audio", tr.artist, tr.title);
            let res = download_and_cache_track(
                &dir,
                &q,
                &tr.title,
                &tr.artist,
                &tr.album,
                tr.cover_url.as_deref().or(fallback_cover.as_deref()),
                tr.duration_sec,
                pl_name.as_deref(),
                max_bytes,
            );
            if is_liked_col {
                if let Ok((ref fname, _)) = res {
                    let mut lib = load_and_sync_library(&dir);
                    if let Some(e) = lib.entries.get_mut(fname) {
                        e.liked = true;
                        save_library(&dir, &lib);
                    }
                }
            }
            let _ = tx.send(WorkerMsg::DownloadDone(res, auto_play));
        });
    }

    fn start_spotify_import_all(&mut self) {
        if self.busy || self.spotify_tracks.is_empty() {
            return;
        }
        let Some(col) = self.spotify_open_col.clone() else {
            return;
        };
        let tracks = self.spotify_tracks.clone();
        let is_liked_col = matches!(col.kind, SpotifyCollectionKind::LikedSongs);
        let pl_name = match &col.kind {
            SpotifyCollectionKind::LikedSongs => None,
            _ => Some(col.name.clone()),
        };
        let fallback_cover = col.cover_url.clone();
        let col_label = col.name.clone();
        self.busy = true;
        self.status = format!("Importing {} tracks from Spotify '{}'...", tracks.len(), col_label);
        let dir = self.dir.clone();
        let max_bytes = self.max_bytes;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let total = tracks.len();
            let mut ok_count = 0usize;
            for (idx, tr) in tracks.iter().enumerate() {
                let _ = tx.send(WorkerMsg::Progress(format!(
                    "Spotify [{}/{}] Downloading {} - {}...",
                    idx + 1,
                    total,
                    tr.artist,
                    tr.title
                )));
                let q = format!("ytsearch1:{} {} audio", tr.artist, tr.title);
                if let Ok((fname, _)) = download_and_cache_track(
                    &dir,
                    &q,
                    &tr.title,
                    &tr.artist,
                    &tr.album,
                    tr.cover_url.as_deref().or(fallback_cover.as_deref()),
                    tr.duration_sec,
                    pl_name.as_deref(),
                    max_bytes,
                ) {
                    if is_liked_col {
                        let mut lib = load_and_sync_library(&dir);
                        if let Some(e) = lib.entries.get_mut(&fname) {
                            e.liked = true;
                            save_library(&dir, &lib);
                        }
                    }
                    ok_count += 1;
                }
            }
            let summary = if is_liked_col {
                format!("Imported {ok_count}/{total} tracks into * Liked")
            } else {
                format!("Imported {ok_count}/{total} tracks into playlist '{col_label}'")
            };
            let _ = tx.send(WorkerMsg::ImportDone(Ok(summary)));
        });
    }

    fn logout_spotify(&mut self) {
        clear_spotify_session();
        self.spotify = SpotifySession::default();
        self.spotify_cols.clear();
        self.spotify_open_col = None;
        self.spotify_tracks.clear();
        self.status = "Logged out of Spotify.".into();
    }

    fn start_youtube_search(&mut self) {
        let q = self.input_buf.trim().to_string();
        self.input_buf.clear();
        if q.is_empty() {
            if !self.search_results.is_empty() {
                self.input_mode = InputMode::SearchResults;
            } else {
                self.input_mode = InputMode::Normal;
            }
            return;
        }
        if q.contains("open.spotify.com/") || q.starts_with("spotify:") || expand_local_path(&q).exists() {
            self.start_smart_import(q);
            return;
        }
        self.last_search_query = q.clone();
        self.search_results.clear();
        self.view = View::Search;
        self.sidebar_state.select(Some(0));
        self.busy = true;
        self.input_mode = InputMode::Normal;
        self.status = format!("Searching YouTube for '{q}'...");
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(WorkerMsg::SearchDone(search_youtube(&q)));
        });
    }

    fn start_download_search_result(&mut self, auto_play: bool) {
        let Some(i) = self.search_state.selected() else {
            return;
        };
        let Some(item) = self.search_results.get(i).cloned() else {
            return;
        };
        self.busy = true;
        self.status = format!(
            "Downloading '{}'{}...",
            item.title,
            if auto_play { " (will play when ready)" } else { "" }
        );
        let dir = self.dir.clone();
        let max_bytes = self.max_bytes;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let url = format!("https://www.youtube.com/watch?v={}", item.id);
            let res = download_and_cache_track(
                &dir,
                &url,
                &item.title,
                &item.uploader,
                "",
                Some(&item.thumbnail),
                0,
                None,
                max_bytes,
            );
            let _ = tx.send(WorkerMsg::DownloadDone(res, auto_play));
        });
    }

    fn start_smart_import(&mut self, raw: String) {
        let input = raw.trim().to_string();
        if input.is_empty() {
            self.input_mode = InputMode::Normal;
            return;
        }
        self.busy = true;
        self.input_mode = InputMode::Normal;
        let dir = self.dir.clone();
        let max_bytes = self.max_bytes;
        let tx = self.tx.clone();

        if expand_local_path(&input).exists() {
            let local_p = expand_local_path(&input);
            if let Some(bundle) = parse_spotify_export_file(&local_p) {
                self.status = format!(
                    "Importing {} tracks from '{}'...",
                    bundle.tracks.len(),
                    bundle.collection_name
                );
                thread::spawn(move || {
                    let total = bundle.tracks.len();
                    let pl_name = Some(bundle.collection_name.as_str());
                    let mut ok_count = 0usize;
                    for (idx, tr) in bundle.tracks.iter().enumerate() {
                        let _ = tx.send(WorkerMsg::Progress(format!(
                            "Export [{}/{}] Downloading {} - {}...",
                            idx + 1,
                            total,
                            tr.artist,
                            tr.title
                        )));
                        let q = format!("ytsearch1:{} {} audio", tr.artist, tr.title);
                        if download_and_cache_track(
                            &dir,
                            &q,
                            &tr.title,
                            &tr.artist,
                            &tr.album,
                            None,
                            tr.duration_sec,
                            pl_name,
                            max_bytes,
                        )
                        .is_ok()
                        {
                            ok_count += 1;
                        }
                    }
                    let _ = tx.send(WorkerMsg::ImportDone(Ok(format!(
                        "Imported {ok_count}/{total} tracks into playlist '{}'",
                        bundle.collection_name
                    ))));
                });
                return;
            }
            self.status = format!("Importing local music from '{input}'...");
            thread::spawn(move || {
                let res = import_local_music(&dir, &input, max_bytes);
                let _ = tx.send(WorkerMsg::ImportDone(res));
            });
            return;
        }

        if input.contains("open.spotify.com/") || input.starts_with("spotify:") {
            self.status = "Fetching Spotify metadata...".into();
            thread::spawn(move || {
                let bundle = match fetch_spotify_metadata(&input) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::ImportDone(Err(e)));
                        return;
                    }
                };
                let total = bundle.tracks.len();
                let pl_name = if bundle.kind == "track" {
                    None
                } else {
                    Some(bundle.collection_name.as_str())
                };
                let mut ok_count = 0usize;
                for (idx, tr) in bundle.tracks.iter().enumerate() {
                    let _ = tx.send(WorkerMsg::Progress(format!(
                        "Spotify [{}/{}] Downloading {} - {}...",
                        idx + 1,
                        total,
                        tr.artist,
                        tr.title
                    )));
                    let q = format!("ytsearch1:{} {} audio", tr.artist, tr.title);
                    if download_and_cache_track(
                        &dir,
                        &q,
                        &tr.title,
                        &tr.artist,
                        &tr.album,
                        tr.cover_url.as_deref().or(bundle.cover_url.as_deref()),
                        tr.duration_sec,
                        pl_name,
                        max_bytes,
                    )
                    .is_ok()
                    {
                        ok_count += 1;
                    }
                }
                let summary = if let Some(pl) = pl_name {
                    format!("Imported {ok_count}/{total} tracks into playlist '{pl}'")
                } else {
                    format!("Imported {ok_count}/{total} track from Spotify")
                };
                let _ = tx.send(WorkerMsg::ImportDone(Ok(summary)));
            });
            return;
        }

        self.status = format!("Importing '{input}'...");
        thread::spawn(move || match search_youtube(&input) {
            Ok(list) if !list.is_empty() => {
                let is_url = input.starts_with("http://") || input.starts_with("https://");
                let items: Vec<SearchResult> = if is_url { list } else { vec![list[0].clone()] };
                let total = items.len();
                let mut ok_count = 0usize;
                for (idx, item) in items.iter().enumerate() {
                    let _ = tx.send(WorkerMsg::Progress(format!(
                        "Downloading [{}/{}] {}...",
                        idx + 1,
                        total,
                        item.title
                    )));
                    let url = format!("https://www.youtube.com/watch?v={}", item.id);
                    if download_and_cache_track(
                        &dir,
                        &url,
                        &item.title,
                        &item.uploader,
                        "",
                        Some(&item.thumbnail),
                        0,
                        None,
                        max_bytes,
                    )
                    .is_ok()
                    {
                        ok_count += 1;
                    }
                }
                let _ = tx.send(WorkerMsg::ImportDone(Ok(format!(
                    "Imported {ok_count}/{total} track(s)"
                ))));
            }
            Ok(_) => {
                let _ = tx.send(WorkerMsg::ImportDone(Err(format!("Nothing found for '{input}'"))));
            }
            Err(e) => {
                let _ = tx.send(WorkerMsg::ImportDone(Err(e)));
            }
        });
    }

    fn trigger_button(&mut self, action: ButtonAction) {
        match action {
            ButtonAction::Prev => self.play_prev(),
            ButtonAction::PlayPause => self.toggle_play_or_start(),
            ButtonAction::Next => self.play_next(),
            ButtonAction::Shuffle => {
                self.shuffle = !self.shuffle;
                self.status = format!("Shuffle: {}", if self.shuffle { "on" } else { "off" });
            }
            ButtonAction::Repeat => {
                self.repeat = self.repeat.next();
                self.status = format!("Repeat: {}", self.repeat.label());
            }
            ButtonAction::Like => self.toggle_like(),
            ButtonAction::VolDown => {
                let v = self.player.volume - 5;
                self.player.set_volume(v);
            }
            ButtonAction::VolUp => {
                let v = self.player.volume + 5;
                self.player.set_volume(v);
            }
            ButtonAction::Import => {
                if !self.busy {
                    self.input_buf.clear();
                    self.input_mode = InputMode::Import;
                }
            }
            ButtonAction::Search => {
                if !self.busy {
                    self.input_buf.clear();
                    self.view = View::Search;
                    self.sidebar_state.select(Some(0));
                    self.focus = Focus::Main;
                    self.input_mode = InputMode::Search;
                    self.status = "Type song name, artist, or URL and press Enter to search".into();
                }
            }
            ButtonAction::ToggleLyrics => {
                self.show_lyrics_pane = !self.show_lyrics_pane;
                self.status = format!(
                    "Live lyrics: {}",
                    if self.show_lyrics_pane { "on" } else { "off" }
                );
            }
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        match kind {
            MouseEventKind::ScrollUp => {
                if rect_contains(self.hits.controls_bar, col, row) {
                    let v = self.player.volume + 5;
                    self.player.set_volume(v);
                } else {
                    self.move_cursor(-1);
                }
            }
            MouseEventKind::ScrollDown => {
                if rect_contains(self.hits.controls_bar, col, row) {
                    let v = self.player.volume - 5;
                    self.player.set_volume(v);
                } else {
                    self.move_cursor(1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let buttons = self.hits.buttons.clone();
                for (r, act) in buttons {
                    if rect_contains(r, col, row) {
                        self.trigger_button(act);
                        return;
                    }
                }

                // Click the always-visible top Search bar anytime!
                if rect_contains(self.hits.search_box, col, row) {
                    self.trigger_button(ButtonAction::Search);
                    return;
                }

                let pb = self.hits.progress_bar;
                if self.pb.active && pb.width > 2 && rect_contains(pb, col, row) {
                    let inner_x = col.saturating_sub(pb.x + 1);
                    let inner_w = pb.width.saturating_sub(2).max(1);
                    let pct = (inner_x as f64 / inner_w as f64) * 100.0;
                    self.player.seek_percent(pct);
                    if self.pb.dur > 0.0 {
                        self.smooth_pos = (pct / 100.0) * self.pb.dur;
                    }
                    return;
                }

                if !matches!(
                    self.input_mode,
                    InputMode::Normal
                        | InputMode::Search
                        | InputMode::SearchResults
                        | InputMode::AddToPlaylist(_)
                ) {
                    return;
                }

                let sb = self.hits.sidebar;
                if matches!(self.input_mode, InputMode::Normal | InputMode::SearchResults | InputMode::Search)
                    && rect_contains(sb, col, row)
                {
                    let rel_y = row.saturating_sub(sb.y + 1) as usize;
                    let idx = self.sidebar_state.offset() + rel_y;
                    if idx < self.sidebar_items().len() {
                        self.focus = Focus::Sidebar;
                        self.sidebar_state.select(Some(idx));
                        self.sync_view_from_sidebar();
                    }
                    return;
                }

                let tl = self.hits.track_list;
                if rect_contains(tl, col, row) {
                    let rel_y = row.saturating_sub(tl.y + 1) as usize;
                    match &self.input_mode {
                        InputMode::SearchResults => {
                            let idx = self.search_state.offset() + rel_y;
                            if idx < self.search_results.len() {
                                self.search_state.select(Some(idx));
                                if !self.busy {
                                    self.start_download_search_result(true);
                                }
                            }
                        }
                        InputMode::AddToPlaylist(fname) => {
                            let fname = fname.clone();
                            let idx = self.playlist_pick_state.offset() + rel_y;
                            if let Some(pl) = self.lib.playlists.get(idx) {
                                let pl_name = pl.name.clone();
                                add_track_to_playlist(&mut self.lib, &pl_name, &fname);
                                save_library(&self.dir, &self.lib);
                                self.status = format!("Added to playlist '{pl_name}'");
                                self.input_mode = InputMode::Normal;
                            }
                        }
                        InputMode::Normal => {
                            if matches!(self.view, View::Spotify) {
                                self.focus = Focus::Main;
                                if self.spotify_open_col.is_some() {
                                    let idx = self.spotify_track_state.offset() + rel_y;
                                    if idx < self.spotify_tracks.len() {
                                        self.spotify_track_state.select(Some(idx));
                                        self.start_spotify_download_track(idx, true);
                                    }
                                } else if !self.spotify_cols.is_empty() {
                                    let idx = self.spotify_col_state.offset() + rel_y;
                                    if idx < self.spotify_cols.len() {
                                        self.spotify_col_state.select(Some(idx));
                                        self.start_spotify_open_collection(idx);
                                    }
                                } else if !self.busy {
                                    self.input_buf.clear();
                                    self.input_mode = InputMode::SpotifyAddUrl;
                                }
                                return;
                            }
                            // Each track row is 2 terminal lines tall (with its 4x2 album thumbnail)
                            let idx = self.track_state.offset() + (rel_y / 2);
                            if idx < self.track_list.len() {
                                self.focus = Focus::Main;
                                let already_sel = self.track_state.selected() == Some(idx);
                                let quick_repeat = self
                                    .last_click
                                    .map(|(i, t)| i == idx && t.elapsed() < Duration::from_millis(450))
                                    .unwrap_or(false);
                                self.track_state.select(Some(idx));
                                self.last_click = Some((idx, Instant::now()));
                                if already_sel || quick_repeat {
                                    if let Some(fname) = self.track_list.get(idx).cloned() {
                                        if matches!(self.view, View::Queue) {
                                            self.queue.remove(idx);
                                            self.rebuild_track_list();
                                        }
                                        self.play_filename(&fname, true);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.input_mode {
            InputMode::SearchResults => {
                if self.search_results.is_empty() {
                    return;
                }
                let n = self.search_results.len() as isize;
                let cur = self.search_state.selected().unwrap_or(0) as isize;
                self.search_state
                    .select(Some((cur + delta).rem_euclid(n) as usize));
            }
            InputMode::AddToPlaylist(_) => {
                if self.lib.playlists.is_empty() {
                    return;
                }
                let n = self.lib.playlists.len() as isize;
                let cur = self.playlist_pick_state.selected().unwrap_or(0) as isize;
                self.playlist_pick_state
                    .select(Some((cur + delta).rem_euclid(n) as usize));
            }
            InputMode::Normal => match self.focus {
                Focus::Sidebar => {
                    let items = self.sidebar_items();
                    if items.is_empty() {
                        return;
                    }
                    let n = items.len() as isize;
                    let cur = self.sidebar_state.selected().unwrap_or(0) as isize;
                    let next = (cur + delta).rem_euclid(n) as usize;
                    self.sidebar_state.select(Some(next));
                    self.sync_view_from_sidebar();
                }
                Focus::Main => {
                    if matches!(self.view, View::Spotify) {
                        if self.spotify_open_col.is_some() {
                            if !self.spotify_tracks.is_empty() {
                                let n = self.spotify_tracks.len() as isize;
                                let cur = self.spotify_track_state.selected().unwrap_or(0) as isize;
                                self.spotify_track_state
                                    .select(Some((cur + delta).rem_euclid(n) as usize));
                            }
                        } else if !self.spotify_cols.is_empty() {
                            let n = self.spotify_cols.len() as isize;
                            let cur = self.spotify_col_state.selected().unwrap_or(0) as isize;
                            self.spotify_col_state
                                .select(Some((cur + delta).rem_euclid(n) as usize));
                        }
                        return;
                    }
                    if self.track_list.is_empty() {
                        return;
                    }
                    let n = self.track_list.len() as isize;
                    let cur = self.track_state.selected().unwrap_or(0) as isize;
                    self.track_state
                        .select(Some((cur + delta).rem_euclid(n) as usize));
                }
            },
            _ => {}
        }
    }

    fn current_track_entry(&self) -> Option<&TrackEntry> {
        self.player
            .current_file
            .as_deref()
            .and_then(|f| self.lib.entries.get(f))
    }
}

fn rect_contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x.saturating_add(r.width) && row >= r.y && row < r.y.saturating_add(r.height)
}

/// Renders word-by-word synced lyrics (no GPU shader — pure crisp terminal styling):
/// - Main vocal active word: bold green with smooth character-by-character sweep
/// - Parallel adlibs `( ... )`: bold yellow sweeping AT THE SAME TIME on the same line
/// - Sung main words: bright white | Sung adlibs: yellow | Upcoming: dark gray
fn render_word_synced_lyrics(app: &App, area_w: u16, area_h: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let h = area_h.saturating_sub(2) as usize;
    let inner_w = area_w.saturating_sub(4).max(16) as usize;

    if app.lrc_lines.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if app.player.current_file.is_some() {
                "No synced lyrics for this track"
            } else {
                "Play a track to see live lyrics"
            },
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    let offset_sec = app
        .current_track_entry()
        .map(|e| e.lrc_offset_ms as f64 / 1000.0)
        .unwrap_or(0.0);
    let pos = app.smooth_pos + 0.04 + offset_sec;
    let active_idx = app
        .lrc_lines
        .iter()
        .rposition(|l| l.time_sec <= pos)
        .unwrap_or(0);

    let above = (h / 3).max(1);
    let start = active_idx.saturating_sub(above);
    let end = (start + h).min(app.lrc_lines.len());

    for i in start..end {
        let entry = &app.lrc_lines[i];
        if i != active_idx {
            let color = if i + 1 == active_idx || i == active_idx + 1 {
                Color::Gray
            } else {
                Color::DarkGray
            };
            lines.push(Line::from(Span::styled(
                entry.text.clone(),
                Style::default().fg(color),
            )));
            continue;
        }

        // Build wrapped lines of word-by-word styled spans for the active bar.
        // Main words and parenthesized adlibs are evaluated independently against `pos`
        // so both can light up simultaneously on the exact same line!
        let mut current_row: Vec<Span<'static>> = Vec::new();
        let mut row_len = 0usize;

        for (wi, w) in entry.words.iter().enumerate() {
            let w_chars: Vec<char> = w.text.chars().collect();
            let w_len = w_chars.len();
            let need_space = if current_row.is_empty() { 0 } else { 1 };
            if !current_row.is_empty() && row_len + need_space + w_len > inner_w {
                lines.push(Line::from(std::mem::take(&mut current_row)));
                row_len = 0;
            }
            if !current_row.is_empty() {
                current_row.push(Span::raw(" "));
                row_len += 1;
            }

            let dur = (w.end - w.start).max(0.08);
            let active_color = if w.is_adlib { Color::Yellow } else { Color::Green };
            let sung_style = if w.is_adlib {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };

            if pos >= w.end {
                current_row.push(Span::styled(w.text.clone(), sung_style));
            } else if pos >= w.start - 0.02 {
                // Smooth character-by-character sweep inside the word
                let frac = ((pos - w.start) / dur).clamp(0.0, 1.0);
                let split_at = ((w_len as f64) * frac).ceil().clamp(1.0, w_len as f64) as usize;
                let lit: String = w_chars[..split_at].iter().collect();
                let rest: String = w_chars[split_at..].iter().collect();
                current_row.push(Span::styled(
                    lit,
                    Style::default()
                        .fg(active_color)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ));
                if !rest.is_empty() {
                    current_row.push(Span::styled(
                        rest,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            } else {
                current_row.push(Span::styled(
                    w.text.clone(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            row_len += w_len;

            if wi + 1 == entry.words.len() && !current_row.is_empty() {
                lines.push(Line::from(std::mem::take(&mut current_row)));
            }
        }
    }
    lines
}

fn run_tui(max_mb: u64) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(max_mb);

    loop {
        app.poll_worker();
        app.tick_playback();

        terminal.draw(|f| {
            app.hits.buttons.clear();

            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // 0: Top cache bar
                    Constraint::Min(10),   // 1: Body: sidebar+cover | tracks | live lyrics
                    Constraint::Length(3), // 2: Now Playing progress + time left
                    Constraint::Length(3), // 3: Clickable simplistic controls bar
                    Constraint::Length(3), // 4: Status / input prompt
                ])
                .split(f.area());

            // 1. Top Cache Gauge
            let used = total_cache_bytes(&app.lib);
            let ratio = ((used as f64) / (app.max_bytes.max(1) as f64)).clamp(0.0, 1.0);
            let top_label = format!(
                "Cache: {} / {} ({} tracks)",
                fmt_mb(used),
                fmt_mb(app.max_bytes),
                app.lib.entries.len(),
            );
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title(" itmp "))
                .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
                .ratio(ratio)
                .label(top_label);
            f.render_widget(gauge, outer[0]);

            // 2. Body split
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Min(30)])
                .split(outer[1]);

            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(12)])
                .split(body[0]);

            app.hits.sidebar = left[0];

            // Sidebar (simplistic, zero emoji)
            let sb_items: Vec<ListItem> = app
                .sidebar_items()
                .into_iter()
                .map(|(label, _)| ListItem::new(label))
                .collect();
            let sb_border = if app.focus == Focus::Sidebar {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let sb_list = List::new(sb_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(sb_border)
                        .title(" Library "),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");
            f.render_stateful_widget(sb_list, left[0], &mut app.sidebar_state);

            // Album Cover Box (bottom-left)
            let cov_inner_w = left[1].width.saturating_sub(2);
            let cov_inner_h = left[1].height.saturating_sub(2);
            app.ensure_cover_art(cov_inner_w, cov_inner_h);
            let cov_title = app
                .current_track_entry()
                .map(|e| {
                    if !e.album.is_empty() {
                        format!(" {} ", e.album)
                    } else {
                        " Cover ".to_string()
                    }
                })
                .unwrap_or_else(|| " Cover ".to_string());
            let cov_widget = if let Some(art) = &app.cover_art {
                Paragraph::new(art.lines.clone())
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL).title(cov_title))
            } else {
                Paragraph::new("\n\n  No Cover")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(Block::default().borders(Borders::ALL).title(cov_title))
            };
            f.render_widget(cov_widget, left[1]);

            // Right side: always-visible Search bar on top + (Tracks/Search Results | Live Lyrics) below
            let right_rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(7)])
                .split(body[1]);

            app.hits.search_box = right_rows[0];
            let is_searching = matches!(app.input_mode, InputMode::Search);
            let search_title = if is_searching {
                " Search (Enter to search, Esc to cancel) "
            } else {
                " Search (click or press / to find & download songs) "
            };
            let (search_text, search_style, search_border) = if is_searching {
                (
                    format!("/ {}_", app.input_buf),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Green),
                )
            } else if !app.last_search_query.is_empty() && matches!(app.view, View::Search) {
                (
                    format!("/ {}  (press / or click to search again)", app.last_search_query),
                    Style::default().fg(Color::White),
                    Style::default().fg(Color::Green),
                )
            } else {
                (
                    "/ Search songs, artists, or paste Spotify/YouTube URL...".to_string(),
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::DarkGray),
                )
            };
            let search_widget = Paragraph::new(search_text)
                .style(search_style)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(search_border)
                        .title(search_title),
                );
            f.render_widget(search_widget, right_rows[0]);

            let (main_area, lyrics_area_opt) = if app.show_lyrics_pane {
                if right_rows[1].width >= 78 {
                    let cols = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
                        .split(right_rows[1]);
                    (cols[0], Some(cols[1]))
                } else {
                    let rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .split(right_rows[1]);
                    (rows[0], Some(rows[1]))
                }
            } else {
                (right_rows[1], None)
            };

            app.hits.track_list = main_area;

            let main_border = if app.focus == Focus::Main {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            if let InputMode::AddToPlaylist(_) = &app.input_mode {
                let items: Vec<ListItem> = app
                    .lib
                    .playlists
                    .iter()
                    .map(|p| ListItem::new(format!("- {} ({})", p.name, p.tracks.len())))
                    .collect();
                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(" Add to Playlist (Enter/Click select, Esc cancel) "),
                    )
                    .highlight_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("+ ");
                f.render_stateful_widget(list, main_area, &mut app.playlist_pick_state);
            } else if matches!(app.input_mode, InputMode::SearchResults) || matches!(app.view, View::Search) {
                if app.search_results.is_empty() {
                    let msg = if app.busy {
                        format!("\n  Searching for '{}'...", app.last_search_query)
                    } else {
                        "\n  Type a song name or artist in the Search bar above and press Enter.\n  You can also paste a Spotify or YouTube link.\n\n  Results will appear here -- press Enter or click any song to download & play,\n  or press d to download it to your 500 MB cache in the background.".to_string()
                    };
                    let empty_search = Paragraph::new(msg)
                        .style(Style::default().fg(if app.busy { Color::Yellow } else { Color::Gray }))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(" Search & Download "),
                        );
                    f.render_widget(empty_search, main_area);
                } else {
                    let items: Vec<ListItem> = app
                        .search_results
                        .iter()
                        .map(|r| {
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    format!("[{:>5}] ", r.duration),
                                    Style::default().fg(Color::Green),
                                ),
                                Span::raw(format!("{} ", r.title)),
                                Span::styled(
                                    format!("- {}", r.uploader),
                                    Style::default().fg(Color::DarkGray),
                                ),
                            ]))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(format!(
                                    " Results ({}) [Enter/Click: Play & Save | d: Download | /: New] ",
                                    app.search_results.len()
                                )),
                        )
                        .highlight_style(
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("+ ");
                    f.render_stateful_widget(list, main_area, &mut app.search_state);
                }
            } else if matches!(app.view, View::Spotify) {
                if let Some(col) = &app.spotify_open_col {
                    let items: Vec<ListItem> = app
                        .spotify_tracks
                        .iter()
                        .map(|tr| {
                            let dur = if tr.duration_sec > 0 {
                                fmt_time(tr.duration_sec as f64)
                            } else {
                                "--:--".to_string()
                            };
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    format!("[{:>5}] ", dur),
                                    Style::default().fg(Color::Green),
                                ),
                                Span::raw(format!("{} ", tr.title)),
                                Span::styled(
                                    format!("- {}", tr.artist),
                                    Style::default().fg(Color::DarkGray),
                                ),
                            ]))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(format!(
                                    " Spotify: {} ({}) [Enter/Click: Play & Save | d: Dl | a: Import All | Esc: Back] ",
                                    col.name,
                                    app.spotify_tracks.len()
                                )),
                        )
                        .highlight_style(
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("+ ");
                    f.render_stateful_widget(list, main_area, &mut app.spotify_track_state);
                } else if !app.spotify_cols.is_empty() {
                    let items: Vec<ListItem> = app
                        .spotify_cols
                        .iter()
                        .map(|c| {
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    format!("{} ", c.name),
                                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!("({}) - {}", c.track_count, c.subtitle),
                                    Style::default().fg(Color::DarkGray),
                                ),
                            ]))
                        })
                        .collect();
                    let header = if app.spotify.is_logged_in() {
                        format!(
                            " Spotify - {} [Enter/Click: Open | p: Add URL/CSV | R: Refresh | X: Logout] ",
                            app.spotify.display_name
                        )
                    } else {
                        " Spotify [Enter/Click: Open | p: Add Playlist/Album URL or CSV | L-login: c] ".to_string()
                    };
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(header),
                        )
                        .highlight_style(
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, main_area, &mut app.spotify_col_state);
                } else {
                    let login_text = if app.spotify.is_logged_in() {
                        format!(
                            "\n  Logged in as: {}\n\n  Note: Spotify blocks Developer Web API on free accounts (HTTP 403\n  Premium required). You can STILL import playlists & songs free:\n\n  [p] Add ANY Spotify Playlist / Album / Track URL (or Exportify .csv /\n      Spotify YourLibrary.json) -- saves right here with zero Premium!\n\n  [R] Retry Spotify API (if you have Spotify Premium)\n  [X] Log out of Spotify",
                            app.spotify.display_name
                        )
                    } else {
                        format!(
                            "\n  Spotify Import & Login\n\n  FREE (No Premium needed):\n    Press [p] or click here to paste ANY Spotify Playlist / Album URL\n    or an Exportify .csv / Spotify Privacy .json export file.\n    It saves right here so you can browse, play, or press [a] to import all.\n\n  SPOTIFY ACCOUNT LOGIN (Requires Spotify Premium for Developer API):\n    1. Press [o] -> developer.spotify.com/dashboard\n       Add Redirect URI: {SPOTIFY_REDIRECT_URI}\n    2. Press [c] to paste your Client ID and log in via browser."
                        )
                    };
                    let card = Paragraph::new(login_text)
                        .style(Style::default().fg(Color::White))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(" Spotify [p: Add URL/CSV (Free) | c: OAuth Login (Premium) | o: Dashboard] "),
                        );
                    f.render_widget(card, main_area);
                }
            } else {
                    let pane_title = match &app.view {
                        View::Search => " Search ".into(),
                        View::Spotify => " Spotify ".into(),
                        View::AllTracks => format!(" All Tracks ({}) ", app.track_list.len()),
                        View::Liked => format!(" * Liked ({}) ", app.track_list.len()),
                        View::Queue => format!(" Queue ({}) ", app.track_list.len()),
                        View::Playlist(idx) => app
                            .lib
                            .playlists
                            .get(*idx)
                            .map(|p| format!(" {} ({}) ", p.name, p.tracks.len()))
                            .unwrap_or_else(|| " Playlist ".into()),
                    };

                    // Pre-collect metadata so we can mutably borrow app.thumbs cleanly
                    let cur_playing = app.player.current_file.clone();
                    let is_paused = app.pb.paused;
                    let row_data: Vec<(String, String, String, String, bool, bool, u64, u64, Option<PathBuf>)> = app
                        .track_list
                        .iter()
                        .filter_map(|fname| {
                            let e = app.lib.entries.get(fname)?;
                            Some((
                                fname.clone(),
                                e.title.clone(),
                                e.artist.clone(),
                                e.album.clone(),
                                e.liked,
                                has_lrc(&app.dir, fname),
                                e.duration_sec,
                                e.size_bytes,
                                app.cover_path_for(fname),
                            ))
                        })
                        .collect();

                    let title_max = main_area.width.saturating_sub(18).clamp(14, 34) as usize;
                    let items: Vec<ListItem> = row_data
                        .into_iter()
                        .map(|(fname, title, artist, album, liked, lrc_ok, dur_sec, size_b, cov_p)| {
                            let thumb_rows = cov_p
                                .as_deref()
                                .and_then(|p| app.thumbs.get_or_load(p))
                                .cloned();

                            let mut line1_spans: Vec<Span<'static>> = Vec::new();
                            let mut line2_spans: Vec<Span<'static>> = Vec::new();

                            if let Some([t0, t1]) = thumb_rows {
                                line1_spans.extend(t0);
                                line2_spans.extend(t1);
                            } else {
                                line1_spans.push(Span::styled("    ", Style::default().bg(Color::DarkGray)));
                                line2_spans.push(Span::styled("    ", Style::default().bg(Color::DarkGray)));
                            }
                            line1_spans.push(Span::raw(" "));
                            line2_spans.push(Span::raw(" "));

                            let is_cur = cur_playing.as_deref() == Some(fname.as_str());
                            let status_tag = if is_cur {
                                if is_paused { "|| " } else { ">  " }
                            } else if liked {
                                "*  "
                            } else {
                                "   "
                            };
                            let tag_style = if is_cur {
                                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                            } else if liked {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default().fg(Color::DarkGray)
                            };

                            let mut t_disp = title;
                            if t_disp.chars().count() > title_max {
                                t_disp = t_disp.chars().take(title_max - 1).collect::<String>() + "..";
                            }
                            line1_spans.push(Span::styled(status_tag, tag_style));
                            line1_spans.push(Span::styled(
                                t_disp,
                                if is_cur {
                                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(Color::White)
                                },
                            ));

                            let mut sub_parts = Vec::new();
                            if !artist.is_empty() {
                                sub_parts.push(artist);
                            }
                            if !album.is_empty() {
                                sub_parts.push(album);
                            }
                            let dur_s = if dur_sec > 0 {
                                fmt_time(dur_sec as f64)
                            } else {
                                "--:--".to_string()
                            };
                            let lrc_s = if lrc_ok { "LRC" } else { "---" };
                            let sub_text = if sub_parts.is_empty() {
                                format!("   {} | {} | {}", dur_s, lrc_s, fmt_mb(size_b))
                            } else {
                                format!("   {} | {} | {}", sub_parts.join(" - "), dur_s, lrc_s)
                            };
                            line2_spans.push(Span::styled(sub_text, Style::default().fg(Color::DarkGray)));

                            ListItem::new(vec![Line::from(line1_spans), Line::from(line2_spans)])
                        })
                        .collect();

                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(main_border)
                                .title(pane_title),
                        )
                        .highlight_style(
                            Style::default()
                                .bg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("| ");
                    f.render_stateful_widget(list, main_area, &mut app.track_state);
            }

            // Live Word-by-Word Synced Lyrics panel (no shader, pure crisp terminal tween)
            if let Some(lrc_area) = lyrics_area_opt {
                let lrc_title = app
                    .current_track_entry()
                    .map(|e| {
                        let mut tags = Vec::new();
                        if app.lrc_audio_synced {
                            tags.push("audio-sync".to_string());
                        }
                        if e.lrc_offset_ms != 0 {
                            tags.push(format!("{:+.2}s", e.lrc_offset_ms as f64 / 1000.0));
                        }
                        if tags.is_empty() {
                            format!(" Lyrics - {} ([ / ] sync) ", e.title)
                        } else {
                            format!(" Lyrics - {} [{}] ", e.title, tags.join(" "))
                        }
                    })
                    .unwrap_or_else(|| " Lyrics ".to_string());
                let lrc_lines = render_word_synced_lyrics(&app, lrc_area.width, lrc_area.height);
                let lrc_widget = Paragraph::new(lrc_lines)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray))
                            .title(lrc_title),
                    );
                f.render_widget(lrc_widget, lrc_area);
            }

            // 3. Now Playing Progress Bar with Elapsed, Total, AND Time Left (-MM:SS left)
            app.hits.progress_bar = outer[2];
            let np_ratio = if app.pb.dur > 0.1 {
                (app.smooth_pos / app.pb.dur).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let np_label = if let Some(e) = app.current_track_entry() {
                let icon = if app.pb.active && !app.pb.paused { ">" } else { "||" };
                let liked_mark = if e.liked { " [*]" } else { "" };
                let artist = if e.artist.is_empty() {
                    String::new()
                } else {
                    format!(" - {}", e.artist)
                };
                if app.pb.dur > 0.1 {
                    let left_sec = (app.pb.dur - app.smooth_pos).max(0.0);
                    format!(
                        "{} {}{}{}   {} / {}  (-{} left)",
                        icon,
                        e.title,
                        artist,
                        liked_mark,
                        fmt_time(app.smooth_pos),
                        fmt_time(app.pb.dur),
                        fmt_time(left_sec),
                    )
                } else {
                    format!("{} {}{}{}   0:00 / --:--", icon, e.title, artist, liked_mark)
                }
            } else {
                "Idle - click a song or press Enter".to_string()
            };
            let np_gauge = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Now Playing (click bar to seek) "),
                )
                .gauge_style(Style::default().fg(Color::Yellow).bg(Color::DarkGray))
                .ratio(np_ratio)
                .label(np_label);
            f.render_widget(np_gauge, outer[2]);

            // 4. Clean Simplistic Clickable Controls Bar (ZERO emojis!)
            app.hits.controls_bar = outer[3];
            let is_playing = app.pb.active && !app.pb.paused;
            let cur_liked = app.current_track_entry().map(|e| e.liked).unwrap_or(false);

            let btn_defs: Vec<(String, Style, Option<ButtonAction>)> = vec![
                ("[ << Prev ]".into(), Style::default().fg(Color::White), Some(ButtonAction::Prev)),
                (
                    if is_playing { "[ || Pause ]".into() } else { "[ > Play ]".into() },
                    if is_playing {
                        Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    },
                    Some(ButtonAction::PlayPause),
                ),
                ("[ >> Next ]".into(), Style::default().fg(Color::White), Some(ButtonAction::Next)),
                (
                    format!("[ Shuf: {} ]", if app.shuffle { "on" } else { "off" }),
                    if app.shuffle {
                        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                    Some(ButtonAction::Shuffle),
                ),
                (
                    format!("[ Rep: {} ]", app.repeat.label()),
                    if app.repeat != RepeatMode::Off {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                    Some(ButtonAction::Repeat),
                ),
                (
                    if cur_liked { "[ * Liked ]".into() } else { "[ + Like ]".into() },
                    if cur_liked {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                    Some(ButtonAction::Like),
                ),
                (
                    "[ + Import ]".into(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    Some(ButtonAction::Import),
                ),
                (
                    format!("[ Lrc: {} ]", if app.show_lyrics_pane { "on" } else { "off" }),
                    if app.show_lyrics_pane {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                    Some(ButtonAction::ToggleLyrics),
                ),
                ("[-]".into(), Style::default().fg(Color::White), Some(ButtonAction::VolDown)),
                (format!("Vol {}%", app.player.volume), Style::default().fg(Color::Gray), None),
                ("[+]".into(), Style::default().fg(Color::White), Some(ButtonAction::VolUp)),
            ];

            let mut spans = Vec::new();
            let mut cursor_x = outer[3].x + 1;
            let max_x = outer[3].x + outer[3].width.saturating_sub(1);
            let btn_y = outer[3].y + 1;
            for (label, style, action_opt) in btn_defs {
                let w = label.chars().count() as u16;
                if cursor_x + w > max_x {
                    break;
                }
                if let Some(action) = action_opt {
                    app.hits.buttons.push((Rect::new(cursor_x, btn_y, w, 1), action));
                }
                spans.push(Span::styled(label, style));
                cursor_x += w;
                if cursor_x + 1 < max_x {
                    spans.push(Span::raw(" "));
                    cursor_x += 1;
                }
            }
            let ctrl_bar = Paragraph::new(Line::from(spans))
                .block(Block::default().borders(Borders::ALL).title(" Controls "));
            f.render_widget(ctrl_bar, outer[3]);

            // 5. Status / Input Prompt
            let bottom = match &app.input_mode {
                InputMode::Search => Paragraph::new(
                    "Type song name, artist, or Spotify/YouTube URL in the Search bar above -> Enter to search | Esc to cancel",
                )
                .style(Style::default().fg(Color::Green))
                .block(Block::default().borders(Borders::ALL)),
                InputMode::Import => Paragraph::new(format!(
                    "Spotify URL, YouTube URL, or local file/folder path: {}_",
                    app.input_buf
                ))
                .style(Style::default().fg(Color::Cyan))
                .block(Block::default().borders(Borders::ALL).title(" Import Music (Enter import, Esc cancel) ")),
                InputMode::SpotifyClientId => Paragraph::new(format!(
                    "Paste Spotify Client ID (Redirect URI: {SPOTIFY_REDIRECT_URI}): {}_",
                    app.input_buf
                ))
                .style(Style::default().fg(Color::Green))
                .block(Block::default().borders(Borders::ALL).title(" Spotify Login (Enter opens browser, Esc cancel) ")),
                InputMode::SpotifyAddUrl => Paragraph::new(format!(
                    "Paste Spotify playlist/album/track URL or Exportify .csv / .json path: {}_",
                    app.input_buf
                ))
                .style(Style::default().fg(Color::Green))
                .block(Block::default().borders(Borders::ALL).title(" Add Spotify Playlist/Album (Free, no Premium) ")),
                InputMode::NewPlaylist => Paragraph::new(format!("New playlist name: {}_", app.input_buf))
                    .style(Style::default().fg(Color::Yellow))
                    .block(Block::default().borders(Borders::ALL).title(" Create Playlist (Enter save, Esc cancel) ")),
                _ => Paragraph::new(app.status.as_str())
                    .style(Style::default().fg(if app.busy { Color::Yellow } else { Color::Gray }))
                    .block(Block::default().borders(Borders::ALL)),
            };
            f.render_widget(bottom, outer[4]);
        })?;

        if event::poll(Duration::from_millis(45))? {
            match event::read()? {
                Event::Mouse(me) => app.handle_mouse(me.kind, me.column, me.row),
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        break;
                    }
                    match &app.input_mode {
                        InputMode::Search => match key.code {
                            KeyCode::Esc => {
                                app.input_buf.clear();
                                if !app.search_results.is_empty() {
                                    app.input_mode = InputMode::SearchResults;
                                } else {
                                    app.view = View::AllTracks;
                                    app.sidebar_state.select(Some(2));
                                    app.input_mode = InputMode::Normal;
                                    app.rebuild_track_list();
                                }
                            }
                            KeyCode::Enter => app.start_youtube_search(),
                            KeyCode::Backspace => {
                                app.input_buf.pop();
                            }
                            KeyCode::Char(c) => app.input_buf.push(c),
                            _ => {}
                        },
                        InputMode::SpotifyClientId => match key.code {
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.input_buf.clear();
                            }
                            KeyCode::Enter => {
                                let cid = app.input_buf.trim().to_string();
                                app.input_buf.clear();
                                app.start_spotify_login(cid);
                            }
                            KeyCode::Backspace => {
                                app.input_buf.pop();
                            }
                            KeyCode::Char(c) => app.input_buf.push(c),
                            _ => {}
                        },
                        InputMode::SpotifyAddUrl => match key.code {
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.input_buf.clear();
                            }
                            KeyCode::Enter => {
                                let raw = app.input_buf.trim().to_string();
                                app.input_buf.clear();
                                app.start_spotify_add_public_url(raw);
                            }
                            KeyCode::Backspace => {
                                app.input_buf.pop();
                            }
                            KeyCode::Char(c) => app.input_buf.push(c),
                            _ => {}
                        },
                        InputMode::Import => match key.code {
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.input_buf.clear();
                            }
                            KeyCode::Enter => {
                                let raw = app.input_buf.trim().to_string();
                                app.input_buf.clear();
                                app.start_smart_import(raw);
                            }
                            KeyCode::Backspace => {
                                app.input_buf.pop();
                            }
                            KeyCode::Char(c) => app.input_buf.push(c),
                            _ => {}
                        },
                        InputMode::NewPlaylist => match key.code {
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.input_buf.clear();
                            }
                            KeyCode::Enter => {
                                let name = app.input_buf.trim().to_string();
                                app.input_buf.clear();
                                app.input_mode = InputMode::Normal;
                                if !name.is_empty() {
                                    app.lib.playlists.push(Playlist {
                                        name: name.clone(),
                                        tracks: Vec::new(),
                                    });
                                    save_library(&app.dir, &app.lib);
                                    app.status = format!("Created playlist '{name}'");
                                }
                            }
                            KeyCode::Backspace => {
                                app.input_buf.pop();
                            }
                            KeyCode::Char(c) => app.input_buf.push(c),
                            _ => {}
                        },
                        InputMode::SearchResults => match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                app.view = View::AllTracks;
                                app.sidebar_state.select(Some(2));
                                app.input_mode = InputMode::Normal;
                                app.rebuild_track_list();
                                app.status = "Returned to All Tracks.".into();
                            }
                            KeyCode::Char('/') | KeyCode::Char('s') => {
                                app.trigger_button(ButtonAction::Search);
                            }
                            KeyCode::Char(' ') => app.trigger_button(ButtonAction::PlayPause),
                            KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                            KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                            KeyCode::Enter => {
                                if !app.busy {
                                    app.start_download_search_result(true);
                                }
                            }
                            KeyCode::Char('d') => {
                                if !app.busy {
                                    app.start_download_search_result(false);
                                }
                            }
                            _ => {}
                        },
                        InputMode::AddToPlaylist(fname) => {
                            let fname = fname.clone();
                            match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    app.input_mode = InputMode::Normal;
                                }
                                KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                                KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                                KeyCode::Enter => {
                                    if let Some(idx) = app.playlist_pick_state.selected() {
                                        if let Some(pl) = app.lib.playlists.get(idx) {
                                            let pl_name = pl.name.clone();
                                            add_track_to_playlist(&mut app.lib, &pl_name, &fname);
                                            save_library(&app.dir, &app.lib);
                                            self_status_msg(&mut app, format!("Added to playlist '{pl_name}'"));
                                        }
                                    }
                                    app.input_mode = InputMode::Normal;
                                }
                                _ => {}
                            }
                        }
                        InputMode::Normal => match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Tab => {
                                app.focus = match app.focus {
                                    Focus::Sidebar => Focus::Main,
                                    Focus::Main => Focus::Sidebar,
                                };
                            }
                            KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                            KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                            KeyCode::Right => app.player.seek_rel(5.0),
                            KeyCode::Left => app.player.seek_rel(-5.0),
                            KeyCode::Char(' ') => app.trigger_button(ButtonAction::PlayPause),
                            KeyCode::Char('n') => app.trigger_button(ButtonAction::Next),
                            KeyCode::Char('b') => app.trigger_button(ButtonAction::Prev),
                            KeyCode::Char('+') | KeyCode::Char('=') => app.trigger_button(ButtonAction::VolUp),
                            KeyCode::Char('-') | KeyCode::Char('_') => app.trigger_button(ButtonAction::VolDown),
                            KeyCode::Char('z') => app.trigger_button(ButtonAction::Shuffle),
                            KeyCode::Char('r') => app.trigger_button(ButtonAction::Repeat),
                            KeyCode::Char('L') => app.trigger_button(ButtonAction::ToggleLyrics),
                            KeyCode::Char('[') => app.adjust_lrc_offset(Some(-50)),
                            KeyCode::Char(']') => app.adjust_lrc_offset(Some(50)),
                            KeyCode::Char('\\') => app.adjust_lrc_offset(None),
                            KeyCode::Char('i') => app.trigger_button(ButtonAction::Import),
                            KeyCode::Char('S') => app.open_spotify_view(),
                            KeyCode::Char('/') | KeyCode::Char('s') => app.trigger_button(ButtonAction::Search),
                            KeyCode::Char('l') => app.trigger_button(ButtonAction::Like),
                            KeyCode::Char('p') if matches!(app.view, View::Spotify) => {
                                if !app.busy {
                                    app.input_buf.clear();
                                    app.input_mode = InputMode::SpotifyAddUrl;
                                }
                            }
                            KeyCode::Char('c') if matches!(app.view, View::Spotify) && !app.spotify.is_logged_in() => {
                                if !app.busy {
                                    app.input_buf = app.spotify.client_id.clone();
                                    app.input_mode = InputMode::SpotifyClientId;
                                }
                            }
                            KeyCode::Char('o') if matches!(app.view, View::Spotify) && !app.spotify.is_logged_in() => {
                                open_url_in_browser("https://developer.spotify.com/dashboard");
                                app.status = format!(
                                    "Opened Spotify Dashboard -- set Redirect URI to {SPOTIFY_REDIRECT_URI}, then press [c]"
                                );
                            }
                            KeyCode::Char('R') if matches!(app.view, View::Spotify) && app.spotify.is_logged_in() => {
                                app.spotify_open_col = None;
                                app.spotify_tracks.clear();
                                app.start_spotify_load_collections();
                            }
                            KeyCode::Char('X') if matches!(app.view, View::Spotify) && app.spotify.is_logged_in() => {
                                app.logout_spotify();
                            }
                            KeyCode::Char('a') if matches!(app.view, View::Spotify) && app.spotify_open_col.is_some() => {
                                app.start_spotify_import_all();
                            }
                            KeyCode::Esc | KeyCode::Backspace
                                if matches!(app.view, View::Spotify) && app.spotify_open_col.is_some() =>
                            {
                                app.spotify_open_col = None;
                                app.spotify_tracks.clear();
                                app.status = "Back to Spotify library.".into();
                            }
                            KeyCode::Enter => {
                                if app.focus == Focus::Sidebar {
                                    app.sync_view_from_sidebar();
                                    app.focus = Focus::Main;
                                } else if matches!(app.view, View::Spotify) {
                                    if app.spotify_open_col.is_some() {
                                        if let Some(i) = app.spotify_track_state.selected() {
                                            app.start_spotify_download_track(i, true);
                                        }
                                    } else if !app.spotify_cols.is_empty() {
                                        if let Some(i) = app.spotify_col_state.selected() {
                                            app.start_spotify_open_collection(i);
                                        }
                                    } else if !app.busy {
                                        app.input_buf.clear();
                                        app.input_mode = InputMode::SpotifyAddUrl;
                                    }
                                } else if let Some(fname) = app.selected_track() {
                                    if matches!(app.view, View::Queue) {
                                        if let Some(i) = app.track_state.selected() {
                                            app.queue.remove(i);
                                            app.rebuild_track_list();
                                        }
                                    }
                                    app.play_filename(&fname, true);
                                }
                            }
                            KeyCode::Char('Q') => {
                                if let Some(fname) = app.selected_track() {
                                    app.queue.push_back(fname.clone());
                                    let t = app
                                        .lib
                                        .entries
                                        .get(&fname)
                                        .map(|e| e.title.clone())
                                        .unwrap_or(fname);
                                    app.status = format!("Queued next: {t}");
                                }
                            }
                            KeyCode::Char('N') => {
                                app.input_buf.clear();
                                app.input_mode = InputMode::NewPlaylist;
                            }
                            KeyCode::Char('P') => {
                                if let Some(fname) = app.selected_track() {
                                    if app.lib.playlists.is_empty() {
                                        app.status = "No playlists yet - press N to create one".into();
                                    } else {
                                        app.playlist_pick_state.select(Some(0));
                                        app.input_mode = InputMode::AddToPlaylist(fname);
                                    }
                                }
                            }
                            KeyCode::Char('d') => {
                                if matches!(app.view, View::Spotify) && app.spotify_open_col.is_some() {
                                    if let Some(i) = app.spotify_track_state.selected() {
                                        app.start_spotify_download_track(i, false);
                                    }
                                } else if let View::Playlist(idx) = app.view {
                                    if let Some(sel) = app.track_state.selected() {
                                        if let Some(pl) = app.lib.playlists.get_mut(idx) {
                                            if sel < pl.tracks.len() {
                                                pl.tracks.remove(sel);
                                                save_library(&app.dir, &app.lib);
                                                app.rebuild_track_list();
                                                app.status = "Removed from playlist".into();
                                            }
                                        }
                                    }
                                } else if let Some(fname) = app.selected_track() {
                                    let t = app
                                        .lib
                                        .entries
                                        .get(&fname)
                                        .map(|e| e.title.clone())
                                        .unwrap_or_default();
                                    remove_track_files(&app.dir, &fname);
                                    app.lib.entries.remove(&fname);
                                    for pl in &mut app.lib.playlists {
                                        pl.tracks.retain(|x| x != &fname);
                                    }
                                    save_library(&app.dir, &app.lib);
                                    app.rebuild_track_list();
                                    app.status = format!("Deleted '{t}' from cache");
                                }
                            }
                            KeyCode::Char('c') => {
                                let (n, freed) = prune_lru(&app.dir, &mut app.lib, app.max_bytes);
                                app.rebuild_track_list();
                                app.status = if n > 0 {
                                    format!("Pruned {n} tracks (freed {})", fmt_mb(freed))
                                } else {
                                    format!(
                                        "Cache under {} ({})",
                                        fmt_mb(app.max_bytes),
                                        fmt_mb(total_cache_bytes(&app.lib))
                                    )
                                };
                            }
                            _ => {}
                        },
                    }
                }
                _ => {}
            }
        }
    }

    app.player.stop();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn self_status_msg(app: &mut App, msg: String) {
    app.status = msg;
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let max_mb: u64 = env::var("ITMP_CACHE_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_MB);
    let max_bytes = max_mb * 1_048_576;
    let dir = audio_dir();

    if args.is_empty() {
        if let Err(e) = run_tui(max_mb) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    match args[0].as_str() {
        "import" => {
            let raw = args[1..].join(" ");
            if raw.is_empty() {
                eprintln!("Usage: itmp import <spotify-url | youtube-url | local-file-or-folder | export.csv>");
                std::process::exit(1);
            }
            if expand_local_path(&raw).exists() {
                match import_local_music(&dir, &raw, max_bytes) {
                    Ok(m) => println!("{m}"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            } else if raw.contains("open.spotify.com/") || raw.starts_with("spotify:") {
                match fetch_spotify_metadata(&raw) {
                    Ok(bundle) => {
                        println!(
                            "Importing {} '{}' ({} tracks)...",
                            bundle.kind,
                            bundle.collection_name,
                            bundle.tracks.len()
                        );
                        let pl = if bundle.kind == "track" {
                            None
                        } else {
                            Some(bundle.collection_name.as_str())
                        };
                        for (i, tr) in bundle.tracks.iter().enumerate() {
                            print!(
                                "[{}/{}] {} - {} ... ",
                                i + 1,
                                bundle.tracks.len(),
                                tr.artist,
                                tr.title
                            );
                            let q = format!("ytsearch1:{} {} audio", tr.artist, tr.title);
                            match download_and_cache_track(
                                &dir,
                                &q,
                                &tr.title,
                                &tr.artist,
                                &tr.album,
                                tr.cover_url.as_deref().or(bundle.cover_url.as_deref()),
                                tr.duration_sec,
                                pl,
                                max_bytes,
                            ) {
                                Ok((_, msg)) => println!("{msg}"),
                                Err(e) => println!("FAIL ({e})"),
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                match search_youtube(&raw) {
                    Ok(list) if !list.is_empty() => {
                        for item in &list {
                            let url = format!("https://www.youtube.com/watch?v={}", item.id);
                            match download_and_cache_track(
                                &dir,
                                &url,
                                &item.title,
                                &item.uploader,
                                "",
                                Some(&item.thumbnail),
                                0,
                                None,
                                max_bytes,
                            ) {
                                Ok((_, m)) => println!("{m}"),
                                Err(e) => eprintln!("FAIL {}: {e}", item.title),
                            }
                        }
                    }
                    Ok(_) => {
                        eprintln!("No results for '{raw}'");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        "add" | "dl" => {
            let query = args[1..].join(" ");
            if query.is_empty() {
                eprintln!("Usage: itmp add <song name or URL>");
                std::process::exit(1);
            }
            match search_youtube(&query) {
                Ok(list) if !list.is_empty() => {
                    let first = &list[0];
                    let url = format!("https://www.youtube.com/watch?v={}", first.id);
                    match download_and_cache_track(
                        &dir,
                        &url,
                        &first.title,
                        &first.uploader,
                        "",
                        Some(&first.thumbnail),
                        0,
                        None,
                        max_bytes,
                    ) {
                        Ok((_, m)) => println!("{m}"),
                        Err(e) => {
                            eprintln!("error: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                Ok(_) => {
                    eprintln!("No results for '{query}'");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        "list" | "status" => {
            let lib = load_and_sync_library(&dir);
            let mut entries: Vec<&TrackEntry> = lib.entries.values().collect();
            entries.sort_by(|a, b| a.filename.cmp(&b.filename));
            for e in &entries {
                let star = if e.liked { "*" } else { " " };
                let lrc = if has_lrc(&dir, &e.filename) { "LRC" } else { "   " };
                let cov = if e.cover.is_some() { "ART" } else { "   " };
                println!(
                    "{} {:<28} {:<16} [{}] [{}] {:>8}",
                    star,
                    e.title,
                    e.artist,
                    lrc,
                    cov,
                    fmt_mb(e.size_bytes)
                );
            }
            println!(
                "Total: {} / {} ({} tracks, {} playlists)",
                fmt_mb(total_cache_bytes(&lib)),
                fmt_mb(max_bytes),
                entries.len(),
                lib.playlists.len()
            );
        }
        "spotify" => match args.get(1).map(|s| s.as_str()) {
            Some("login") => {
                let cid = args.get(2).cloned().unwrap_or_default();
                if cid.is_empty() {
                    eprintln!("Usage: itmp spotify login <spotify-client-id>");
                    eprintln!("Set Redirect URI in https://developer.spotify.com/dashboard to:");
                    eprintln!("  {SPOTIFY_REDIRECT_URI}");
                    std::process::exit(1);
                }
                match login_pkce(&cid, |m| println!("{m}")) {
                    Ok(sess) => println!("Logged in to Spotify as '{}'!", sess.display_name),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("logout") => {
                clear_spotify_session();
                println!("Logged out of Spotify.");
            }
            Some("status") | None => {
                let sess = load_spotify_session();
                if sess.is_logged_in() {
                    println!("Spotify: logged in as '{}'", sess.display_name);
                } else {
                    println!("Spotify: not logged in (run `itmp spotify login <client-id>` or open Spotify inside TUI)");
                }
            }
            _ => {
                eprintln!("Usage: itmp spotify <login <client-id> | status | logout>");
                std::process::exit(1);
            }
        },
        "prune" => {
            let mut lib = load_and_sync_library(&dir);
            let (n, freed) = prune_lru(&dir, &mut lib, max_bytes);
            println!(
                "Pruned {n} tracks, freed {} (now {})",
                fmt_mb(freed),
                fmt_mb(total_cache_bytes(&lib))
            );
        }
        _ => {
            println!("itmp (in-terminal music player) - lightweight Rust TUI music player & 500 MB LRU cache");
            println!("  itmp                             Launch music player TUI");
            println!("  itmp spotify login <client-id>   Log in to Spotify (OAuth PKCE, Redirect URI {SPOTIFY_REDIRECT_URI})");
            println!("  itmp spotify status|logout       Check or clear Spotify login");
            println!("  itmp import <spotify|yt|path>    Import from Spotify URL/CSV/JSON, YouTube, or local files/folder");
            println!("  itmp add <query|url>             Download track + cover art + synced lyrics");
            println!("  itmp list                        List cached tracks, covers, lyrics & playlists");
            println!("  itmp prune                       Prune least-recently-used tracks to <=500 MB");
        }
    }
}
