mod artist;
mod cache;
mod cover;
mod downloader;
mod genius;
mod lyrics;
mod player;
mod spotify;

use artist::{
    fetch_album_detail, fetch_artist_profile, find_and_fetch_album_by_name,
    parse_spotify_artist_id, search_albums, AlbumDetail, AlbumSummary, ArtistProfile,
};
use cache::{
    add_track_to_playlist, audio_dir, clean_artist_name, find_cached_track, fmt_mb, fmt_time,
    has_lrc, load_and_sync_library, now_ts, prune_lru, remove_track_files, save_library,
    total_cache_bytes, Library, LikedArtist, Playlist, TrackEntry, DEFAULT_MAX_MB,
};
use cover::{decode_thumb, fetch_remote_image_to_temp, CoverArt, ThumbCache, ThumbRows};
use downloader::{
    download_and_cache_track, expand_local_path, fetch_spotify_metadata, import_local_music,
    parse_spotify_export_file, search_youtube, SearchResult, SpotifyTrackSpec,
};
use genius::{fetch_song_annotations, match_lyric_annotation, GeniusSongAnnotations};
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
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::{
    collections::{HashMap, VecDeque},
    env, fs, io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Main,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    AllTracks,
    Liked,
    Queue,
    Playlist(usize),
    Search,
    Artist,
    Album,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtistTab {
    TopTracks,
    All,
    Albums,
    Eps,
    Singles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchTab {
    Songs,
    AllReleases,
    Albums,
    Eps,
    Singles,
}

#[derive(Debug, Clone)]
enum SidebarEntry {
    AllTracks,
    Liked,
    Queue,
    Header,
    Artist(usize),
    Playlist(usize),
    NewPlaylist,
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
    SearchAlbumsDone(Result<Vec<AlbumSummary>, String>),
    ThumbReady(String, ThumbRows),
    DownloadDone(Result<(String, String), String>, bool), // (filename, msg), auto_play
    ImportDone(Result<String, String>),
    LyricsAligned(String, Vec<LrcLine>),
    SpotifyLoginDone(Result<SpotifySession, String>),
    SpotifyCollectionsDone(Result<Vec<SpotifyCollection>, String>),
    SpotifyTracksDone(Result<(SpotifyCollection, Vec<SpotifyTrackSpec>), String>),
    SpotifyPublicUrlAdded(Result<(SavedSpotifyLink, Vec<SpotifyTrackSpec>), String>),
    ArtistLoaded(Result<(ArtistProfile, Option<PathBuf>), String>),
    AlbumLoaded(Result<(AlbumDetail, Option<PathBuf>), String>),
    GeniusLoaded(String, String, Result<GeniusSongAnnotations, String>), // (song_key, lyric_line, res)
}

#[derive(Debug, Clone, Copy)]
enum ButtonAction {
    Home,
    SearchBtn,
    ToggleDownloads,
    Settings,
    Prev,
    PlayPause,
    Next,
    Shuffle,
    Repeat,
    Like,
    VolDown,
    VolUp,
    Import,
    ToggleLyrics,
    LyricsReturn,
    OpenQueue,
    CacheDec,
    CacheInc,
    PruneCache,
    SpotifyAddUrl,
    SpotifyLoginPrompt,
    SpotifyDashboard,
    SpotifyRefresh,
    SpotifyLogout,
    ToggleLikeArtist,
    ArtistImportAll,
    ArtistTab(ArtistTab),
    SearchTab(SearchTab),
    AlbumImportAll,
    AlbumBack,
    OpenAlbumArtist,
    OpenCurrentArtist,
    ClosePopup,
}

#[derive(Debug, Clone)]
struct GeniusPopup {
    song_title: String,
    lyric_line: String,
    fragment: String,
    explanation: String,
    url: String,
    loading: bool,
}

#[derive(Default)]
struct HitAreas {
    search_box: Rect,
    dl_btn: Rect,
    sidebar: Rect,
    track_list: Rect,
    settings_list: Rect,
    lyrics_area: Rect,
    progress_bar: Rect,
    controls_bar: Rect,
    buttons: Vec<(Rect, ButtonAction)>,
    lyrics_rows: Vec<(u16, usize, f64)>,       // (screen_y, lrc_idx, time_sec)
    lyric_help_btns: Vec<(Rect, usize)>,       // ([?] hitbox, lrc_idx)
    artist_links: Vec<(Rect, String)>,         // (clickable artist rect, artist_name)
    album_links: Vec<(Rect, String, String)>,  // (clickable album rect, artist, album)
    dl_dropdown: Rect,
    dl_play_rows: Vec<(Rect, String)>,         // (row rect, filename)
    dl_del_btns: Vec<(Rect, String)>,          // ([x] delete rect, filename)
    popup_rect: Rect,
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
    play_context: Vec<String>,
    upcoming: VecDeque<String>,
    history: Vec<String>,
    shuffle: bool,
    repeat: RepeatMode,
    rng_seed: u64,

    lrc_lines: Vec<LrcLine>,
    lrc_file: Option<String>,
    lyrics_scroll_offset: isize,
    cover_art: Option<CoverArt>,
    cover_size: (u16, u16),
    thumbs: ThumbCache,

    input_mode: InputMode,
    input_buf: String,
    last_search_query: String,
    search_tab: SearchTab,
    search_results: Vec<SearchResult>,
    search_state: ListState,
    search_albums_list: Vec<AlbumSummary>,
    search_album_state: ListState,
    playlist_pick_state: ListState,

    // Spotify linking & playlist browser (lives in Settings!)
    spotify: SpotifySession,
    spotify_cols: Vec<SpotifyCollection>,
    spotify_col_state: ListState,
    spotify_open_col: Option<SpotifyCollection>,
    spotify_tracks: Vec<SpotifyTrackSpec>,
    spotify_track_state: ListState,

    // Artist Profile (Spotify + full discography)
    artist_profile: Option<ArtistProfile>,
    artist_loading_name: Option<String>,
    artist_avatar_path: Option<PathBuf>,
    artist_avatar_art: Option<CoverArt>,
    artist_avatar_size: (u16, u16),
    artist_tab: ArtistTab,
    artist_track_state: ListState,
    artist_album_state: ListState,

    // Album / EP / Single Page
    album_detail: Option<AlbumDetail>,
    album_loading_name: Option<String>,
    album_cover_path: Option<PathBuf>,
    album_cover_art: Option<CoverArt>,
    album_cover_size: (u16, u16),
    album_track_state: ListState,
    album_return_view: View,

    hits: HitAreas,
    mouse_pos: (u16, u16),
    last_click: Option<(usize, Instant)>,

    // Top Downloads dropdown & Genius lyric explanation popup
    show_downloads_menu: bool,
    genius_cache: HashMap<String, GeniusSongAnnotations>,
    genius_popup: Option<GeniusPopup>,

    status: String,
    status_at: Instant,
    busy: bool,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
}

impl App {
    fn new(env_max_mb: Option<u64>) -> Self {
        let dir = audio_dir();
        let lib = load_and_sync_library(&dir);
        let max_mb = env_max_mb
            .or(lib.max_cache_mb)
            .unwrap_or(DEFAULT_MAX_MB)
            .clamp(100, 10_000);
        let (tx, rx) = mpsc::channel();
        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(0)); // All Tracks
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
            play_context: Vec::new(),
            upcoming: VecDeque::new(),
            history: Vec::new(),
            shuffle: false,
            repeat: RepeatMode::All,
            rng_seed: now_ts() ^ 0x9e37_79b9_7f4a_7c15,
            lrc_lines: Vec::new(),
            lrc_file: None,
            lyrics_scroll_offset: 0,
            cover_art: None,
            cover_size: (0, 0),
            thumbs: ThumbCache::default(),
            input_mode: InputMode::Normal,
            input_buf: String::new(),
            last_search_query: String::new(),
            search_tab: SearchTab::Songs,
            search_results: Vec::new(),
            search_state: ListState::default(),
            search_albums_list: Vec::new(),
            search_album_state: ListState::default(),
            playlist_pick_state: ListState::default(),
            spotify,
            spotify_cols: initial_sp_cols,
            spotify_col_state: initial_sp_state,
            spotify_open_col: None,
            spotify_tracks: Vec::new(),
            spotify_track_state: ListState::default(),
            artist_profile: None,
            artist_loading_name: None,
            artist_avatar_path: None,
            artist_avatar_art: None,
            artist_avatar_size: (0, 0),
            artist_tab: ArtistTab::TopTracks,
            artist_track_state: ListState::default(),
            artist_album_state: ListState::default(),
            album_detail: None,
            album_loading_name: None,
            album_cover_path: None,
            album_cover_art: None,
            album_cover_size: (0, 0),
            album_track_state: ListState::default(),
            album_return_view: View::AllTracks,
            hits: HitAreas::default(),
            mouse_pos: (0, 0),
            last_click: None,
            show_downloads_menu: false,
            genius_cache: HashMap::new(),
            genius_popup: None,
            status: String::new(),
            status_at: now - Duration::from_secs(10),
            busy: false,
            tx,
            rx,
        };
        app.rebuild_track_list();
        app.play_context = app.track_list.clone();
        app.rebuild_upcoming(None);
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

    fn sidebar_rows(&self) -> Vec<(String, SidebarEntry)> {
        let liked_count = self.lib.entries.values().filter(|e| e.liked).count();
        let mut rows = vec![
            (
                format!("All Tracks ({})", self.lib.entries.len()),
                SidebarEntry::AllTracks,
            ),
            (
                format!("* Liked Songs ({})", liked_count),
                SidebarEntry::Liked,
            ),
            (
                format!("Queue ({})", self.queue.len() + self.upcoming.len()),
                SidebarEntry::Queue,
            ),
        ];

        rows.push(("-- Artists --".to_string(), SidebarEntry::Header));
        if self.lib.liked_artists.is_empty() {
            rows.push(("  (none liked)".to_string(), SidebarEntry::Header));
        } else {
            for (idx, ar) in self.lib.liked_artists.iter().enumerate() {
                rows.push((format!("* {}", ar.name), SidebarEntry::Artist(idx)));
            }
        }

        rows.push(("-- Playlists --".to_string(), SidebarEntry::Header));
        for (idx, pl) in self.lib.playlists.iter().enumerate() {
            rows.push((
                format!("- {} ({})", pl.name, pl.tracks.len()),
                SidebarEntry::Playlist(idx),
            ));
        }
        rows.push(("+ New Playlist".to_string(), SidebarEntry::NewPlaylist));
        rows
    }

    fn activate_sidebar_index(&mut self, idx: usize) {
        let rows = self.sidebar_rows();
        let Some((_, entry)) = rows.get(idx) else {
            return;
        };
        match entry {
            SidebarEntry::Header => return,
            SidebarEntry::AllTracks => {
                self.view = View::AllTracks;
                self.input_mode = InputMode::Normal;
                self.rebuild_track_list();
            }
            SidebarEntry::Liked => {
                self.view = View::Liked;
                self.input_mode = InputMode::Normal;
                self.rebuild_track_list();
            }
            SidebarEntry::Queue => {
                self.view = View::Queue;
                self.input_mode = InputMode::Normal;
                self.rebuild_track_list();
            }
            SidebarEntry::Artist(aidx) => {
                if let Some(ar) = self.lib.liked_artists.get(*aidx).cloned() {
                    self.open_artist_profile(&ar.name, ar.spotify_id.as_deref());
                }
            }
            SidebarEntry::Playlist(pidx) => {
                self.view = View::Playlist(*pidx);
                self.input_mode = InputMode::Normal;
                self.rebuild_track_list();
            }
            SidebarEntry::NewPlaylist => {
                self.input_buf.clear();
                self.input_mode = InputMode::NewPlaylist;
            }
        }
    }

    fn rebuild_track_list(&mut self) {
        let list: Vec<String> = match &self.view {
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
            View::Queue => {
                let mut combined: Vec<String> = self.queue.iter().cloned().collect();
                combined.extend(self.upcoming.iter().cloned());
                combined
            }
            View::Playlist(idx) => self
                .lib
                .playlists
                .get(*idx)
                .map(|p| p.tracks.clone())
                .unwrap_or_default(),
            View::Search | View::Artist | View::Album | View::Settings => Vec::new(),
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
                self.sidebar_state.select(Some(0));
            }
        }
        if !self.lib.entries.is_empty() {
            self.play_context.retain(|f| self.lib.entries.contains_key(f));
            self.upcoming.retain(|f| self.lib.entries.contains_key(f));
            self.queue.retain(|f| self.lib.entries.contains_key(f));
            if self.play_context.is_empty() {
                let mut k: Vec<String> = self.lib.entries.keys().cloned().collect();
                k.sort();
                self.play_context = k;
                let cur = self.player.current_file.clone();
                self.rebuild_upcoming(cur.as_deref());
            }
        }
        self.rebuild_track_list();
    }

    fn rebuild_upcoming(&mut self, current_file: Option<&str>) {
        let mut pool: Vec<String> = if !self.play_context.is_empty() {
            self.play_context
                .iter()
                .filter(|f| self.lib.entries.contains_key(*f))
                .cloned()
                .collect()
        } else {
            let mut k: Vec<String> = self.lib.entries.keys().cloned().collect();
            k.sort();
            k
        };
        self.upcoming.clear();
        if pool.is_empty() {
            return;
        }
        if self.shuffle {
            if let Some(cur) = current_file {
                pool.retain(|x| x != cur);
            }
            // Fisher-Yates shuffle so Queue shows exact upcoming shuffled order
            for i in (1..pool.len()).rev() {
                let j = self.next_rand(i + 1);
                pool.swap(i, j);
            }
            self.upcoming = pool.into();
        } else if let Some(cur) = current_file {
            if let Some(pos) = pool.iter().position(|x| x == cur) {
                for x in &pool[pos + 1..] {
                    self.upcoming.push_back(x.clone());
                }
                if self.repeat == RepeatMode::All {
                    for x in &pool[..pos] {
                        self.upcoming.push_back(x.clone());
                    }
                }
            } else {
                self.upcoming = pool.into();
            }
        } else {
            self.upcoming = pool.into();
        }
        if matches!(self.view, View::Queue) {
            self.rebuild_track_list();
        }
    }

    fn play_from_queue_index(&mut self, idx: usize) {
        if idx < self.queue.len() {
            if let Some(fname) = self.queue.remove(idx) {
                self.play_filename_internal(&fname, true, false);
                self.rebuild_track_list();
            }
        } else {
            let u_idx = idx - self.queue.len();
            if u_idx < self.upcoming.len() {
                let mut picked = None;
                for _ in 0..=u_idx {
                    picked = self.upcoming.pop_front();
                }
                if let Some(fname) = picked {
                    self.play_filename_internal(&fname, true, false);
                    if self.upcoming.is_empty() && self.repeat == RepeatMode::All {
                        self.rebuild_upcoming(Some(&fname));
                    }
                    self.rebuild_track_list();
                }
            }
        }
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
        if !matches!(
            self.view,
            View::Queue | View::Search | View::Artist | View::Album | View::Settings
        ) && !self.track_list.is_empty()
        {
            self.play_context = self.track_list.clone();
        } else if self.play_context.is_empty() {
            let mut k: Vec<String> = self.lib.entries.keys().cloned().collect();
            k.sort();
            self.play_context = k;
        }
        self.play_filename_internal(fname, record_history, true);
    }

    fn play_filename_internal(&mut self, fname: &str, record_history: bool, rebuild_up: bool) {
        let full = self.dir.join(fname);
        if !full.is_file() {
            self.set_notice(format!("File missing: {fname}"));
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
                    self.set_notice(format!("Playing: {label}"));
                }
                save_library(&self.dir, &self.lib);
                self.refresh_lyrics_for(fname);
                self.cover_art = None;
                if rebuild_up {
                    self.rebuild_upcoming(Some(fname));
                }
            }
            Err(e) => {
                self.set_notice(format!("Playback error: {e}"));
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
                    "Liked (pinned from LRU eviction)"
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

    fn is_artist_liked(&self, name: &str) -> bool {
        let target = name.trim().to_ascii_lowercase();
        self.lib
            .liked_artists
            .iter()
            .any(|a| a.name.trim().to_ascii_lowercase() == target)
    }

    fn toggle_like_current_artist(&mut self) {
        let Some(prof) = self.artist_profile.clone() else {
            return;
        };
        let target = prof.name.trim().to_ascii_lowercase();
        if let Some(pos) = self
            .lib
            .liked_artists
            .iter()
            .position(|a| a.name.trim().to_ascii_lowercase() == target)
        {
            self.lib.liked_artists.remove(pos);
            save_library(&self.dir, &self.lib);
            self.status = format!("Removed '{}' from Liked Artists", prof.name);
        } else {
            self.lib.liked_artists.push(LikedArtist {
                name: prof.name.clone(),
                spotify_id: prof.spotify_id.clone(),
                image_url: prof.image_url.clone(),
            });
            self.lib
                .liked_artists
                .sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
            save_library(&self.dir, &self.lib);
            self.status = format!("Liked artist '{}' (added to Library sidebar)", prof.name);
        }
    }

    fn open_artist_profile(&mut self, raw_name: &str, known_spotify_id: Option<&str>) {
        let cleaned = clean_artist_name(raw_name);
        let direct_id = parse_spotify_artist_id(raw_name).or_else(|| known_spotify_id.map(|s| s.to_string()));
        if cleaned.is_empty() && direct_id.is_none() {
            self.status = "No artist metadata on this track".into();
            return;
        }
        let lookup = if direct_id.is_some() {
            raw_name.to_string()
        } else {
            cleaned.clone()
        };
        let label = if cleaned.is_empty() {
            "Spotify Artist".to_string()
        } else {
            cleaned.clone()
        };

        // Check if we already have a stored spotify_id for this liked artist
        let saved_id = direct_id.or_else(|| {
            self.lib
                .liked_artists
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(&label))
                .and_then(|a| a.spotify_id.clone())
        });

        self.view = View::Artist;
        self.focus = Focus::Main;
        self.input_mode = InputMode::Normal;
        self.artist_loading_name = Some(label.clone());
        self.artist_profile = None;
        self.artist_avatar_path = None;
        self.artist_avatar_art = None;
        self.artist_tab = ArtistTab::TopTracks;
        self.artist_track_state.select(None);
        self.artist_album_state.select(None);
        self.busy = true;
        self.status = format!("Loading Spotify profile for '{label}'...");

        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = fetch_artist_profile(&lookup, saved_id.as_deref()).map(|prof| {
                // Download artist avatar atomically BEFORE sending ArtistLoaded so it's 100% race-free!
                let avatar_path = prof
                    .image_url
                    .as_deref()
                    .and_then(|u| fetch_remote_image_to_temp(u, &format!("artist-{}", prof.name)));
                let mut seen_urls = std::collections::HashSet::new();
                let mut thumb_jobs: Vec<(String, String)> = Vec::new();
                for t in &prof.top_tracks {
                    if let Some(u) = &t.cover_url {
                        if seen_urls.insert(u.clone()) {
                            thumb_jobs.push((format!("url:{u}"), u.clone()));
                        }
                    }
                }
                for al in &prof.albums {
                    if let Some(u) = &al.cover_url {
                        if seen_urls.insert(u.clone()) {
                            thumb_jobs.push((format!("url:{u}"), u.clone()));
                        }
                    }
                }
                spawn_thumb_fetcher(thumb_jobs, tx.clone());
                (prof, avatar_path)
            });
            let _ = tx.send(WorkerMsg::ArtistLoaded(res));
        });
    }

    fn set_notice(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_at = Instant::now();
    }

    fn recent_downloaded_tracks(&self, limit: usize) -> Vec<(String, String, String, u64)> {
        let mut items: Vec<(u64, String, String, String, u64)> = self
            .lib
            .entries
            .values()
            .map(|e| {
                let mtime = fs::metadata(self.dir.join(&e.filename))
                    .and_then(|m| m.modified())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).map_err(|_| io::Error::other("ts")))
                    .map(|d| d.as_secs())
                    .unwrap_or(e.last_used);
                (
                    mtime,
                    e.filename.clone(),
                    e.title.clone(),
                    clean_artist_name(&e.artist),
                    e.size_bytes,
                )
            })
            .collect();
        items.sort_by(|a, b| b.0.cmp(&a.0));
        items
            .into_iter()
            .take(limit)
            .map(|(_, f, t, a, sz)| (f, t, a, sz))
            .collect()
    }

    fn delete_track_by_filename(&mut self, fname: &str) {
        let title = self
            .lib
            .entries
            .get(fname)
            .map(|e| e.title.clone())
            .unwrap_or_else(|| fname.to_string());
        if self.player.current_file.as_deref() == Some(fname) {
            self.player.stop();
            self.was_playing_file = false;
        }
        remove_track_files(&self.dir, fname);
        self.lib.entries.remove(fname);
        for pl in &mut self.lib.playlists {
            pl.tracks.retain(|x| x != fname);
        }
        self.queue.retain(|x| x != fname);
        self.upcoming.retain(|x| x != fname);
        self.play_context.retain(|x| x != fname);
        save_library(&self.dir, &self.lib);
        self.rebuild_track_list();
        self.set_notice(format!("Deleted '{title}'"));
    }

    fn open_genius_for_lyric(&mut self, lrc_idx: usize) {
        let Some(line) = self.lrc_lines.get(lrc_idx).cloned() else {
            return;
        };
        let Some(cur) = self.current_track_entry().cloned() else {
            return;
        };
        let song_key = format!("{}|{}", cur.artist, cur.title);
        if let Some(cached) = self.genius_cache.get(&song_key).cloned() {
            self.show_genius_match(&cached, &line.text);
            return;
        }
        self.genius_popup = Some(GeniusPopup {
            song_title: format!("{} - {}", cur.title, clean_artist_name(&cur.artist)),
            lyric_line: line.text.clone(),
            fragment: String::new(),
            explanation: "Loading Genius explanation...".into(),
            url: String::new(),
            loading: true,
        });
        let title = cur.title.clone();
        let artist = cur.artist.clone();
        let lyric_text = line.text.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = fetch_song_annotations(&title, &artist);
            let _ = tx.send(WorkerMsg::GeniusLoaded(song_key, lyric_text, res));
        });
    }

    fn show_genius_match(&mut self, song: &GeniusSongAnnotations, lyric_line: &str) {
        if let Some(ann) = match_lyric_annotation(song, lyric_line) {
            self.genius_popup = Some(GeniusPopup {
                song_title: song.song_title.clone(),
                lyric_line: lyric_line.to_string(),
                fragment: ann.fragment.clone(),
                explanation: ann.explanation.clone(),
                url: song.song_url.clone(),
                loading: false,
            });
        } else {
            self.genius_popup = Some(GeniusPopup {
                song_title: song.song_title.clone(),
                lyric_line: lyric_line.to_string(),
                fragment: String::new(),
                explanation: format!(
                    "No Genius annotation found for this specific line ({} annotated bars on this song).",
                    song.items.len()
                ),
                url: song.song_url.clone(),
                loading: false,
            });
        }
    }

    fn filtered_artist_albums(&self) -> Vec<AlbumSummary> {
        let Some(prof) = &self.artist_profile else {
            return Vec::new();
        };
        prof.albums
            .iter()
            .filter(|a| match self.artist_tab {
                ArtistTab::TopTracks => false,
                ArtistTab::All => true,
                ArtistTab::Albums => a.record_type == "album" || a.record_type == "compile",
                ArtistTab::Eps => a.record_type == "ep",
                ArtistTab::Singles => a.record_type == "single",
            })
            .cloned()
            .collect()
    }

    fn filtered_search_albums(&self) -> Vec<AlbumSummary> {
        self.search_albums_list
            .iter()
            .filter(|a| match self.search_tab {
                SearchTab::Songs => false,
                SearchTab::AllReleases => true,
                SearchTab::Albums => a.record_type == "album" || a.record_type == "compile",
                SearchTab::Eps => a.record_type == "ep",
                SearchTab::Singles => a.record_type == "single",
            })
            .cloned()
            .collect()
    }

    fn open_album_by_id(&mut self, album_id: u64, title: &str) {
        if !matches!(self.view, View::Album) {
            self.album_return_view = self.view.clone();
        }
        self.view = View::Album;
        self.focus = Focus::Main;
        self.input_mode = InputMode::Normal;
        self.album_loading_name = Some(title.to_string());
        self.album_detail = None;
        self.album_cover_path = None;
        self.album_cover_art = None;
        self.album_track_state.select(None);
        self.busy = true;
        self.set_notice(format!("Loading '{title}'..."));

        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = fetch_album_detail(album_id).map(|det| {
                let cover_path = det
                    .cover_url
                    .as_deref()
                    .and_then(|u| fetch_remote_image_to_temp(u, &format!("album-{}", det.id)));
                if let Some(u) = &det.cover_url {
                    spawn_thumb_fetcher(vec![(format!("url:{u}"), u.clone())], tx.clone());
                }
                (det, cover_path)
            });
            let _ = tx.send(WorkerMsg::AlbumLoaded(res));
        });
    }

    fn open_album_by_name(&mut self, artist: &str, album: &str) {
        if !matches!(self.view, View::Album) {
            self.album_return_view = self.view.clone();
        }
        self.view = View::Album;
        self.focus = Focus::Main;
        self.input_mode = InputMode::Normal;
        self.album_loading_name = Some(album.to_string());
        self.album_detail = None;
        self.album_cover_path = None;
        self.album_cover_art = None;
        self.album_track_state.select(None);
        self.busy = true;
        self.set_notice(format!("Loading album '{album}'..."));

        let ar = artist.to_string();
        let al = album.to_string();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = find_and_fetch_album_by_name(&ar, &al).map(|det| {
                let cover_path = det
                    .cover_url
                    .as_deref()
                    .and_then(|u| fetch_remote_image_to_temp(u, &format!("album-{}", det.id)));
                if let Some(u) = &det.cover_url {
                    spawn_thumb_fetcher(vec![(format!("url:{u}"), u.clone())], tx.clone());
                }
                (det, cover_path)
            });
            let _ = tx.send(WorkerMsg::AlbumLoaded(res));
        });
    }

    fn open_selected_or_playing_artist(&mut self) {
        if matches!(self.view, View::Album) {
            if let Some(det) = self.album_detail.clone() {
                if !det.artist.trim().is_empty() {
                    self.open_artist_profile(&det.artist, None);
                    return;
                }
            }
        }
        if matches!(self.input_mode, InputMode::SearchResults) || matches!(self.view, View::Search) {
            if self.search_tab == SearchTab::Songs {
                if let Some(i) = self.search_state.selected() {
                    if let Some(item) = self.search_results.get(i).cloned() {
                        self.open_artist_profile(&item.uploader, None);
                        return;
                    }
                }
            } else {
                let albums = self.filtered_search_albums();
                if let Some(i) = self.search_album_state.selected() {
                    if let Some(al) = albums.get(i) {
                        if !al.artist.trim().is_empty() {
                            self.open_artist_profile(&al.artist, None);
                            return;
                        }
                    }
                }
            }
        }
        if matches!(self.view, View::Settings) && self.spotify_open_col.is_some() {
            if let Some(i) = self.spotify_track_state.selected() {
                if let Some(tr) = self.spotify_tracks.get(i).cloned() {
                    self.open_artist_profile(&tr.artist, None);
                    return;
                }
            }
        }
        if let Some(fname) = self.selected_track() {
            if let Some(e) = self.lib.entries.get(&fname) {
                if !e.artist.trim().is_empty() {
                    let a = e.artist.clone();
                    self.open_artist_profile(&a, None);
                    return;
                }
            }
        }
        if let Some(e) = self.current_track_entry() {
            if !e.artist.trim().is_empty() {
                let a = e.artist.clone();
                self.open_artist_profile(&a, None);
                return;
            }
        }
        self.status = "No artist name available for this item".into();
    }

    fn refresh_lyrics_for(&mut self, fname: &str) {
        if self.lrc_file.as_deref() == Some(fname) {
            return;
        }
        self.lrc_lines = load_lrc(&self.dir, fname, None);
        self.lrc_file = Some(fname.to_string());
        self.lyrics_scroll_offset = 0;

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

    fn adjust_cache_limit(&mut self, delta_mb: i64) {
        let cur_mb = (self.max_bytes / 1_048_576) as i64;
        let next_mb = (cur_mb + delta_mb).clamp(100, 10_000) as u64;
        self.max_bytes = next_mb * 1_048_576;
        self.lib.max_cache_mb = Some(next_mb);
        save_library(&self.dir, &self.lib);
        self.status = format!("Cache limit set to {} MB (saved)", next_mb);
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

    fn ensure_artist_avatar(&mut self, cols: u16, rows: u16) {
        if cols < 4 || rows < 2 {
            return;
        }
        let Some(path) = self.artist_avatar_path.clone() else {
            return;
        };
        let key = path.to_string_lossy().to_string();
        if let Some(existing) = &self.artist_avatar_art {
            if existing.path_key == key && self.artist_avatar_size == (cols, rows) {
                return;
            }
        }
        self.artist_avatar_art = CoverArt::load(&path, cols, rows);
        self.artist_avatar_size = (cols, rows);
    }

    fn ensure_album_cover(&mut self, cols: u16, rows: u16) {
        if cols < 4 || rows < 2 {
            return;
        }
        let Some(path) = self.album_cover_path.clone() else {
            return;
        };
        let key = path.to_string_lossy().to_string();
        if let Some(existing) = &self.album_cover_art {
            if existing.path_key == key && self.album_cover_size == (cols, rows) {
                return;
            }
        }
        self.album_cover_art = CoverArt::load(&path, cols, rows);
        self.album_cover_size = (cols, rows);
    }

    fn scroll_lyrics(&mut self, delta: isize) {
        if self.lrc_lines.is_empty() {
            self.lyrics_scroll_offset = 0;
            return;
        }
        let off = self
            .current_track_entry()
            .map(|e| e.lrc_offset_ms as f64 / 1000.0)
            .unwrap_or(0.0);
        let pos = self.smooth_pos + off;
        let active_idx = self
            .lrc_lines
            .iter()
            .rposition(|l| l.time_sec <= pos)
            .unwrap_or(0) as isize;
        let max_idx = self.lrc_lines.len().saturating_sub(1) as isize;
        let min_off = -active_idx;
        let max_off = max_idx - active_idx;
        self.lyrics_scroll_offset = (self.lyrics_scroll_offset + delta).clamp(min_off, max_off);
    }

    fn play_next(&mut self) {
        if let Some(q) = self.queue.pop_front() {
            self.play_filename_internal(&q, true, false);
            if matches!(self.view, View::Queue) {
                self.rebuild_track_list();
            }
            return;
        }
        if self.repeat == RepeatMode::One {
            if let Some(cur) = self.player.current_file.clone() {
                self.play_filename_internal(&cur, false, false);
                return;
            }
        }
        if self.upcoming.is_empty() && self.repeat == RepeatMode::All {
            let cur = self.player.current_file.clone();
            self.rebuild_upcoming(cur.as_deref());
        }
        if let Some(next) = self.upcoming.pop_front() {
            self.play_filename_internal(&next, true, false);
            if self.upcoming.is_empty() && self.repeat == RepeatMode::All {
                self.rebuild_upcoming(Some(&next));
            }
            if matches!(self.view, View::Queue) {
                self.rebuild_track_list();
            }
        } else {
            self.was_playing_file = false;
        }
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

        if self.last_pb_poll.elapsed() < Duration::from_millis(50) {
            return;
        }
        self.last_pb_poll = now;
        self.pb = self.player.poll_state();
        if !self.pb.active || self.pb.paused || (self.smooth_pos - self.pb.pos).abs() > 0.06 {
            self.smooth_pos = self.pb.pos;
        } else {
            self.smooth_pos = self.smooth_pos * 0.50 + self.pb.pos * 0.50;
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
                WorkerMsg::ThumbReady(key, rows) => {
                    self.thumbs.insert(key, rows);
                }
                WorkerMsg::SearchDone(Ok(list)) => {
                    self.busy = false;
                    if list.is_empty() && self.search_albums_list.is_empty() {
                        self.set_notice("No results found. Click Search or press / to try again.");
                        self.input_mode = InputMode::Normal;
                    } else {
                        let jobs: Vec<(String, String)> = list
                            .iter()
                            .map(|r| (format!("yt:{}", r.id), r.thumbnail.clone()))
                            .collect();
                        spawn_thumb_fetcher(jobs, self.tx.clone());
                        self.search_results = list;
                        if !self.search_results.is_empty() {
                            self.search_state.select(Some(0));
                        } else if !self.search_albums_list.is_empty() {
                            self.search_tab = SearchTab::AllReleases;
                        }
                        self.view = View::Search;
                        self.focus = Focus::Main;
                        self.input_mode = InputMode::SearchResults;
                        self.set_notice(format!(
                            "{} songs, {} releases for '{}'",
                            self.search_results.len(),
                            self.search_albums_list.len(),
                            self.last_search_query
                        ));
                    }
                }
                WorkerMsg::SearchDone(Err(e)) => {
                    self.busy = false;
                    self.set_notice(format!("Search failed: {e}"));
                    if self.search_albums_list.is_empty() {
                        self.input_mode = InputMode::Normal;
                    }
                }
                WorkerMsg::SearchAlbumsDone(Ok(list)) => {
                    let jobs: Vec<(String, String)> = list
                        .iter()
                        .filter_map(|a| {
                            let u = a.cover_url.as_ref()?;
                            Some((format!("url:{u}"), u.clone()))
                        })
                        .collect();
                    spawn_thumb_fetcher(jobs, self.tx.clone());
                    self.search_albums_list = list;
                    self.search_album_state
                        .select(if self.search_albums_list.is_empty() { None } else { Some(0) });
                    if !self.search_albums_list.is_empty() && matches!(self.view, View::Search) {
                        if !self.busy && self.search_results.is_empty() {
                            self.search_tab = SearchTab::AllReleases;
                            self.input_mode = InputMode::SearchResults;
                        }
                    }
                }
                WorkerMsg::SearchAlbumsDone(Err(_)) => {}
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
                    }
                }
                WorkerMsg::SpotifyLoginDone(Ok(sess)) => {
                    self.busy = false;
                    let who = sess.display_name.clone();
                    self.spotify = sess;
                    self.spotify_open_col = None;
                    self.spotify_tracks.clear();
                    self.status = format!("Logged in to Spotify as {who} -- loading library...");
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
                    self.status = format!("Loaded {n} Spotify collections in Settings");
                }
                WorkerMsg::SpotifyCollectionsDone(Err(e)) => {
                    self.busy = false;
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
                    self.status = format!("Spotify: {name} ({n} tracks)");
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
                        subtitle: format!("Spotify {}", link.kind),
                        track_count: tracks.len(),
                        cover_url: link.cover_url.clone(),
                        kind: SpotifyCollectionKind::PublicUrl { url: link.url },
                    };
                    let n = tracks.len();
                    self.spotify_open_col = Some(col);
                    self.spotify_tracks = tracks;
                    self.spotify_track_state.select(if n > 0 { Some(0) } else { None });
                    self.status = format!("Linked '{}' ({n} tracks)", link.name);
                }
                WorkerMsg::SpotifyPublicUrlAdded(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Spotify link error: {e}");
                }
                WorkerMsg::ArtistLoaded(Ok((prof, avatar_path))) => {
                    self.busy = false;
                    self.artist_loading_name = None;
                    let n = prof.top_tracks.len();
                    let al_n = prof.albums.len();
                    let name = prof.name.clone();
                    self.artist_track_state.select(if n > 0 { Some(0) } else { None });
                    self.artist_album_state.select(if al_n > 0 { Some(0) } else { None });
                    self.artist_profile = Some(prof);
                    self.artist_avatar_path = avatar_path;
                    self.artist_avatar_art = None;
                    self.set_notice(format!("Artist: {name} ({n} top tracks, {al_n} releases)"));
                }
                WorkerMsg::ArtistLoaded(Err(e)) => {
                    self.busy = false;
                    self.artist_loading_name = None;
                    self.set_notice(format!("Artist lookup failed: {e}"));
                }
                WorkerMsg::AlbumLoaded(Ok((det, cover_path))) => {
                    self.busy = false;
                    self.album_loading_name = None;
                    let n = det.tracks.len();
                    let title = det.title.clone();
                    self.album_track_state.select(if n > 0 { Some(0) } else { None });
                    self.album_detail = Some(det);
                    self.album_cover_path = cover_path;
                    self.album_cover_art = None;
                    self.set_notice(format!("Album: {title} ({n} tracks)"));
                }
                WorkerMsg::AlbumLoaded(Err(e)) => {
                    self.busy = false;
                    self.album_loading_name = None;
                    self.set_notice(format!("Album lookup failed: {e}"));
                }
                WorkerMsg::GeniusLoaded(song_key, lyric_line, Ok(song)) => {
                    self.genius_cache.insert(song_key, song.clone());
                    if self.genius_popup.as_ref().map(|p| p.loading).unwrap_or(false) {
                        self.show_genius_match(&song, &lyric_line);
                    }
                }
                WorkerMsg::GeniusLoaded(_, lyric_line, Err(e)) => {
                    if let Some(p) = self.genius_popup.as_mut() {
                        p.loading = false;
                        p.lyric_line = lyric_line;
                        p.explanation = e;
                    }
                }
            }
        }
    }

    fn open_settings_view(&mut self) {
        self.view = View::Settings;
        self.focus = Focus::Main;
        if matches!(self.input_mode, InputMode::Search | InputMode::SearchResults) {
            self.input_mode = InputMode::Normal;
        }
        if self.spotify.is_logged_in() && self.spotify_cols.len() <= self.spotify.saved_links.len() && !self.busy {
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
        self.status = "Loading Spotify playlists & library...".into();
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
        if parse_spotify_artist_id(&input).is_some() {
            self.open_artist_profile(&input, None);
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
                self.view = View::Settings;
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
        self.view = View::Settings;
        self.status = "Linking Spotify playlist/album (no Premium needed)...".into();
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
        let Some(tr) = self.spotify_tracks.get(idx).cloned() else {
            return;
        };
        if auto_play {
            if let Some(cached) = find_cached_track(&self.lib, &tr.title, &tr.artist) {
                self.play_filename(&cached, true);
                return;
            }
        }
        if self.busy {
            return;
        }
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
        self.spawn_spec_download(tr, fallback_cover, pl_name, is_liked_col, auto_play);
    }

    fn start_artist_download_track(&mut self, idx: usize, auto_play: bool) {
        let Some(prof) = self.artist_profile.clone() else {
            return;
        };
        let Some(tr) = prof.top_tracks.get(idx).cloned() else {
            return;
        };
        if auto_play {
            if let Some(cached) = find_cached_track(&self.lib, &tr.title, &tr.artist) {
                self.play_filename(&cached, true);
                return;
            }
        }
        if self.busy {
            return;
        }
        self.spawn_spec_download(tr, prof.image_url, None, false, auto_play);
    }

    fn start_album_download_track(&mut self, idx: usize, auto_play: bool) {
        let Some(det) = self.album_detail.clone() else {
            return;
        };
        let Some(tr) = det.tracks.get(idx).cloned() else {
            return;
        };
        if auto_play {
            if let Some(cached) = find_cached_track(&self.lib, &tr.title, &tr.artist) {
                self.play_filename(&cached, true);
                return;
            }
        }
        if self.busy {
            return;
        }
        self.spawn_spec_download(tr, det.cover_url, None, false, auto_play);
    }

    fn start_album_import_all(&mut self) {
        if self.busy {
            return;
        }
        let Some(det) = self.album_detail.clone() else {
            return;
        };
        if det.tracks.is_empty() {
            return;
        }
        let label = det.title.clone();
        self.spawn_bulk_import(
            det.tracks,
            det.cover_url,
            Some(label.clone()),
            false,
            label,
        );
    }

    fn spawn_spec_download(
        &mut self,
        tr: SpotifyTrackSpec,
        fallback_cover: Option<String>,
        pl_name: Option<String>,
        mark_liked: bool,
        auto_play: bool,
    ) {
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
            if mark_liked {
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
        self.spawn_bulk_import(tracks, fallback_cover, pl_name, is_liked_col, col_label);
    }

    fn start_artist_import_all(&mut self) {
        if self.busy {
            return;
        }
        let Some(prof) = self.artist_profile.clone() else {
            return;
        };
        if prof.top_tracks.is_empty() {
            return;
        }
        let label = format!("{} Top Tracks", prof.name);
        self.spawn_bulk_import(
            prof.top_tracks,
            prof.image_url,
            Some(label.clone()),
            false,
            label,
        );
    }

    fn spawn_bulk_import(
        &mut self,
        tracks: Vec<SpotifyTrackSpec>,
        fallback_cover: Option<String>,
        pl_name: Option<String>,
        mark_liked: bool,
        col_label: String,
    ) {
        self.busy = true;
        self.status = format!("Importing {} tracks from '{}'...", tracks.len(), col_label);
        let dir = self.dir.clone();
        let max_bytes = self.max_bytes;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let total = tracks.len();
            let mut ok_count = 0usize;
            for (idx, tr) in tracks.iter().enumerate() {
                let _ = tx.send(WorkerMsg::Progress(format!(
                    "Import [{}/{}] Downloading {} - {}...",
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
                    if mark_liked {
                        let mut lib = load_and_sync_library(&dir);
                        if let Some(e) = lib.entries.get_mut(&fname) {
                            e.liked = true;
                            save_library(&dir, &lib);
                        }
                    }
                    ok_count += 1;
                }
            }
            let summary = if mark_liked {
                format!("Imported {ok_count}/{total} tracks into * Liked")
            } else {
                format!("Imported {ok_count}/{total} tracks into playlist '{col_label}'")
            };
            let _ = tx.send(WorkerMsg::ImportDone(Ok(summary)));
        });
    }

    fn logout_spotify(&mut self) {
        let kept_links = self.spotify.saved_links.clone();
        clear_spotify_session();
        self.spotify = SpotifySession {
            saved_links: kept_links,
            ..Default::default()
        };
        if !self.spotify.saved_links.is_empty() {
            let _ = save_spotify_session(&self.spotify);
        }
        self.spotify_cols = saved_links_as_collections(&self.spotify);
        self.spotify_open_col = None;
        self.spotify_tracks.clear();
        self.status = "Logged out of Spotify OAuth (saved public links kept).".into();
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
        if parse_spotify_artist_id(&q).is_some() {
            self.open_artist_profile(&q, None);
            return;
        }
        if q.contains("open.spotify.com/") || q.starts_with("spotify:") || expand_local_path(&q).exists() {
            self.start_smart_import(q);
            return;
        }
        self.last_search_query = q.clone();
        self.search_results.clear();
        self.search_albums_list.clear();
        self.search_album_state.select(None);
        self.search_tab = SearchTab::Songs;
        self.view = View::Search;
        self.focus = Focus::Main;
        self.busy = true;
        self.input_mode = InputMode::Normal;
        self.set_notice(format!("Searching for '{q}'..."));
        let tx_yt = self.tx.clone();
        let q_yt = q.clone();
        thread::spawn(move || {
            let _ = tx_yt.send(WorkerMsg::SearchDone(search_youtube(&q_yt)));
        });
        let tx_al = self.tx.clone();
        thread::spawn(move || {
            let _ = tx_al.send(WorkerMsg::SearchAlbumsDone(search_albums(&q)));
        });
    }

    fn start_download_search_result(&mut self, auto_play: bool) {
        let Some(i) = self.search_state.selected() else {
            return;
        };
        let Some(item) = self.search_results.get(i).cloned() else {
            return;
        };
        if auto_play {
            if let Some(cached) = find_cached_track(&self.lib, &item.title, &item.uploader) {
                self.play_filename(&cached, true);
                return;
            }
        }
        if self.busy {
            return;
        }
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
        if parse_spotify_artist_id(&input).is_some() {
            self.open_artist_profile(&input, None);
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
            ButtonAction::Home => {
                self.view = View::AllTracks;
                self.sidebar_state.select(Some(0));
                self.input_mode = InputMode::Normal;
                self.focus = Focus::Main;
                self.rebuild_track_list();
            }
            ButtonAction::SearchBtn => {
                if matches!(self.input_mode, InputMode::Search) && !self.input_buf.trim().is_empty() {
                    self.start_youtube_search();
                } else if !self.busy {
                    self.input_buf.clear();
                    self.view = View::Search;
                    self.focus = Focus::Main;
                    self.input_mode = InputMode::Search;
                    self.status = "Type song name, artist, or Spotify/YouTube URL -> press Enter or click [ Search ]".into();
                }
            }
            ButtonAction::Settings => self.open_settings_view(),
            ButtonAction::Prev => self.play_prev(),
            ButtonAction::PlayPause => self.toggle_play_or_start(),
            ButtonAction::Next => self.play_next(),
            ButtonAction::Shuffle => {
                self.shuffle = !self.shuffle;
                let cur = self.player.current_file.clone();
                self.rebuild_upcoming(cur.as_deref());
                self.set_notice(format!("Shuffle: {}", if self.shuffle { "on" } else { "off" }));
            }
            ButtonAction::Repeat => {
                self.repeat = self.repeat.next();
                let cur = self.player.current_file.clone();
                self.rebuild_upcoming(cur.as_deref());
                self.set_notice(format!("Repeat: {}", self.repeat.label()));
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
            ButtonAction::ToggleLyrics => {
                self.show_lyrics_pane = !self.show_lyrics_pane;
                self.set_notice(format!(
                    "Live lyrics: {}",
                    if self.show_lyrics_pane { "on" } else { "off" }
                ));
            }
            ButtonAction::LyricsReturn => {
                self.lyrics_scroll_offset = 0;
            }
            ButtonAction::OpenQueue => {
                self.view = View::Queue;
                self.sidebar_state.select(Some(2));
                self.focus = Focus::Main;
                self.input_mode = InputMode::Normal;
                self.rebuild_track_list();
            }
            ButtonAction::CacheDec => self.adjust_cache_limit(-100),
            ButtonAction::CacheInc => self.adjust_cache_limit(100),
            ButtonAction::PruneCache => {
                let (n, freed) = prune_lru(&self.dir, &mut self.lib, self.max_bytes);
                self.rebuild_track_list();
                self.status = if n > 0 {
                    format!("Pruned {n} tracks (freed {})", fmt_mb(freed))
                } else {
                    format!(
                        "Cache under {} ({})",
                        fmt_mb(self.max_bytes),
                        fmt_mb(total_cache_bytes(&self.lib))
                    )
                };
            }
            ButtonAction::SpotifyAddUrl => {
                if !self.busy {
                    self.input_buf.clear();
                    self.input_mode = InputMode::SpotifyAddUrl;
                }
            }
            ButtonAction::SpotifyLoginPrompt => {
                if !self.busy {
                    self.input_buf = self.spotify.client_id.clone();
                    self.input_mode = InputMode::SpotifyClientId;
                }
            }
            ButtonAction::SpotifyDashboard => {
                open_url_in_browser("https://developer.spotify.com/dashboard");
                self.status = format!(
                    "Opened Spotify Dashboard -- set Redirect URI to {SPOTIFY_REDIRECT_URI}, then click OAuth Login"
                );
            }
            ButtonAction::SpotifyRefresh => {
                self.spotify_open_col = None;
                self.spotify_tracks.clear();
                self.start_spotify_load_collections();
            }
            ButtonAction::SpotifyLogout => self.logout_spotify(),
            ButtonAction::ToggleDownloads => {
                self.show_downloads_menu = !self.show_downloads_menu;
                if self.show_downloads_menu {
                    self.genius_popup = None;
                }
            }
            ButtonAction::ClosePopup => {
                self.genius_popup = None;
                self.show_downloads_menu = false;
            }
            ButtonAction::ToggleLikeArtist => self.toggle_like_current_artist(),
            ButtonAction::ArtistImportAll => self.start_artist_import_all(),
            ButtonAction::ArtistTab(tab) => {
                self.artist_tab = tab;
                if tab != ArtistTab::TopTracks {
                    let n = self.filtered_artist_albums().len();
                    self.artist_album_state.select(if n > 0 { Some(0) } else { None });
                }
            }
            ButtonAction::SearchTab(tab) => {
                self.search_tab = tab;
                if tab != SearchTab::Songs {
                    let n = self.filtered_search_albums().len();
                    self.search_album_state.select(if n > 0 { Some(0) } else { None });
                }
            }
            ButtonAction::AlbumImportAll => self.start_album_import_all(),
            ButtonAction::AlbumBack => {
                let target = self.album_return_view.clone();
                self.view = target;
                self.focus = Focus::Main;
                if matches!(self.view, View::Search)
                    && (!self.search_results.is_empty() || !self.search_albums_list.is_empty())
                {
                    self.input_mode = InputMode::SearchResults;
                } else {
                    self.input_mode = InputMode::Normal;
                    self.rebuild_track_list();
                }
            }
            ButtonAction::OpenAlbumArtist => {
                if let Some(det) = self.album_detail.clone() {
                    if !det.artist.trim().is_empty() {
                        self.open_artist_profile(&det.artist, None);
                    }
                }
            }
            ButtonAction::OpenCurrentArtist => self.open_selected_or_playing_artist(),
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        self.mouse_pos = (col, row);
        match kind {
            MouseEventKind::Moved => {}
            MouseEventKind::ScrollUp => {
                if rect_contains(self.hits.controls_bar, col, row) {
                    let v = self.player.volume + 5;
                    self.player.set_volume(v);
                } else if rect_contains(self.hits.lyrics_area, col, row) {
                    self.scroll_lyrics(-2);
                } else {
                    self.move_cursor(-1);
                }
            }
            MouseEventKind::ScrollDown => {
                if rect_contains(self.hits.controls_bar, col, row) {
                    let v = self.player.volume - 5;
                    self.player.set_volume(v);
                } else if rect_contains(self.hits.lyrics_area, col, row) {
                    self.scroll_lyrics(2);
                } else {
                    self.move_cursor(1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // 1. Floating Genius popup captures clicks first
                if self.genius_popup.is_some() {
                    let buttons = self.hits.buttons.clone();
                    for (r, act) in buttons {
                        if matches!(act, ButtonAction::ClosePopup) && rect_contains(r, col, row) {
                            self.genius_popup = None;
                            return;
                        }
                    }
                    if !rect_contains(self.hits.popup_rect, col, row) {
                        self.genius_popup = None;
                    }
                    return;
                }

                // 2. Floating Downloads dropdown captures clicks before underlying UI
                if self.show_downloads_menu {
                    let del_btns = self.hits.dl_del_btns.clone();
                    for (r, fname) in del_btns {
                        if rect_contains(r, col, row) {
                            self.delete_track_by_filename(&fname);
                            return;
                        }
                    }
                    let play_rows = self.hits.dl_play_rows.clone();
                    for (r, fname) in play_rows {
                        if rect_contains(r, col, row) {
                            self.play_filename(&fname, true);
                            self.show_downloads_menu = false;
                            return;
                        }
                    }
                    if rect_contains(self.hits.dl_btn, col, row) {
                        self.show_downloads_menu = false;
                        return;
                    }
                    if !rect_contains(self.hits.dl_dropdown, col, row) {
                        self.show_downloads_menu = false;
                        // fall through so clicking outside also interacts cleanly
                    } else {
                        return;
                    }
                }

                // 3. Regular buttons (Top bar, Controls, Artist/Settings actions)
                let buttons = self.hits.buttons.clone();
                for (r, act) in buttons {
                    if rect_contains(r, col, row) {
                        self.trigger_button(act);
                        return;
                    }
                }

                // 4. Lyric [?] Genius explanation buttons
                let lrc_help = self.hits.lyric_help_btns.clone();
                for (r, lrc_idx) in lrc_help {
                    if rect_contains(r, col, row) {
                        self.open_genius_for_lyric(lrc_idx);
                        return;
                    }
                }

                // 5. Click anywhere on a lyric line -> seek straight to that timestamp & snap back!
                if rect_contains(self.hits.lyrics_area, col, row) {
                    let lrows = self.hits.lyrics_rows.clone();
                    if let Some(&(_, _, target_sec)) = lrows.iter().find(|&&(y, _, _)| y == row) {
                        if self.pb.active {
                            self.player.seek_abs(target_sec);
                            self.smooth_pos = target_sec;
                            self.lyrics_scroll_offset = 0;
                            self.set_notice(format!("Seeked to {}", fmt_time(target_sec)));
                        }
                        return;
                    }
                }

                // 6. Click underlined Artist or Album links anywhere in Song Browser / Search / Artist!
                let artist_links = self.hits.artist_links.clone();
                for (r, artist_name) in artist_links {
                    if rect_contains(r, col, row) {
                        self.open_artist_profile(&artist_name, None);
                        return;
                    }
                }
                let album_links = self.hits.album_links.clone();
                for (r, artist_name, album_name) in album_links {
                    if rect_contains(r, col, row) {
                        self.open_album_by_name(&artist_name, &album_name);
                        return;
                    }
                }

                // Click top Search box anytime
                if rect_contains(self.hits.search_box, col, row) {
                    if !self.busy {
                        self.view = View::Search;
                        self.focus = Focus::Main;
                        if !matches!(self.input_mode, InputMode::Search) {
                            self.input_buf.clear();
                            self.input_mode = InputMode::Search;
                        }
                    }
                    return;
                }

                // Click progress bar to seek
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

                // Click sidebar
                let sb = self.hits.sidebar;
                if matches!(
                    self.input_mode,
                    InputMode::Normal | InputMode::SearchResults | InputMode::Search
                ) && rect_contains(sb, col, row)
                {
                    let rel_y = row.saturating_sub(sb.y + 1) as usize;
                    let idx = self.sidebar_state.offset() + rel_y;
                    let rows = self.sidebar_rows();
                    if let Some((_, entry)) = rows.get(idx) {
                        if !matches!(entry, SidebarEntry::Header) {
                            self.focus = Focus::Sidebar;
                            self.sidebar_state.select(Some(idx));
                            self.activate_sidebar_index(idx);
                        }
                    }
                    return;
                }

                // Click Settings Spotify collections / tracks list
                if matches!(self.view, View::Settings) && rect_contains(self.hits.settings_list, col, row) {
                    let sl = self.hits.settings_list;
                    let rel_y = row.saturating_sub(sl.y + 1) as usize;
                    self.focus = Focus::Main;
                    if self.spotify_open_col.is_some() {
                        let idx = self.spotify_track_state.offset() + rel_y;
                        if idx < self.spotify_tracks.len() {
                            self.spotify_track_state.select(Some(idx));
                            self.start_spotify_download_track(idx, true);
                        }
                    } else {
                        let idx = self.spotify_col_state.offset() + rel_y;
                        if idx < self.spotify_cols.len() {
                            self.spotify_col_state.select(Some(idx));
                            self.start_spotify_open_collection(idx);
                        }
                    }
                    return;
                }

                // Click Main Track / Search / Artist Track list
                let tl = self.hits.track_list;
                if rect_contains(tl, col, row) {
                    let rel_y = row.saturating_sub(tl.y + 1) as usize;
                    let rel_x = col.saturating_sub(tl.x + 1);
                    match &self.input_mode {
                        InputMode::SearchResults => {
                            if self.search_tab == SearchTab::Songs {
                                // Search rows are 2 lines tall with 4x2 thumbnail
                                let idx = self.search_state.offset() + (rel_y / 2);
                                let on_line2 = (rel_y % 2) == 1;
                                if idx < self.search_results.len() {
                                    self.search_state.select(Some(idx));
                                    if on_line2 && rel_x >= 6 {
                                        // Clicked the artist line -> load Artist Profile from Spotify!
                                        let artist = self.search_results[idx].uploader.clone();
                                        self.open_artist_profile(&artist, None);
                                    } else {
                                        self.start_download_search_result(true);
                                    }
                                }
                            } else {
                                let albums = self.filtered_search_albums();
                                let idx = self.search_album_state.offset() + (rel_y / 2);
                                if idx < albums.len() {
                                    self.search_album_state.select(Some(idx));
                                    self.open_album_by_id(albums[idx].id, &albums[idx].title);
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
                            if matches!(self.view, View::Artist) {
                                self.focus = Focus::Main;
                                if self.artist_tab == ArtistTab::TopTracks {
                                    // Artist top tracks are 2 lines tall with 4x2 album thumbnail
                                    let idx = self.artist_track_state.offset() + (rel_y / 2);
                                    let total = self
                                        .artist_profile
                                        .as_ref()
                                        .map(|p| p.top_tracks.len())
                                        .unwrap_or(0);
                                    if idx < total {
                                        self.artist_track_state.select(Some(idx));
                                        self.start_artist_download_track(idx, true);
                                    }
                                } else {
                                    let albums = self.filtered_artist_albums();
                                    let idx = self.artist_album_state.offset() + (rel_y / 2);
                                    if idx < albums.len() {
                                        self.artist_album_state.select(Some(idx));
                                        self.open_album_by_id(albums[idx].id, &albums[idx].title);
                                    }
                                }
                                return;
                            }
                            if matches!(self.view, View::Album) {
                                self.focus = Focus::Main;
                                let idx = self.album_track_state.offset() + (rel_y / 2);
                                let total = self
                                    .album_detail
                                    .as_ref()
                                    .map(|d| d.tracks.len())
                                    .unwrap_or(0);
                                if idx < total {
                                    self.album_track_state.select(Some(idx));
                                    self.start_album_download_track(idx, true);
                                }
                                return;
                            }
                            if matches!(self.view, View::Search)
                                && (!self.search_results.is_empty() || !self.search_albums_list.is_empty())
                            {
                                self.input_mode = InputMode::SearchResults;
                                if self.search_tab == SearchTab::Songs {
                                    let idx = self.search_state.offset() + (rel_y / 2);
                                    let on_line2 = (rel_y % 2) == 1;
                                    if idx < self.search_results.len() {
                                        self.search_state.select(Some(idx));
                                        if on_line2 && rel_x >= 6 {
                                            let artist = self.search_results[idx].uploader.clone();
                                            self.open_artist_profile(&artist, None);
                                        } else {
                                            self.start_download_search_result(true);
                                        }
                                    }
                                } else {
                                    let albums = self.filtered_search_albums();
                                    let idx = self.search_album_state.offset() + (rel_y / 2);
                                    if idx < albums.len() {
                                        self.search_album_state.select(Some(idx));
                                        self.open_album_by_id(albums[idx].id, &albums[idx].title);
                                    }
                                }
                                return;
                            }
                            // Library track rows: 2 lines tall with 4x2 thumbnail
                            let idx = self.track_state.offset() + (rel_y / 2);
                            let on_line2 = (rel_y % 2) == 1;
                            if idx < self.track_list.len() {
                                self.focus = Focus::Main;
                                let fname = self.track_list[idx].clone();
                                // Check if user clicked the Artist name on line 2!
                                if on_line2 {
                                    if let Some(e) = self.lib.entries.get(&fname) {
                                        let artist_clean = clean_artist_name(&e.artist);
                                        let artist_len = artist_clean.chars().count() as u16;
                                        // Line 2 layout: "| " (2) + thumb (4) + " " (1) + "   " (3) = col 10 start
                                        if artist_len > 0 && rel_x >= 9 && rel_x <= 10 + artist_len + 1 {
                                            self.track_state.select(Some(idx));
                                            self.open_artist_profile(&artist_clean, None);
                                            return;
                                        }
                                    }
                                }
                                let already_sel = self.track_state.selected() == Some(idx);
                                let quick_repeat = self
                                    .last_click
                                    .map(|(i, t)| i == idx && t.elapsed() < Duration::from_millis(450))
                                    .unwrap_or(false);
                                self.track_state.select(Some(idx));
                                self.last_click = Some((idx, Instant::now()));
                                if already_sel || quick_repeat {
                                    if matches!(self.view, View::Queue) {
                                        self.play_from_queue_index(idx);
                                    } else {
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
                if self.search_tab == SearchTab::Songs {
                    if self.search_results.is_empty() {
                        return;
                    }
                    let n = self.search_results.len() as isize;
                    let cur = self.search_state.selected().unwrap_or(0) as isize;
                    self.search_state
                        .select(Some((cur + delta).rem_euclid(n) as usize));
                } else {
                    let len = self.filtered_search_albums().len();
                    if len == 0 {
                        return;
                    }
                    let n = len as isize;
                    let cur = self.search_album_state.selected().unwrap_or(0) as isize;
                    self.search_album_state
                        .select(Some((cur + delta).rem_euclid(n) as usize));
                }
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
                    let rows = self.sidebar_rows();
                    if rows.is_empty() {
                        return;
                    }
                    let n = rows.len() as isize;
                    let mut idx = self.sidebar_state.selected().unwrap_or(0) as isize;
                    let step = if delta >= 0 { 1 } else { -1 };
                    for _ in 0..rows.len() {
                        idx = (idx + step).rem_euclid(n);
                        if !matches!(rows[idx as usize].1, SidebarEntry::Header) {
                            self.sidebar_state.select(Some(idx as usize));
                            if !matches!(rows[idx as usize].1, SidebarEntry::NewPlaylist | SidebarEntry::Artist(_)) {
                                self.activate_sidebar_index(idx as usize);
                            }
                            break;
                        }
                    }
                }
                Focus::Main => match self.view {
                    View::Artist => {
                        if self.artist_tab == ArtistTab::TopTracks {
                            let total = self
                                .artist_profile
                                .as_ref()
                                .map(|p| p.top_tracks.len())
                                .unwrap_or(0);
                            if total > 0 {
                                let n = total as isize;
                                let cur = self.artist_track_state.selected().unwrap_or(0) as isize;
                                self.artist_track_state
                                    .select(Some((cur + delta).rem_euclid(n) as usize));
                            }
                        } else {
                            let total = self.filtered_artist_albums().len();
                            if total > 0 {
                                let n = total as isize;
                                let cur = self.artist_album_state.selected().unwrap_or(0) as isize;
                                self.artist_album_state
                                    .select(Some((cur + delta).rem_euclid(n) as usize));
                            }
                        }
                    }
                    View::Album => {
                        let total = self
                            .album_detail
                            .as_ref()
                            .map(|d| d.tracks.len())
                            .unwrap_or(0);
                        if total > 0 {
                            let n = total as isize;
                            let cur = self.album_track_state.selected().unwrap_or(0) as isize;
                            self.album_track_state
                                .select(Some((cur + delta).rem_euclid(n) as usize));
                        }
                    }
                    View::Settings => {
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
                    }
                    View::Search => {
                        if self.search_tab == SearchTab::Songs {
                            if !self.search_results.is_empty() {
                                let n = self.search_results.len() as isize;
                                let cur = self.search_state.selected().unwrap_or(0) as isize;
                                self.search_state
                                    .select(Some((cur + delta).rem_euclid(n) as usize));
                            }
                        } else {
                            let len = self.filtered_search_albums().len();
                            if len > 0 {
                                let n = len as isize;
                                let cur = self.search_album_state.selected().unwrap_or(0) as isize;
                                self.search_album_state
                                    .select(Some((cur + delta).rem_euclid(n) as usize));
                            }
                        }
                    }
                    _ => {
                        if self.track_list.is_empty() {
                            return;
                        }
                        let n = self.track_list.len() as isize;
                        let cur = self.track_state.selected().unwrap_or(0) as isize;
                        self.track_state
                            .select(Some((cur + delta).rem_euclid(n) as usize));
                    }
                },
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

fn spawn_thumb_fetcher(jobs: Vec<(String, String)>, tx: Sender<WorkerMsg>) {
    if jobs.is_empty() {
        return;
    }
    // Split across up to 4 lightweight worker threads so search/artist thumbnails pop in almost instantly
    let worker_count = jobs.len().min(4);
    let mut buckets: Vec<Vec<(String, String)>> = (0..worker_count).map(|_| Vec::new()).collect();
    for (i, job) in jobs.into_iter().enumerate() {
        buckets[i % worker_count].push(job);
    }
    for bucket in buckets {
        let tx_c = tx.clone();
        thread::spawn(move || {
            for (key, url) in bucket {
                if let Some(path) = fetch_remote_image_to_temp(&url, &key) {
                    if let Some(rows) = decode_thumb(&path) {
                        let _ = tx_c.send(WorkerMsg::ThumbReady(key, rows));
                    }
                }
            }
        });
    }
}

fn rect_contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x.saturating_add(r.width) && row >= r.y && row < r.y.saturating_add(r.height)
}

fn fmt_fans(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M fans", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K fans", n as f64 / 1_000.0)
    } else {
        format!("{n} fans")
    }
}

fn render_inline_buttons(
    defs: Vec<(String, Style, Option<ButtonAction>)>,
    area: Rect,
    top_offset: u16,
    hits: &mut Vec<(Rect, ButtonAction)>,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut cursor_x = area.x + 1;
    let max_x = area.x + area.width.saturating_sub(1);
    let btn_y = area.y + top_offset;
    for (label, style, action_opt) in defs {
        let w = label.chars().count() as u16;
        if cursor_x + w > max_x {
            break;
        }
        if let Some(action) = action_opt {
            hits.push((Rect::new(cursor_x, btn_y, w, 1), action));
        }
        spans.push(Span::styled(label, style));
        cursor_x += w;
        if cursor_x + 1 < max_x {
            spans.push(Span::raw(" "));
            cursor_x += 1;
        }
    }
    Line::from(spans)
}

fn wrap_plain_text(text: &str, max_w: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        let wlen = w.chars().count();
        let clen = cur.chars().count();
        if !cur.is_empty() && clen + 1 + wlen > max_w {
            rows.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// Clean, vertically-centered truecolor word-by-word synced lyrics with manual scrolling,
/// hover highlight, click-to-seek timestamp mapping, and clickable `[?]` Genius triggers.
fn render_word_synced_lyrics(
    app: &App,
    lrc_area: Rect,
) -> (Vec<Line<'static>>, Vec<(u16, usize, f64)>, Vec<(Rect, usize)>) {
    let h = lrc_area.height.saturating_sub(2) as usize;
    let inner_w = lrc_area.width.saturating_sub(8).max(16) as usize;
    let (mx, my) = app.mouse_pos;
    let mouse_in_lyrics = rect_contains(lrc_area, mx, my);

    if app.lrc_lines.is_empty() {
        let pad = h / 2;
        let mut lines: Vec<Line<'static>> = (0..pad).map(|_| Line::from("")).collect();
        lines.push(Line::from(Span::styled(
            if app.player.current_file.is_some() {
                "No synced lyrics for this track"
            } else {
                "Play a track to see live lyrics"
            },
            Style::default().fg(Color::Rgb(84, 84, 92)),
        )));
        return (lines, Vec::new(), Vec::new());
    }

    let offset_sec = app
        .current_track_entry()
        .map(|e| e.lrc_offset_ms as f64 / 1000.0)
        .unwrap_or(0.0);
    let pos = app.smooth_pos + offset_sec;

    let active_opt = app.lrc_lines.iter().rposition(|l| l.time_sec <= pos);
    let active_idx = active_opt.unwrap_or(0);
    let has_started = active_opt.is_some();

    let max_idx = app.lrc_lines.len().saturating_sub(1);
    let view_idx = (active_idx as isize + app.lyrics_scroll_offset).clamp(0, max_idx as isize) as usize;

    // Helper to build the live word-by-word active bar rows + hairline
    let build_active_block = || -> (Vec<Vec<Span<'static>>>, Line<'static>) {
        let active_entry = &app.lrc_lines[active_idx];
        let mut active_rows: Vec<Vec<Span<'static>>> = Vec::new();
        let mut current_row: Vec<Span<'static>> = Vec::new();
        let mut row_len = 0usize;
        let mut bar_start = f64::MAX;
        let mut bar_end = 0.0f64;

        for (wi, w) in active_entry.words.iter().enumerate() {
            bar_start = bar_start.min(w.start);
            bar_end = bar_end.max(w.end);

            let w_chars: Vec<char> = w.text.chars().collect();
            let w_len = w_chars.len();
            let need_space = if current_row.is_empty() { 0 } else { 1 };
            if !current_row.is_empty() && row_len + need_space + w_len > inner_w {
                active_rows.push(std::mem::take(&mut current_row));
                row_len = 0;
            }
            if !current_row.is_empty() {
                current_row.push(Span::raw(" "));
                row_len += 1;
            }

            let dur = (w.end - w.start).max(0.07);
            let (active_fg, unlit_active_fg, sung_style, upcoming_fg) = if w.is_adlib {
                (
                    Color::Rgb(242, 202, 92),
                    Color::Rgb(138, 122, 82),
                    Style::default().fg(Color::Rgb(192, 166, 98)),
                    Color::Rgb(84, 84, 92),
                )
            } else {
                (
                    Color::Rgb(118, 238, 148),
                    Color::Rgb(132, 136, 144),
                    Style::default()
                        .fg(Color::Rgb(244, 244, 246))
                        .add_modifier(Modifier::BOLD),
                    Color::Rgb(96, 98, 106),
                )
            };

            if pos >= w.end {
                current_row.push(Span::styled(w.text.clone(), sung_style));
            } else if pos >= w.start {
                let frac = ((pos - w.start) / dur).clamp(0.0, 1.0);
                let split_at = ((w_len as f64) * frac).ceil().clamp(1.0, w_len as f64) as usize;
                let lit: String = w_chars[..split_at].iter().collect();
                let rest: String = w_chars[split_at..].iter().collect();
                current_row.push(Span::styled(
                    lit,
                    Style::default().fg(active_fg).add_modifier(Modifier::BOLD),
                ));
                if !rest.is_empty() {
                    current_row.push(Span::styled(rest, Style::default().fg(unlit_active_fg)));
                }
            } else {
                current_row.push(Span::styled(w.text.clone(), Style::default().fg(upcoming_fg)));
            }
            row_len += w_len;

            if wi + 1 == active_entry.words.len() && !current_row.is_empty() {
                active_rows.push(std::mem::take(&mut current_row));
            }
        }

        let bar_w = inner_w.min(26).max(8);
        let bar_frac = if bar_end > bar_start {
            ((pos - bar_start) / (bar_end - bar_start)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled = ((bar_w as f64) * bar_frac).round() as usize;
        let bar_line = Line::from(vec![
            Span::styled(
                "─".repeat(filled),
                Style::default().fg(Color::Rgb(118, 238, 148)),
            ),
            Span::styled(
                "─".repeat(bar_w.saturating_sub(filled)),
                Style::default().fg(Color::Rgb(46, 48, 54)),
            ),
        ]);
        (active_rows, bar_line)
    };

    let (active_rows_opt, bar_line_opt) = if has_started {
        let (ar, bl) = build_active_block();
        (Some(ar), Some(bl))
    } else {
        (None, None)
    };

    // Build centered viewport rows around `view_idx` (so scrolling smoothly moves through ALL lyrics!)
    enum RawRow {
        Blank,
        Plain {
            text: String,
            idx: usize,
            t: f64,
            color: Color,
        },
        Active {
            spans: Vec<Span<'static>>,
            idx: usize,
            t: f64,
            show_help: bool,
        },
        Bar(Line<'static>),
    }

    let make_line_rows = |idx: usize, is_center_focus: bool| -> Vec<RawRow> {
        if has_started && idx == active_idx {
            let mut v = Vec::new();
            v.push(RawRow::Blank);
            if let Some(ref ar) = active_rows_opt {
                let t = app.lrc_lines[idx].time_sec;
                for (ri, sp) in ar.iter().enumerate() {
                    v.push(RawRow::Active {
                        spans: sp.clone(),
                        idx,
                        t,
                        show_help: ri == 0,
                    });
                }
            }
            if let Some(ref bl) = bar_line_opt {
                v.push(RawRow::Bar(bl.clone()));
            }
            v.push(RawRow::Blank);
            v
        } else {
            let dist = idx.abs_diff(if has_started { active_idx } else { view_idx });
            let color = if app.lyrics_scroll_offset != 0 && is_center_focus {
                Color::Rgb(126, 130, 142)
            } else if dist == 1 {
                Color::Rgb(82, 82, 90)
            } else {
                Color::Rgb(56, 56, 62)
            };
            let t = app.lrc_lines[idx].time_sec;
            wrap_plain_text(&app.lrc_lines[idx].text, inner_w)
                .into_iter()
                .map(|text| RawRow::Plain { text, idx, t, color })
                .collect()
        }
    };

    let center_rows = make_line_rows(view_idx, true);
    let avail_above = h.saturating_sub(center_rows.len()) / 2;
    let avail_below = h.saturating_sub(center_rows.len() + avail_above);

    let mut above_rows: Vec<RawRow> = Vec::new();
    for back in 1..=view_idx {
        if above_rows.len() >= avail_above {
            break;
        }
        let idx = view_idx - back;
        for r in make_line_rows(idx, false).into_iter().rev() {
            if above_rows.len() >= avail_above {
                break;
            }
            above_rows.push(r);
        }
    }
    above_rows.reverse();

    let mut below_rows: Vec<RawRow> = Vec::new();
    for fwd in 1.. {
        let idx = view_idx + fwd;
        if idx >= app.lrc_lines.len() || below_rows.len() >= avail_below {
            break;
        }
        for r in make_line_rows(idx, false) {
            if below_rows.len() >= avail_below {
                break;
            }
            below_rows.push(r);
        }
    }

    let mut all_raw = Vec::with_capacity(h);
    for _ in 0..avail_above.saturating_sub(above_rows.len()) {
        all_raw.push(RawRow::Blank);
    }
    all_raw.extend(above_rows);
    all_raw.extend(center_rows);
    all_raw.extend(below_rows);

    let mut out: Vec<Line<'static>> = Vec::with_capacity(h);
    let mut row_map: Vec<(u16, usize, f64)> = Vec::new();
    let mut help_btns: Vec<(Rect, usize)> = Vec::new();
    let box_inner_w = lrc_area.width.saturating_sub(2) as usize;

    for raw in all_raw.into_iter().take(h) {
        let sy = lrc_area.y + 1 + out.len() as u16;
        match raw {
            RawRow::Blank => out.push(Line::from("")),
            RawRow::Bar(bl) => out.push(bl),
            RawRow::Plain { text, idx, t, color } => {
                let hovered = mouse_in_lyrics && my == sy;
                row_map.push((sy, idx, t));
                if hovered {
                    let total_chars = text.chars().count() + 4;
                    let left_pad = box_inner_w.saturating_sub(total_chars) / 2;
                    let q_x = lrc_area.x + 1 + (left_pad + text.chars().count() + 1) as u16;
                    let q_rect = Rect::new(q_x, sy, 3, 1);
                    let q_hov = rect_contains(q_rect, mx, my);
                    help_btns.push((q_rect, idx));
                    out.push(Line::from(vec![
                        Span::styled(
                            text,
                            Style::default()
                                .fg(Color::Rgb(178, 186, 200))
                                .add_modifier(Modifier::UNDERLINED),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            "[?]",
                            if q_hov {
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::Yellow)
                            },
                        ),
                    ]));
                } else {
                    out.push(Line::from(Span::styled(text, Style::default().fg(color))));
                }
            }
            RawRow::Active {
                mut spans,
                idx,
                t,
                show_help,
            } => {
                row_map.push((sy, idx, t));
                if show_help {
                    let text_chars: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                    let total_chars = text_chars + 4;
                    let left_pad = box_inner_w.saturating_sub(total_chars) / 2;
                    let q_x = lrc_area.x + 1 + (left_pad + text_chars + 1) as u16;
                    let q_rect = Rect::new(q_x, sy, 3, 1);
                    let q_hov = mouse_in_lyrics && rect_contains(q_rect, mx, my);
                    help_btns.push((q_rect, idx));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        "[?]",
                        if q_hov {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Rgb(120, 122, 130))
                        },
                    ));
                }
                out.push(Line::from(spans));
            }
        }
    }

    (out, row_map, help_btns)
}

fn run_tui(env_max_mb: Option<u64>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(env_max_mb);

    loop {
        app.poll_worker();
        app.tick_playback();

        terminal.draw(|f| {
            app.hits.buttons.clear();
            app.hits.lyrics_rows.clear();
            app.hits.lyric_help_btns.clear();
            app.hits.artist_links.clear();
            app.hits.album_links.clear();
            app.hits.dl_play_rows.clear();
            app.hits.dl_del_btns.clear();
            app.hits.settings_list = Rect::default();
            app.hits.lyrics_area = Rect::default();
            app.hits.dl_dropdown = Rect::default();
            app.hits.popup_rect = Rect::default();

            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // 0: Top Bar ([ itmp ] | Search | [ Search ] | [ v DL ] | [ Settings ])
                    Constraint::Min(10),   // 1: Body: Library+Cover | Main View | Live Lyrics
                    Constraint::Length(3), // 2: Now Playing progress + time left
                    Constraint::Length(3), // 3: Controls bar
                ])
                .split(f.area());

            // 1. Spotify-style Top Navigation Bar
            let top_cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(10), // [ itmp ]
                    Constraint::Min(18),    // Search input
                    Constraint::Length(11), // [ Search ]
                    Constraint::Length(10), // [ v DL ]
                    Constraint::Length(13), // [ Settings ]
                ])
                .split(outer[0]);

            let home_active = matches!(
                app.view,
                View::AllTracks | View::Liked | View::Queue | View::Playlist(_)
            );
            let home_btn = Paragraph::new(" itmp ")
                .alignment(Alignment::Center)
                .style(if home_active {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if home_active {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        }),
                );
            app.hits.buttons.push((top_cols[0], ButtonAction::Home));
            f.render_widget(home_btn, top_cols[0]);

            // Top Search input box
            app.hits.search_box = top_cols[1];
            let is_searching = matches!(app.input_mode, InputMode::Search);
            let (search_text, search_style, search_border) = if is_searching {
                (
                    format!(" / {}_", app.input_buf),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Green),
                )
            } else if !app.last_search_query.is_empty() && matches!(app.view, View::Search) {
                (
                    format!(" / {}", app.last_search_query),
                    Style::default().fg(Color::White),
                    Style::default().fg(Color::Green),
                )
            } else {
                (
                    " / Search songs, albums, artists, or paste Spotify/YouTube URL...".to_string(),
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::DarkGray),
                )
            };
            let search_box = Paragraph::new(search_text).style(search_style).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(search_border),
            );
            f.render_widget(search_box, top_cols[1]);

            // Top [ Search ] button
            let search_btn_active = is_searching || matches!(app.view, View::Search);
            let search_btn = Paragraph::new(" Search ")
                .alignment(Alignment::Center)
                .style(if search_btn_active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if search_btn_active {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        }),
                );
            app.hits.buttons.push((top_cols[2], ButtonAction::SearchBtn));
            f.render_widget(search_btn, top_cols[2]);

            // Top [ v DL ] Downloads dropdown button right next to Search
            app.hits.dl_btn = top_cols[3];
            let dl_label = if app.busy { " * DL " } else { " v DL " };
            let dl_btn = Paragraph::new(dl_label)
                .alignment(Alignment::Center)
                .style(if app.show_downloads_menu {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if app.busy {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if app.show_downloads_menu {
                            Style::default().fg(Color::Cyan)
                        } else if app.busy {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        }),
                );
            app.hits.buttons.push((top_cols[3], ButtonAction::ToggleDownloads));
            f.render_widget(dl_btn, top_cols[3]);

            // Top [ Settings ] button
            let settings_active = matches!(app.view, View::Settings);
            let settings_btn = Paragraph::new(" Settings ")
                .alignment(Alignment::Center)
                .style(if settings_active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if settings_active {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        }),
                );
            app.hits.buttons.push((top_cols[4], ButtonAction::Settings));
            f.render_widget(settings_btn, top_cols[4]);

            // 2. Body split
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Min(30)])
                .split(outer[1]);

            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(12)])
                .split(body[0]);

            app.hits.sidebar = left[0];

            // Clean Spotify-style Library Sidebar (Tracks, Liked, Queue, Liked Artists, Playlists)
            let sb_rows = app.sidebar_rows();
            let sb_items: Vec<ListItem> = sb_rows
                .iter()
                .map(|(label, entry)| match entry {
                    SidebarEntry::Header => ListItem::new(Span::styled(
                        label.clone(),
                        Style::default().fg(Color::DarkGray),
                    )),
                    SidebarEntry::Artist(_) => ListItem::new(Span::styled(
                        label.clone(),
                        Style::default().fg(Color::Cyan),
                    )),
                    SidebarEntry::NewPlaylist => ListItem::new(Span::styled(
                        label.clone(),
                        Style::default().fg(Color::Green),
                    )),
                    _ => ListItem::new(label.clone()),
                })
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

            // Right area: Main View + optional Live Lyrics pane
            let (main_area, lyrics_area_opt) = if app.show_lyrics_pane && !matches!(app.view, View::Settings) {
                if body[1].width >= 78 {
                    let cols = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .split(body[1]);
                    (cols[0], Some(cols[1]))
                } else {
                    let rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                        .split(body[1]);
                    (rows[0], Some(rows[1]))
                }
            } else {
                (body[1], None)
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
                let search_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(6)])
                    .split(main_area);

                let songs_n = app.search_results.len();
                let all_al_n = app.search_albums_list.len();
                let albums_n = app
                    .search_albums_list
                    .iter()
                    .filter(|a| a.record_type == "album" || a.record_type == "compile")
                    .count();
                let eps_n = app
                    .search_albums_list
                    .iter()
                    .filter(|a| a.record_type == "ep")
                    .count();
                let singles_n = app
                    .search_albums_list
                    .iter()
                    .filter(|a| a.record_type == "single")
                    .count();

                let wide_tabs = search_rows[0].width >= 72;
                let s_tab_style = |tab: SearchTab, cnt: usize| {
                    if app.search_tab == tab {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else if cnt > 0 {
                        Style::default().fg(Color::Rgb(218, 222, 230))
                    } else {
                        Style::default().fg(Color::DarkGray)
                    }
                };
                let s_tab_lbl = |name: &str, cnt: usize| {
                    if wide_tabs {
                        format!("[ {name} ({cnt}) ]")
                    } else {
                        format!("[ {name} ]")
                    }
                };
                let s_tab_line = render_inline_buttons(
                    vec![
                        (
                            s_tab_lbl("Songs", songs_n),
                            s_tab_style(SearchTab::Songs, songs_n),
                            Some(ButtonAction::SearchTab(SearchTab::Songs)),
                        ),
                        (
                            s_tab_lbl("All", all_al_n),
                            s_tab_style(SearchTab::AllReleases, all_al_n),
                            Some(ButtonAction::SearchTab(SearchTab::AllReleases)),
                        ),
                        (
                            s_tab_lbl("Albums", albums_n),
                            s_tab_style(SearchTab::Albums, albums_n),
                            Some(ButtonAction::SearchTab(SearchTab::Albums)),
                        ),
                        (
                            s_tab_lbl("EPs", eps_n),
                            s_tab_style(SearchTab::Eps, eps_n),
                            Some(ButtonAction::SearchTab(SearchTab::Eps)),
                        ),
                        (
                            s_tab_lbl("Singles", singles_n),
                            s_tab_style(SearchTab::Singles, singles_n),
                            Some(ButtonAction::SearchTab(SearchTab::Singles)),
                        ),
                    ],
                    search_rows[0],
                    1,
                    &mut app.hits.buttons,
                );
                let s_tab_bar = Paragraph::new(s_tab_line).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(" Filter "),
                );
                f.render_widget(s_tab_bar, search_rows[0]);

                let list_area = search_rows[1];
                app.hits.track_list = list_area;

                if app.search_tab == SearchTab::Songs {
                    if app.search_results.is_empty() {
                        let msg = if app.busy {
                            format!("\n  Searching for '{}'...", app.last_search_query)
                        } else {
                            "\n  Search for songs, albums, EPs, artists, or paste a Spotify / YouTube URL above."
                                .to_string()
                        };
                        let empty_search = Paragraph::new(msg)
                            .style(Style::default().fg(if app.busy { Color::Yellow } else { Color::Gray }))
                            .wrap(Wrap { trim: false })
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::Green))
                                    .title(" Search "),
                            );
                        f.render_widget(empty_search, list_area);
                    } else {
                        let title_max = list_area.width.saturating_sub(28).clamp(16, 48) as usize;
                        let scroll_off = app.search_state.offset();
                        let (mx, my) = app.mouse_pos;
                        let max_y = list_area.y + list_area.height.saturating_sub(1);
                        let items: Vec<ListItem> = app
                            .search_results
                            .iter()
                            .enumerate()
                            .map(|(i, r)| {
                                let thumb_key = format!("yt:{}", r.id);
                                let thumb_rows = app.thumbs.get_by_key(&thumb_key).cloned();
                                let downloaded = find_cached_track(&app.lib, &r.title, &r.uploader).is_some();

                                let mut l1: Vec<Span<'static>> = Vec::new();
                                let mut l2: Vec<Span<'static>> = Vec::new();
                                if let Some([t0, t1]) = thumb_rows {
                                    l1.extend(t0);
                                    l2.extend(t1);
                                } else {
                                    l1.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                    l2.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                }
                                l1.push(Span::raw(" "));
                                l2.push(Span::raw(" "));

                                let mut t_disp = r.title.clone();
                                if t_disp.chars().count() > title_max {
                                    t_disp = t_disp.chars().take(title_max - 1).collect::<String>() + "..";
                                }
                                l1.push(Span::styled(
                                    format!("[{:>5}] ", r.duration),
                                    Style::default().fg(Color::Rgb(118, 238, 148)),
                                ));
                                l1.push(Span::styled(
                                    t_disp,
                                    Style::default()
                                        .fg(Color::Rgb(244, 244, 246))
                                        .add_modifier(Modifier::BOLD),
                                ));
                                if downloaded {
                                    l1.push(Span::styled(
                                        " [DOWNLOADED]",
                                        Style::default().fg(Color::Rgb(118, 238, 148)),
                                    ));
                                }

                                l2.push(Span::styled("        ", Style::default()));
                                let mut artist_style = Style::default().fg(Color::Cyan);
                                if i >= scroll_off {
                                    let row_y2 = list_area.y + 1 + ((i - scroll_off) * 2) as u16 + 1;
                                    if row_y2 < max_y && !r.uploader.is_empty() {
                                        // border(1) + highlight(2) + thumb(4) + space(1) + indent(8) = +16
                                        let ax = list_area.x + 16;
                                        let aw = (r.uploader.chars().count() as u16)
                                            .min(list_area.width.saturating_sub(18));
                                        let arect = Rect::new(ax, row_y2, aw, 1);
                                        if rect_contains(arect, mx, my) {
                                            artist_style = artist_style.add_modifier(Modifier::UNDERLINED);
                                        }
                                        app.hits.artist_links.push((arect, r.uploader.clone()));
                                    }
                                }
                                l2.push(Span::styled(r.uploader.clone(), artist_style));

                                ListItem::new(vec![Line::from(l1), Line::from(l2)])
                            })
                            .collect();
                        let list = List::new(items)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::Green))
                                    .title(format!(" Songs ({}) ", app.search_results.len())),
                            )
                            .highlight_style(
                                Style::default()
                                    .bg(Color::Rgb(46, 52, 62))
                                    .add_modifier(Modifier::BOLD),
                            )
                            .highlight_symbol("| ");
                        f.render_stateful_widget(list, list_area, &mut app.search_state);
                    }
                } else {
                    let filtered = app.filtered_search_albums();
                    let tab_title = match app.search_tab {
                        SearchTab::AllReleases => format!(" All Releases ({}) ", filtered.len()),
                        SearchTab::Albums => format!(" Albums ({}) ", filtered.len()),
                        SearchTab::Eps => format!(" EPs ({}) ", filtered.len()),
                        SearchTab::Singles => format!(" Singles ({}) ", filtered.len()),
                        SearchTab::Songs => " Releases ".into(),
                    };
                    if filtered.is_empty() {
                        let msg = if app.busy {
                            format!("\n  Searching releases for '{}'...", app.last_search_query)
                        } else if app.last_search_query.is_empty() {
                            "\n  Search for an artist or album name above to browse Albums, EPs & Singles.".into()
                        } else {
                            format!("\n  No matching releases in this category for '{}'.", app.last_search_query)
                        };
                        let empty = Paragraph::new(msg)
                            .style(Style::default().fg(if app.busy { Color::Yellow } else { Color::Gray }))
                            .wrap(Wrap { trim: false })
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::Green))
                                    .title(tab_title),
                            );
                        f.render_widget(empty, list_area);
                    } else {
                        let title_max = list_area.width.saturating_sub(26).clamp(16, 48) as usize;
                        let scroll_off = app.search_album_state.offset();
                        let (mx, my) = app.mouse_pos;
                        let max_y = list_area.y + list_area.height.saturating_sub(1);
                        let items: Vec<ListItem> = filtered
                            .iter()
                            .enumerate()
                            .map(|(i, al)| {
                                let thumb_rows = al
                                    .cover_url
                                    .as_ref()
                                    .and_then(|u| app.thumbs.get_by_key(&format!("url:{u}")))
                                    .cloned();
                                let mut l1: Vec<Span<'static>> = Vec::new();
                                let mut l2: Vec<Span<'static>> = Vec::new();
                                if let Some([t0, t1]) = thumb_rows {
                                    l1.extend(t0);
                                    l2.extend(t1);
                                } else {
                                    l1.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                    l2.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                }
                                l1.push(Span::raw(" "));
                                l2.push(Span::raw(" "));

                                let badge_color = match al.record_type.as_str() {
                                    "ep" => Color::Cyan,
                                    "single" => Color::Rgb(118, 238, 148),
                                    "compile" => Color::Magenta,
                                    _ => Color::Yellow,
                                };
                                let badge = format!("[{}] ", al.type_badge());
                                let mut t_disp = al.title.clone();
                                if t_disp.chars().count() > title_max {
                                    t_disp = t_disp.chars().take(title_max - 1).collect::<String>() + "..";
                                }
                                l1.push(Span::styled(badge, Style::default().fg(badge_color)));
                                l1.push(Span::styled(
                                    t_disp,
                                    Style::default()
                                        .fg(Color::Rgb(244, 244, 246))
                                        .add_modifier(Modifier::BOLD),
                                ));

                                let mut cur_x = list_area.x + 8; // border(1) + highlight(2) + thumb(4) + space(1)
                                let row_y2_opt = if i >= scroll_off {
                                    let y2 = list_area.y + 1 + ((i - scroll_off) * 2) as u16 + 1;
                                    (y2 < max_y).then_some(y2)
                                } else {
                                    None
                                };
                                if !al.artist.is_empty() {
                                    let mut a_style = Style::default().fg(Color::Cyan);
                                    let a_len = al.artist.chars().count() as u16;
                                    if let Some(y2) = row_y2_opt {
                                        let arect = Rect::new(cur_x, y2, a_len, 1);
                                        if rect_contains(arect, mx, my) {
                                            a_style = a_style.add_modifier(Modifier::UNDERLINED);
                                        }
                                        app.hits.artist_links.push((arect, al.artist.clone()));
                                    }
                                    l2.push(Span::styled(al.artist.clone(), a_style));
                                    cur_x += a_len;
                                }
                                let mut meta = String::new();
                                if !al.release_year.is_empty() {
                                    meta.push_str(&format!(" | {}", al.release_year));
                                }
                                if al.track_count > 0 {
                                    let unit = if al.track_count == 1 { "track" } else { "tracks" };
                                    meta.push_str(&format!(" | {} {unit}", al.track_count));
                                }
                                if !meta.is_empty() {
                                    let _ = cur_x;
                                    l2.push(Span::styled(meta, Style::default().fg(Color::Rgb(150, 155, 165))));
                                }
                                ListItem::new(vec![Line::from(l1), Line::from(l2)])
                            })
                            .collect();
                        let list = List::new(items)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::Green))
                                    .title(tab_title),
                            )
                            .highlight_style(
                                Style::default()
                                    .bg(Color::Rgb(46, 52, 62))
                                    .add_modifier(Modifier::BOLD),
                            )
                            .highlight_symbol("| ");
                        f.render_stateful_widget(list, list_area, &mut app.search_album_state);
                    }
                }
            } else if matches!(app.view, View::Artist) {
                let artist_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(9), Constraint::Length(3), Constraint::Min(6)])
                    .split(main_area);

                // Artist Profile Header (Avatar + Info + Like/Import buttons)
                let hdr_cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(18), Constraint::Min(24)])
                    .split(artist_rows[0]);

                let av_w = hdr_cols[0].width.saturating_sub(2);
                let av_h = hdr_cols[0].height.saturating_sub(2);
                app.ensure_artist_avatar(av_w, av_h);
                let av_widget = if let Some(art) = &app.artist_avatar_art {
                    Paragraph::new(art.lines.clone())
                        .alignment(Alignment::Center)
                        .block(Block::default().borders(Borders::ALL).title(" Artist "))
                } else {
                    Paragraph::new("\n\n  Loading...")
                        .style(Style::default().fg(Color::DarkGray))
                        .block(Block::default().borders(Borders::ALL).title(" Artist "))
                };
                f.render_widget(av_widget, hdr_cols[0]);

                if let Some(prof) = app.artist_profile.clone() {
                    let liked = app.is_artist_liked(&prof.name);
                    let dl_cnt = app
                        .lib
                        .entries
                        .values()
                        .filter(|e| {
                            clean_artist_name(&e.artist)
                                .to_ascii_lowercase()
                                .contains(&prof.name.to_ascii_lowercase())
                        })
                        .count();

                    let info_rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(4), Constraint::Length(3), Constraint::Min(2)])
                        .split(hdr_cols[1]);

                    let mut meta_lines = vec![Line::from(vec![
                        Span::styled(
                            format!(" {}", prof.name),
                            Style::default().fg(Color::Rgb(245, 245, 248)).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  [{}]", prof.source),
                            Style::default().fg(Color::Rgb(118, 238, 148)),
                        ),
                    ])];
                    let mut sub = format!(" {} tracks downloaded", dl_cnt);
                    if let Some(fans) = prof.fans {
                        sub.push_str(&format!("  |  {}", fmt_fans(fans)));
                    }
                    meta_lines.push(Line::from(Span::styled(sub, Style::default().fg(Color::Rgb(165, 170, 180)))));
                    if let Some(u) = &prof.spotify_url {
                        meta_lines.push(Line::from(Span::styled(
                            format!(" {u}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    let info_box = Paragraph::new(meta_lines).block(
                        Block::default()
                            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                            .border_style(Style::default().fg(Color::Green)),
                    );
                    f.render_widget(info_box, info_rows[0]);

                    // Top border is omitted on info_rows[1], so button text sits on row offset 0!
                    let btn_line = render_inline_buttons(
                        vec![
                            (
                                if liked { "[ * Liked Artist ]".into() } else { "[ + Like Artist ]".into() },
                                if liked {
                                    Style::default()
                                        .fg(Color::Black)
                                        .bg(Color::Yellow)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                                },
                                Some(ButtonAction::ToggleLikeArtist),
                            ),
                            (
                                "[ + Import Top Tracks ]".into(),
                                Style::default().fg(Color::Rgb(118, 238, 148)).add_modifier(Modifier::BOLD),
                                Some(ButtonAction::ArtistImportAll),
                            ),
                            (
                                "[ Back ]".into(),
                                Style::default().fg(Color::Gray),
                                Some(ButtonAction::Home),
                            ),
                        ],
                        info_rows[1],
                        0,
                        &mut app.hits.buttons,
                    );
                    let act_bar = Paragraph::new(btn_line).block(
                        Block::default()
                            .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                            .border_style(Style::default().fg(Color::Green)),
                    );
                    f.render_widget(act_bar, info_rows[1]);

                    // Spotify-style Discography Filter Tabs
                    let top_n = prof.top_tracks.len();
                    let all_n = prof.albums.len();
                    let alb_n = prof
                        .albums
                        .iter()
                        .filter(|a| a.record_type == "album" || a.record_type == "compile")
                        .count();
                    let ep_n = prof.albums.iter().filter(|a| a.record_type == "ep").count();
                    let sng_n = prof.albums.iter().filter(|a| a.record_type == "single").count();

                    let wide_tabs = artist_rows[1].width >= 72;
                    let a_tab_style = |tab: ArtistTab, cnt: usize| {
                        if app.artist_tab == tab {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Green)
                                .add_modifier(Modifier::BOLD)
                        } else if cnt > 0 {
                            Style::default().fg(Color::Rgb(218, 222, 230))
                        } else {
                            Style::default().fg(Color::DarkGray)
                        }
                    };
                    let a_tab_lbl = |name: &str, cnt: usize| {
                        if wide_tabs {
                            format!("[ {name} ({cnt}) ]")
                        } else {
                            format!("[ {name} ]")
                        }
                    };
                    let tab_line = render_inline_buttons(
                        vec![
                            (
                                a_tab_lbl("Top", top_n),
                                a_tab_style(ArtistTab::TopTracks, top_n),
                                Some(ButtonAction::ArtistTab(ArtistTab::TopTracks)),
                            ),
                            (
                                a_tab_lbl("All", all_n),
                                a_tab_style(ArtistTab::All, all_n),
                                Some(ButtonAction::ArtistTab(ArtistTab::All)),
                            ),
                            (
                                a_tab_lbl("Albums", alb_n),
                                a_tab_style(ArtistTab::Albums, alb_n),
                                Some(ButtonAction::ArtistTab(ArtistTab::Albums)),
                            ),
                            (
                                a_tab_lbl("EPs", ep_n),
                                a_tab_style(ArtistTab::Eps, ep_n),
                                Some(ButtonAction::ArtistTab(ArtistTab::Eps)),
                            ),
                            (
                                a_tab_lbl("Singles", sng_n),
                                a_tab_style(ArtistTab::Singles, sng_n),
                                Some(ButtonAction::ArtistTab(ArtistTab::Singles)),
                            ),
                        ],
                        artist_rows[1],
                        1,
                        &mut app.hits.buttons,
                    );
                    let tab_bar = Paragraph::new(tab_line).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray))
                            .title(" Discography "),
                    );
                    f.render_widget(tab_bar, artist_rows[1]);

                    let list_area = artist_rows[2];
                    app.hits.track_list = list_area;

                    if app.artist_tab == ArtistTab::TopTracks {
                        // Artist Top Tracks (2-line rows with 4x2 album cover art + hover-underlined Artist & Album)
                        let title_max = list_area.width.saturating_sub(28).clamp(16, 46) as usize;
                        let scroll_off = app.artist_track_state.offset();
                        let (mx, my) = app.mouse_pos;
                        let max_y = list_area.y + list_area.height.saturating_sub(1);
                        let items: Vec<ListItem> = prof
                            .top_tracks
                            .iter()
                            .enumerate()
                            .map(|(i, tr)| {
                                let thumb_rows = tr
                                    .cover_url
                                    .as_ref()
                                    .and_then(|u| app.thumbs.get_by_key(&format!("url:{u}")))
                                    .cloned();
                                let downloaded = find_cached_track(&app.lib, &tr.title, &tr.artist).is_some();

                                let mut l1: Vec<Span<'static>> = Vec::new();
                                let mut l2: Vec<Span<'static>> = Vec::new();
                                if let Some([t0, t1]) = thumb_rows {
                                    l1.extend(t0);
                                    l2.extend(t1);
                                } else {
                                    l1.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                    l2.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                }
                                l1.push(Span::raw(" "));
                                l2.push(Span::raw(" "));

                                let mut t_disp = tr.title.clone();
                                if t_disp.chars().count() > title_max {
                                    t_disp = t_disp.chars().take(title_max - 1).collect::<String>() + "..";
                                }
                                l1.push(Span::styled(
                                    format!("{:>2}. ", i + 1),
                                    Style::default().fg(Color::Rgb(135, 140, 150)),
                                ));
                                l1.push(Span::styled(
                                    t_disp,
                                    Style::default()
                                        .fg(Color::Rgb(244, 244, 246))
                                        .add_modifier(Modifier::BOLD),
                                ));
                                if downloaded {
                                    l1.push(Span::styled(
                                        " [DOWNLOADED]",
                                        Style::default().fg(Color::Rgb(118, 238, 148)),
                                    ));
                                }

                                let dur = if tr.duration_sec > 0 {
                                    fmt_time(tr.duration_sec as f64)
                                } else {
                                    "--:--".to_string()
                                };
                                l2.push(Span::raw("    "));
                                let row_y2_opt = if i >= scroll_off {
                                    let y2 = list_area.y + 1 + ((i - scroll_off) * 2) as u16 + 1;
                                    (y2 < max_y).then_some(y2)
                                } else {
                                    None
                                };
                                // border(1) + highlight(2) + thumb(4) + space(1) + indent(4) = +12
                                let mut cur_x = list_area.x + 12;
                                let mut a_style = Style::default().fg(Color::Cyan);
                                let a_len = tr.artist.chars().count() as u16;
                                if let Some(y2) = row_y2_opt {
                                    if a_len > 0 {
                                        let arect = Rect::new(cur_x, y2, a_len, 1);
                                        if rect_contains(arect, mx, my) {
                                            a_style = a_style.add_modifier(Modifier::UNDERLINED);
                                        }
                                        app.hits.artist_links.push((arect, tr.artist.clone()));
                                    }
                                }
                                l2.push(Span::styled(tr.artist.clone(), a_style));
                                cur_x += a_len;

                                if !tr.album.is_empty() {
                                    l2.push(Span::styled(" - ", Style::default().fg(Color::Rgb(135, 140, 150))));
                                    cur_x += 3;
                                    let mut al_style = Style::default().fg(Color::Rgb(165, 172, 185));
                                    let al_len = tr.album.chars().count() as u16;
                                    if let Some(y2) = row_y2_opt {
                                        let alrect = Rect::new(cur_x, y2, al_len, 1);
                                        if rect_contains(alrect, mx, my) {
                                            al_style = al_style.add_modifier(Modifier::UNDERLINED);
                                        }
                                        app.hits.album_links.push((alrect, tr.artist.clone(), tr.album.clone()));
                                    }
                                    l2.push(Span::styled(tr.album.clone(), al_style));
                                }
                                l2.push(Span::styled(
                                    format!(" | {dur}"),
                                    Style::default().fg(Color::Rgb(135, 140, 150)),
                                ));

                                ListItem::new(vec![Line::from(l1), Line::from(l2)])
                            })
                            .collect();

                        let list = List::new(items)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(main_border)
                                    .title(format!(" Top Tracks ({}) ", prof.top_tracks.len())),
                            )
                            .highlight_style(
                                Style::default()
                                    .bg(Color::Rgb(46, 52, 62))
                                    .add_modifier(Modifier::BOLD),
                            )
                            .highlight_symbol("| ");
                        f.render_stateful_widget(list, list_area, &mut app.artist_track_state);
                    } else {
                        let filtered = app.filtered_artist_albums();
                        let disc_title = match app.artist_tab {
                            ArtistTab::All => format!(" All Releases ({}) ", filtered.len()),
                            ArtistTab::Albums => format!(" Albums ({}) ", filtered.len()),
                            ArtistTab::Eps => format!(" EPs ({}) ", filtered.len()),
                            ArtistTab::Singles => format!(" Singles ({}) ", filtered.len()),
                            ArtistTab::TopTracks => " Top Tracks ".into(),
                        };
                        if filtered.is_empty() {
                            let p = Paragraph::new("\n  No releases in this category.")
                                .style(Style::default().fg(Color::Gray))
                                .block(
                                    Block::default()
                                        .borders(Borders::ALL)
                                        .border_style(main_border)
                                        .title(disc_title),
                                );
                            f.render_widget(p, list_area);
                        } else {
                            let title_max = list_area.width.saturating_sub(24).clamp(16, 48) as usize;
                            let items: Vec<ListItem> = filtered
                                .iter()
                                .map(|al| {
                                    let thumb_rows = al
                                        .cover_url
                                        .as_ref()
                                        .and_then(|u| app.thumbs.get_by_key(&format!("url:{u}")))
                                        .cloned();
                                    let mut l1: Vec<Span<'static>> = Vec::new();
                                    let mut l2: Vec<Span<'static>> = Vec::new();
                                    if let Some([t0, t1]) = thumb_rows {
                                        l1.extend(t0);
                                        l2.extend(t1);
                                    } else {
                                        l1.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                        l2.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                    }
                                    l1.push(Span::raw(" "));
                                    l2.push(Span::raw(" "));

                                    let badge_color = match al.record_type.as_str() {
                                        "ep" => Color::Cyan,
                                        "single" => Color::Rgb(118, 238, 148),
                                        "compile" => Color::Magenta,
                                        _ => Color::Yellow,
                                    };
                                    let mut t_disp = al.title.clone();
                                    if t_disp.chars().count() > title_max {
                                        t_disp = t_disp.chars().take(title_max - 1).collect::<String>() + "..";
                                    }
                                    l1.push(Span::styled(
                                        format!("[{}] ", al.type_badge()),
                                        Style::default().fg(badge_color),
                                    ));
                                    l1.push(Span::styled(
                                        t_disp,
                                        Style::default()
                                            .fg(Color::Rgb(244, 244, 246))
                                            .add_modifier(Modifier::BOLD),
                                    ));

                                    let mut sub = if al.release_year.is_empty() {
                                        al.type_badge().to_string()
                                    } else {
                                        format!("{} | {}", al.release_year, al.type_badge())
                                    };
                                    if al.track_count > 0 {
                                        let unit = if al.track_count == 1 { "track" } else { "tracks" };
                                        sub.push_str(&format!(" | {} {unit}", al.track_count));
                                    }
                                    l2.push(Span::styled(sub, Style::default().fg(Color::Rgb(155, 160, 170))));
                                    ListItem::new(vec![Line::from(l1), Line::from(l2)])
                                })
                                .collect();
                            let list = List::new(items)
                                .block(
                                    Block::default()
                                        .borders(Borders::ALL)
                                        .border_style(main_border)
                                        .title(disc_title),
                                )
                                .highlight_style(
                                    Style::default()
                                        .bg(Color::Rgb(46, 52, 62))
                                        .add_modifier(Modifier::BOLD),
                                )
                                .highlight_symbol("| ");
                            f.render_stateful_widget(list, list_area, &mut app.artist_album_state);
                        }
                    }
                } else {
                    let loading_msg = app
                        .artist_loading_name
                        .as_deref()
                        .map(|n| format!("\n  Loading profile for '{n}'..."))
                        .unwrap_or_else(|| "\n  Select an artist to view their profile.".into());
                    let p = Paragraph::new(loading_msg)
                        .style(Style::default().fg(Color::Yellow))
                        .block(Block::default().borders(Borders::ALL).title(" Artist Profile "));
                    f.render_widget(p, hdr_cols[1]);
                }
            } else if matches!(app.view, View::Album) {
                let album_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(9), Constraint::Min(6)])
                    .split(main_area);

                let hdr_cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(18), Constraint::Min(24)])
                    .split(album_rows[0]);

                let cov_w = hdr_cols[0].width.saturating_sub(2);
                let cov_h = hdr_cols[0].height.saturating_sub(2);
                app.ensure_album_cover(cov_w, cov_h);
                let cov_widget = if let Some(art) = &app.album_cover_art {
                    Paragraph::new(art.lines.clone())
                        .alignment(Alignment::Center)
                        .block(Block::default().borders(Borders::ALL).title(" Cover "))
                } else {
                    Paragraph::new("\n\n  Loading...")
                        .style(Style::default().fg(Color::DarkGray))
                        .block(Block::default().borders(Borders::ALL).title(" Cover "))
                };
                f.render_widget(cov_widget, hdr_cols[0]);

                if let Some(det) = app.album_detail.clone() {
                    let dl_cnt = det
                        .tracks
                        .iter()
                        .filter(|t| find_cached_track(&app.lib, &t.title, &t.artist).is_some())
                        .count();

                    let info_rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(4), Constraint::Length(3), Constraint::Min(2)])
                        .split(hdr_cols[1]);

                    let badge_color = match det.record_type.as_str() {
                        "ep" => Color::Cyan,
                        "single" => Color::Rgb(118, 238, 148),
                        "compile" => Color::Magenta,
                        _ => Color::Yellow,
                    };
                    let mut meta_lines = vec![Line::from(vec![
                        Span::styled(
                            format!(" {}", det.title),
                            Style::default().fg(Color::Rgb(245, 245, 248)).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  [{}]", det.type_badge()),
                            Style::default().fg(badge_color),
                        ),
                    ])];
                    let mut sub = format!(" {}", det.artist);
                    if !det.release_date.is_empty() {
                        sub.push_str(&format!("  |  {}", det.release_date));
                    }
                    sub.push_str(&format!("  |  {} tracks", det.tracks.len()));
                    meta_lines.push(Line::from(Span::styled(sub, Style::default().fg(Color::Cyan))));
                    meta_lines.push(Line::from(Span::styled(
                        format!(" {} / {} downloaded", dl_cnt, det.tracks.len()),
                        Style::default().fg(Color::Rgb(165, 170, 180)),
                    )));
                    let info_box = Paragraph::new(meta_lines).block(
                        Block::default()
                            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                            .border_style(Style::default().fg(Color::Green)),
                    );
                    f.render_widget(info_box, info_rows[0]);

                    let btn_line = render_inline_buttons(
                        vec![
                            (
                                format!("[ Artist: {} ]", det.artist),
                                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                                Some(ButtonAction::OpenAlbumArtist),
                            ),
                            (
                                "[ + Import Album ]".into(),
                                Style::default().fg(Color::Rgb(118, 238, 148)).add_modifier(Modifier::BOLD),
                                Some(ButtonAction::AlbumImportAll),
                            ),
                            (
                                "[ Back ]".into(),
                                Style::default().fg(Color::Gray),
                                Some(ButtonAction::AlbumBack),
                            ),
                        ],
                        info_rows[1],
                        0,
                        &mut app.hits.buttons,
                    );
                    let act_bar = Paragraph::new(btn_line).block(
                        Block::default()
                            .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                            .border_style(Style::default().fg(Color::Green)),
                    );
                    f.render_widget(act_bar, info_rows[1]);

                    let list_area = album_rows[1];
                    app.hits.track_list = list_area;
                    let title_max = list_area.width.saturating_sub(28).clamp(16, 46) as usize;
                    let scroll_off = app.album_track_state.offset();
                    let (mx, my) = app.mouse_pos;
                    let max_y = list_area.y + list_area.height.saturating_sub(1);
                    let album_thumb = det
                        .cover_url
                        .as_ref()
                        .and_then(|u| app.thumbs.get_by_key(&format!("url:{u}")))
                        .cloned();
                    let items: Vec<ListItem> = det
                        .tracks
                        .iter()
                        .enumerate()
                        .map(|(i, tr)| {
                            let downloaded = find_cached_track(&app.lib, &tr.title, &tr.artist).is_some();
                            let mut l1: Vec<Span<'static>> = Vec::new();
                            let mut l2: Vec<Span<'static>> = Vec::new();
                            if let Some([t0, t1]) = album_thumb.clone() {
                                l1.extend(t0);
                                l2.extend(t1);
                            } else {
                                l1.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                                l2.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                            }
                            l1.push(Span::raw(" "));
                            l2.push(Span::raw(" "));

                            let mut t_disp = tr.title.clone();
                            if t_disp.chars().count() > title_max {
                                t_disp = t_disp.chars().take(title_max - 1).collect::<String>() + "..";
                            }
                            l1.push(Span::styled(
                                format!("{:>2}. ", i + 1),
                                Style::default().fg(Color::Rgb(135, 140, 150)),
                            ));
                            l1.push(Span::styled(
                                t_disp,
                                Style::default()
                                    .fg(Color::Rgb(244, 244, 246))
                                    .add_modifier(Modifier::BOLD),
                            ));
                            if downloaded {
                                l1.push(Span::styled(
                                    " [DOWNLOADED]",
                                    Style::default().fg(Color::Rgb(118, 238, 148)),
                                ));
                            }

                            let dur = if tr.duration_sec > 0 {
                                fmt_time(tr.duration_sec as f64)
                            } else {
                                "--:--".to_string()
                            };
                            l2.push(Span::raw("    "));
                            let row_y2_opt = if i >= scroll_off {
                                let y2 = list_area.y + 1 + ((i - scroll_off) * 2) as u16 + 1;
                                (y2 < max_y).then_some(y2)
                            } else {
                                None
                            };
                            let cur_x = list_area.x + 12;
                            let mut a_style = Style::default().fg(Color::Cyan);
                            let a_len = tr.artist.chars().count() as u16;
                            if let Some(y2) = row_y2_opt {
                                if a_len > 0 {
                                    let arect = Rect::new(cur_x, y2, a_len, 1);
                                    if rect_contains(arect, mx, my) {
                                        a_style = a_style.add_modifier(Modifier::UNDERLINED);
                                    }
                                    app.hits.artist_links.push((arect, tr.artist.clone()));
                                }
                            }
                            l2.push(Span::styled(tr.artist.clone(), a_style));
                            l2.push(Span::styled(
                                format!(" | {dur}"),
                                Style::default().fg(Color::Rgb(135, 140, 150)),
                            ));
                            ListItem::new(vec![Line::from(l1), Line::from(l2)])
                        })
                        .collect();

                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(main_border)
                                .title(format!(" Tracklist ({}) ", det.tracks.len())),
                        )
                        .highlight_style(
                            Style::default()
                                .bg(Color::Rgb(46, 52, 62))
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("| ");
                    f.render_stateful_widget(list, list_area, &mut app.album_track_state);
                } else {
                    let loading_msg = app
                        .album_loading_name
                        .as_deref()
                        .map(|n| format!("\n  Loading '{n}'..."))
                        .unwrap_or_else(|| "\n  Select an album to view its tracks.".into());
                    let p = Paragraph::new(loading_msg)
                        .style(Style::default().fg(Color::Yellow))
                        .block(Block::default().borders(Borders::ALL).title(" Album "));
                    f.render_widget(p, hdr_cols[1]);
                }
            } else if matches!(app.view, View::Settings) {
                // Settings split: Left = Cache + Spotify Linking | Right = Keybinds reference
                let (settings_left, keybinds_opt) = if main_area.width >= 82 {
                    let cols = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                        .split(main_area);
                    (cols[0], Some(cols[1]))
                } else {
                    (main_area, None)
                };

                let s_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(8), Constraint::Min(10)])
                    .split(settings_left);

                // Section 1: Audio Cache Settings
                let cache_inner = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Length(2), Constraint::Length(3)])
                    .split(s_rows[0]);

                let used = total_cache_bytes(&app.lib);
                let ratio = ((used as f64) / (app.max_bytes.max(1) as f64)).clamp(0.0, 1.0);
                let cache_label = format!(
                    "{} / {} ({} tracks, {} liked)",
                    fmt_mb(used),
                    fmt_mb(app.max_bytes),
                    app.lib.entries.len(),
                    app.lib.entries.values().filter(|e| e.liked).count()
                );
                let gauge = Gauge::default()
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(" Audio Cache "),
                    )
                    .gauge_style(Style::default().fg(Color::Rgb(68, 152, 92)).bg(Color::Rgb(36, 38, 44)))
                    .ratio(ratio)
                    .label(Span::styled(
                        cache_label,
                        Style::default().fg(Color::Rgb(245, 245, 248)).add_modifier(Modifier::BOLD),
                    ));
                f.render_widget(gauge, cache_inner[0]);

                let cache_path_p = Paragraph::new(format!(
                    " Folder: {}  |  Limit: {} MB",
                    app.dir.display(),
                    app.max_bytes / 1_048_576
                ))
                .style(Style::default().fg(Color::Rgb(165, 170, 180)));
                f.render_widget(cache_path_p, cache_inner[1]);

                let cache_btns = render_inline_buttons(
                    vec![
                        ("[ -100 MB ]".into(), Style::default().fg(Color::White), Some(ButtonAction::CacheDec)),
                        ("[ +100 MB ]".into(), Style::default().fg(Color::White), Some(ButtonAction::CacheInc)),
                        (
                            "[ Prune LRU ]".into(),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                            Some(ButtonAction::PruneCache),
                        ),
                        (
                            "[ + Import ]".into(),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                            Some(ButtonAction::Import),
                        ),
                    ],
                    cache_inner[2],
                    1,
                    &mut app.hits.buttons,
                );
                let cache_bar = Paragraph::new(cache_btns)
                    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
                f.render_widget(cache_bar, cache_inner[2]);

                // Section 2: Spotify Linking & Playlist Imports
                let sp_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(6)])
                    .split(s_rows[1]);

                let mut sp_btn_defs = vec![(
                    "[ + Link URL / CSV ]".into(),
                    Style::default().fg(Color::Rgb(118, 238, 148)).add_modifier(Modifier::BOLD),
                    Some(ButtonAction::SpotifyAddUrl),
                )];
                if app.spotify.is_logged_in() {
                    sp_btn_defs.push((
                        format!("({})", app.spotify.display_name),
                        Style::default().fg(Color::Gray),
                        None,
                    ));
                    sp_btn_defs.push((
                        "[ Refresh ]".into(),
                        Style::default().fg(Color::White),
                        Some(ButtonAction::SpotifyRefresh),
                    ));
                    sp_btn_defs.push((
                        "[ Logout ]".into(),
                        Style::default().fg(Color::Yellow),
                        Some(ButtonAction::SpotifyLogout),
                    ));
                } else {
                    sp_btn_defs.push((
                        "[ OAuth Login ]".into(),
                        Style::default().fg(Color::Cyan),
                        Some(ButtonAction::SpotifyLoginPrompt),
                    ));
                    sp_btn_defs.push((
                        "[ Dashboard ]".into(),
                        Style::default().fg(Color::Gray),
                        Some(ButtonAction::SpotifyDashboard),
                    ));
                }
                let sp_btn_line = render_inline_buttons(sp_btn_defs, sp_rows[0], 1, &mut app.hits.buttons);
                let sp_bar = Paragraph::new(sp_btn_line).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Green))
                        .title(" Spotify Linking "),
                );
                f.render_widget(sp_bar, sp_rows[0]);

                app.hits.settings_list = sp_rows[1];
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
                            let downloaded = find_cached_track(&app.lib, &tr.title, &tr.artist).is_some();
                            let mut spans = vec![
                                Span::styled(format!("[{:>5}] ", dur), Style::default().fg(Color::Rgb(118, 238, 148))),
                                Span::styled(format!("{} ", tr.title), Style::default().fg(Color::Rgb(244, 244, 246))),
                                Span::styled(format!("- {}", tr.artist), Style::default().fg(Color::Cyan)),
                            ];
                            if downloaded {
                                spans.push(Span::styled(
                                    " [DOWNLOADED]",
                                    Style::default().fg(Color::Rgb(118, 238, 148)),
                                ));
                            }
                            ListItem::new(Line::from(spans))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(format!(" Spotify: {} ({}) ", col.name, app.spotify_tracks.len())),
                        )
                        .highlight_style(
                            Style::default()
                                .bg(Color::Rgb(46, 52, 62))
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("+ ");
                    f.render_stateful_widget(list, sp_rows[1], &mut app.spotify_track_state);
                } else if app.spotify_cols.is_empty() {
                    let help = format!(
                        "\n  Link any public Spotify playlist or album URL (or Exportify .csv /\n  Spotify .json) using [ + Link URL / CSV ] -- no Premium required.\n\n  For Spotify OAuth (Premium): set Redirect URI to\n  {SPOTIFY_REDIRECT_URI} in Dashboard, then click [ OAuth Login ]."
                    );
                    let p = Paragraph::new(help)
                        .style(Style::default().fg(Color::Rgb(155, 160, 170)))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::DarkGray))
                                .title(" Linked Spotify Playlists "),
                        );
                    f.render_widget(p, sp_rows[1]);
                } else {
                    let items: Vec<ListItem> = app
                        .spotify_cols
                        .iter()
                        .map(|c| {
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    format!("{} ", c.name),
                                    Style::default().fg(Color::Rgb(244, 244, 246)).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!("({}) - {}", c.track_count, c.subtitle),
                                    Style::default().fg(Color::Rgb(155, 160, 170)),
                                ),
                            ]))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Green))
                                .title(" Linked Spotify Playlists "),
                        )
                        .highlight_style(
                            Style::default()
                                .bg(Color::Rgb(46, 52, 62))
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, sp_rows[1], &mut app.spotify_col_state);
                }

                // Section 3: Keybinds reference inside Settings
                if let Some(kb_area) = keybinds_opt {
                    let kb_rows = vec![
                        ("Playback", None),
                        ("Space", Some("Play / Pause")),
                        ("Enter / Click", Some("Play or download song")),
                        ("n / b", Some("Next / Previous track")),
                        ("Left / Right", Some("Seek -5s / +5s (or click bar)")),
                        ("+ / -", Some("Volume up / down")),
                        ("z / r", Some("Shuffle / Repeat mode")),
                        ("", None),
                        ("Navigation & Search", None),
                        ("/ or s", Some("Focus Search bar")),
                        ("D", Some("Toggle Downloads dropdown")),
                        ("A / Click Name", Some("Open Artist Profile")),
                        ("Click Album", Some("Open Album / EP / Single")),
                        ("1-5", Some("Switch Discography / Search tabs")),
                        ("f", Some("Like / Follow Artist")),
                        ("a", Some("Import All (Album/Artist/Spotify)")),
                        (", (comma)", Some("Open Settings")),
                        ("Tab / Esc", Some("Switch pane / Close / Back")),
                        ("", None),
                        ("Library & Lyrics", None),
                        ("Click Lyric", Some("Seek to lyric timestamp")),
                        ("? / Click [?]", Some("Genius lyric explanation")),
                        ("l", Some("Like song (pin from cache)")),
                        ("L", Some("Toggle live lyrics")),
                        ("[ / ] / \\", Some("Lyrics sync -50ms / +50ms / 0")),
                        ("d", Some("Download (Search/Artist) / Delete")),
                        ("Q / N / P", Some("Queue / New / Add to Playlist")),
                        ("i / p / c", Some("Import / Link Spotify / Prune")),
                        ("q", Some("Quit itmp")),
                    ];
                    let lines: Vec<Line<'static>> = kb_rows
                        .into_iter()
                        .map(|(k, desc)| match desc {
                            None if k.is_empty() => Line::from(""),
                            None => Line::from(Span::styled(
                                format!(" {k}"),
                                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                            )),
                            Some(d) => Line::from(vec![
                                Span::styled(
                                    format!(" {:<15}", k),
                                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(d.to_string(), Style::default().fg(Color::Rgb(185, 190, 200))),
                            ]),
                        })
                        .collect();
                    let kb_widget = Paragraph::new(lines).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray))
                            .title(" Keybinds "),
                    );
                    f.render_widget(kb_widget, kb_area);
                }
            } else {
                // Library Track Views: All Tracks / Liked Songs / Queue / Playlist
                let pane_title = match &app.view {
                    View::AllTracks => format!(" All Tracks ({}) ", app.track_list.len()),
                    View::Liked => format!(" * Liked Songs ({}) ", app.track_list.len()),
                    View::Queue => {
                        let np = app
                            .current_track_entry()
                            .map(|e| format!(" | Now: {}", e.title))
                            .unwrap_or_default();
                        format!(" Up Next Queue ({}){} ", app.track_list.len(), np)
                    }
                    View::Playlist(idx) => app
                        .lib
                        .playlists
                        .get(*idx)
                        .map(|p| format!(" {} ({}) ", p.name, p.tracks.len()))
                        .unwrap_or_else(|| " Playlist ".into()),
                    _ => " Tracks ".into(),
                };

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
                            clean_artist_name(&e.artist),
                            e.album.clone(),
                            e.liked,
                            has_lrc(&app.dir, fname),
                            e.duration_sec,
                            e.size_bytes,
                            app.cover_path_for(fname),
                        ))
                    })
                    .collect();

                let title_max = main_area.width.saturating_sub(18).clamp(14, 38) as usize;
                let scroll_off = app.track_state.offset();
                let (mx, my) = app.mouse_pos;
                let max_y = main_area.y + main_area.height.saturating_sub(1);
                let items: Vec<ListItem> = row_data
                    .into_iter()
                    .enumerate()
                    .map(|(i, (fname, title, artist, album, liked, lrc_ok, dur_sec, size_b, cov_p))| {
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
                            line1_spans.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                            line2_spans.push(Span::styled("    ", Style::default().bg(Color::Rgb(42, 44, 50))));
                        }
                        line1_spans.push(Span::raw(" "));
                        line2_spans.push(Span::raw(" "));

                        let is_cur = cur_playing.as_deref() == Some(fname.as_str());
                        let is_queue_view = matches!(app.view, View::Queue);
                        let is_manual_queued = is_queue_view && i < app.queue.len();
                        let status_tag = if is_queue_view {
                            format!("{:>2}.", i + 1)
                        } else if is_cur {
                            if is_paused { "|| ".into() } else { ">  ".into() }
                        } else if liked {
                            "*  ".into()
                        } else {
                            "   ".into()
                        };
                        let tag_style = if is_manual_queued {
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                        } else if is_queue_view {
                            Style::default().fg(Color::Rgb(135, 140, 150))
                        } else if is_cur {
                            Style::default().fg(Color::Rgb(118, 238, 148)).add_modifier(Modifier::BOLD)
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
                        if is_queue_view {
                            line1_spans.push(Span::raw(" "));
                        }
                        line1_spans.push(Span::styled(
                            t_disp,
                            if is_cur {
                                Style::default().fg(Color::Rgb(118, 238, 148)).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::Rgb(244, 244, 246))
                            },
                        ));
                        if is_manual_queued {
                            line1_spans.push(Span::styled(
                                " [QUEUED]",
                                Style::default().fg(Color::Yellow),
                            ));
                        }

                        let dur_s = if dur_sec > 0 {
                            fmt_time(dur_sec as f64)
                        } else {
                            "--:--".to_string()
                        };
                        let lrc_s = if lrc_ok { "LRC" } else { "---" };

                        line2_spans.push(Span::raw("   "));
                        let row_y2_opt = if i >= scroll_off {
                            let y2 = main_area.y + 1 + ((i - scroll_off) * 2) as u16 + 1;
                            (y2 < max_y).then_some(y2)
                        } else {
                            None
                        };
                        // border(1) + highlight(2) + thumb(4) + space(1) + indent(3) = +11
                        let mut cur_x = main_area.x + 11;

                        if !artist.is_empty() {
                            let mut a_style = Style::default().fg(Color::Cyan);
                            let a_len = artist.chars().count() as u16;
                            if let Some(y2) = row_y2_opt {
                                let arect = Rect::new(cur_x, y2, a_len, 1);
                                if rect_contains(arect, mx, my) {
                                    a_style = a_style.add_modifier(Modifier::UNDERLINED);
                                }
                                app.hits.artist_links.push((arect, artist.clone()));
                            }
                            line2_spans.push(Span::styled(artist.clone(), a_style));
                            cur_x += a_len;

                            if !album.is_empty() {
                                line2_spans.push(Span::styled(" - ", Style::default().fg(Color::Rgb(140, 145, 155))));
                                cur_x += 3;
                                let mut al_style = Style::default().fg(Color::Rgb(165, 172, 185));
                                let al_len = album.chars().count() as u16;
                                if let Some(y2) = row_y2_opt {
                                    let alrect = Rect::new(cur_x, y2, al_len, 1);
                                    if rect_contains(alrect, mx, my) {
                                        al_style = al_style.add_modifier(Modifier::UNDERLINED);
                                    }
                                    app.hits.album_links.push((alrect, artist.clone(), album.clone()));
                                }
                                line2_spans.push(Span::styled(album, al_style));
                            }
                            line2_spans.push(Span::styled(
                                format!(" | {} | {}", dur_s, lrc_s),
                                Style::default().fg(Color::Rgb(140, 145, 155)),
                            ));
                        } else {
                            line2_spans.push(Span::styled(
                                format!("{} | {} | {}", dur_s, lrc_s, fmt_mb(size_b)),
                                Style::default().fg(Color::Rgb(155, 160, 170)),
                            ));
                        }

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
                            .bg(Color::Rgb(46, 52, 62))
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("| ");
                f.render_stateful_widget(list, main_area, &mut app.track_state);
            }

            // Live Word-by-Word Synced Lyrics panel (hover + click-to-seek + [?] Genius)
            if let Some(lrc_area) = lyrics_area_opt {
                app.hits.lyrics_area = lrc_area;
                let lrc_title = app
                    .current_track_entry()
                    .map(|e| {
                        if e.lrc_offset_ms != 0 {
                            format!(" Lyrics - {} [{:+.2}s] ", e.title, e.lrc_offset_ms as f64 / 1000.0)
                        } else {
                            format!(" Lyrics - {} ", e.title)
                        }
                    })
                    .unwrap_or_else(|| " Lyrics ".to_string());
                let (lrc_lines, lrc_rows, lrc_helps) = render_word_synced_lyrics(&app, lrc_area);
                app.hits.lyrics_rows = lrc_rows;
                app.hits.lyric_help_btns = lrc_helps;
                let lrc_widget = Paragraph::new(lrc_lines)
                    .alignment(Alignment::Center)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray))
                            .title(lrc_title),
                    );
                f.render_widget(lrc_widget, lrc_area);

                // Floating [ Return ] button appears right after you scroll away from current lyrics!
                if app.lyrics_scroll_offset != 0 && lrc_area.width >= 16 && lrc_area.height >= 5 {
                    let bw = 12u16;
                    let bx = lrc_area.x + (lrc_area.width.saturating_sub(bw)) / 2;
                    let by = lrc_area.y + lrc_area.height.saturating_sub(2);
                    let brect = Rect::new(bx, by, bw, 1);
                    let hov = rect_contains(brect, app.mouse_pos.0, app.mouse_pos.1);
                    f.render_widget(Clear, brect);
                    let ret_btn = Paragraph::new(" [ Return ] ")
                        .alignment(Alignment::Center)
                        .style(if hov {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Rgb(118, 238, 148))
                                .add_modifier(Modifier::BOLD)
                        });
                    app.hits.buttons.push((brect, ButtonAction::LyricsReturn));
                    f.render_widget(ret_btn, brect);
                }
            }

            // 3. Now Playing Progress Bar (high-contrast crisp white text!)
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
                    format!(" - {}", clean_artist_name(&e.artist))
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
                "Idle".to_string()
            };
            let np_gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title(" Now Playing "))
                .gauge_style(
                    Style::default()
                        .fg(Color::Rgb(68, 152, 92))
                        .bg(Color::Rgb(36, 38, 44)),
                )
                .ratio(np_ratio)
                .label(Span::styled(
                    np_label,
                    Style::default()
                        .fg(Color::Rgb(248, 248, 250))
                        .add_modifier(Modifier::BOLD),
                ));
            f.render_widget(np_gauge, outer[2]);

            // 4. Clean Simplistic Clickable Controls Bar
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
                    "[ Artist ]".into(),
                    Style::default().fg(Color::Cyan),
                    Some(ButtonAction::OpenCurrentArtist),
                ),
                (
                    "[ Queue ]".into(),
                    if matches!(app.view, View::Queue) {
                        Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    },
                    Some(ButtonAction::OpenQueue),
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

            let ctrl_line = render_inline_buttons(btn_defs, outer[3], 1, &mut app.hits.buttons);
            let show_transient = app.busy || (!app.status.is_empty() && app.status_at.elapsed() < Duration::from_secs(4));
            let ctrl_title = if show_transient {
                format!(" Controls - {} ", app.status)
            } else {
                " Controls ".to_string()
            };
            let ctrl_bar = Paragraph::new(ctrl_line)
                .block(Block::default().borders(Borders::ALL).title(ctrl_title));
            f.render_widget(ctrl_bar, outer[3]);

            // --- FLOATING OVERLAYS (overlap all UI underneath via Clear) ---

            // Overlay 1: Downloads Dropdown directly under top [ v DL ] button
            if app.show_downloads_menu {
                let recent = app.recent_downloaded_tracks(10);
                let dd_w = 50u16.min(f.area().width.saturating_sub(2));
                let busy_rows = if app.busy { 2u16 } else { 0 };
                let dd_h = ((recent.len().max(1) as u16) + busy_rows + 3).min(f.area().height.saturating_sub(4));
                let dd_x = (top_cols[3].x + top_cols[3].width)
                    .saturating_sub(dd_w)
                    .min(f.area().width.saturating_sub(dd_w));
                let dd_y = outer[0].y + outer[0].height;
                let dd_rect = Rect::new(dd_x, dd_y, dd_w, dd_h);
                app.hits.dl_dropdown = dd_rect;

                f.render_widget(Clear, dd_rect);
                let mut lines: Vec<Line<'static>> = Vec::new();
                let mut cur_y = dd_rect.y + 1;
                if app.busy {
                    lines.push(Line::from(Span::styled(
                        format!(" > {}", app.status),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::styled(
                        "─".repeat(dd_rect.width.saturating_sub(2) as usize),
                        Style::default().fg(Color::DarkGray),
                    )));
                    cur_y += 2;
                }
                if recent.is_empty() {
                    lines.push(Line::from(Span::styled(
                        " No downloaded songs yet",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    let inner_w = dd_rect.width.saturating_sub(2) as usize;
                    let (mx, my) = app.mouse_pos;
                    for (fname, title, artist, sz) in recent {
                        if cur_y >= dd_rect.y + dd_rect.height.saturating_sub(1) {
                            break;
                        }
                        let sz_s = fmt_mb(sz);
                        let right_part = format!(" {sz_s} [x]");
                        let max_left = inner_w.saturating_sub(right_part.chars().count() + 1);
                        let mut label = if artist.is_empty() {
                            title
                        } else {
                            format!("{title} - {artist}")
                        };
                        if label.chars().count() > max_left {
                            label = label
                                .chars()
                                .take(max_left.saturating_sub(2))
                                .collect::<String>()
                                + "..";
                        }
                        let pad = max_left.saturating_sub(label.chars().count());
                        let del_x = dd_rect.x + 1 + (inner_w.saturating_sub(3)) as u16;
                        let del_rect = Rect::new(del_x, cur_y, 3, 1);
                        let play_rect = Rect::new(
                            dd_rect.x + 1,
                            cur_y,
                            dd_rect.width.saturating_sub(5),
                            1,
                        );
                        let row_hov = rect_contains(play_rect, mx, my);
                        let del_hov = rect_contains(del_rect, mx, my);
                        app.hits.dl_play_rows.push((play_rect, fname.clone()));
                        app.hits.dl_del_btns.push((del_rect, fname));

                        lines.push(Line::from(vec![
                            Span::styled(
                                format!(" {label}{}", " ".repeat(pad)),
                                if row_hov {
                                    Style::default()
                                        .fg(Color::Rgb(118, 238, 148))
                                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                                } else {
                                    Style::default().fg(Color::Rgb(235, 237, 242))
                                },
                            ),
                            Span::styled(
                                format!(" {sz_s} "),
                                Style::default().fg(Color::Rgb(125, 130, 140)),
                            ),
                            Span::styled(
                                "[x]",
                                if del_hov {
                                    Style::default()
                                        .fg(Color::Black)
                                        .bg(Color::Red)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(Color::Red)
                                },
                            ),
                        ]));
                        cur_y += 1;
                    }
                }
                let dd_widget = Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .style(Style::default().bg(Color::Rgb(20, 22, 26)))
                        .title(" Recent Downloads "),
                );
                f.render_widget(dd_widget, dd_rect);
            }

            // Overlay 2: Floating Input Modal (only appears when actively entering Import / Playlist / Spotify URL)
            if matches!(
                app.input_mode,
                InputMode::Import
                    | InputMode::SpotifyClientId
                    | InputMode::SpotifyAddUrl
                    | InputMode::NewPlaylist
            ) {
                let mw = 68u16.min(f.area().width.saturating_sub(4));
                let mh = 5u16;
                let mx = f.area().x + (f.area().width.saturating_sub(mw)) / 2;
                let my = f.area().y + (f.area().height.saturating_sub(mh)) / 2;
                let modal_rect = Rect::new(mx, my, mw, mh);
                f.render_widget(Clear, modal_rect);
                let (title, prompt, color) = match &app.input_mode {
                    InputMode::Import => (
                        " Import Music ",
                        format!(" URL, Exportify .csv, or path: {}_", app.input_buf),
                        Color::Cyan,
                    ),
                    InputMode::SpotifyClientId => (
                        " Spotify OAuth Login ",
                        format!(" Client ID ({SPOTIFY_REDIRECT_URI}): {}_", app.input_buf),
                        Color::Green,
                    ),
                    InputMode::SpotifyAddUrl => (
                        " Link Spotify Playlist / Album ",
                        format!(" Spotify URL or .csv/.json path: {}_", app.input_buf),
                        Color::Green,
                    ),
                    InputMode::NewPlaylist => (
                        " New Playlist ",
                        format!(" Name: {}_", app.input_buf),
                        Color::Yellow,
                    ),
                    _ => ("", String::new(), Color::White),
                };
                let modal = Paragraph::new(vec![Line::from(""), Line::from(prompt)])
                    .style(Style::default().fg(color).bg(Color::Rgb(20, 22, 26)))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(color))
                            .title(title),
                    );
                f.render_widget(modal, modal_rect);
            }

            // Overlay 3: Genius Lyric Explanation Popup
            if let Some(pop) = &app.genius_popup {
                let pw = 68u16.min(f.area().width.saturating_sub(4));
                let ph = 18u16.min(f.area().height.saturating_sub(4));
                let px = f.area().x + (f.area().width.saturating_sub(pw)) / 2;
                let py = f.area().y + (f.area().height.saturating_sub(ph)) / 2;
                let prect = Rect::new(px, py, pw, ph);
                app.hits.popup_rect = prect;
                f.render_widget(Clear, prect);

                let close_rect = Rect::new(px + pw.saturating_sub(12), py, 10, 1);
                app.hits.buttons.push((close_rect, ButtonAction::ClosePopup));

                let mut plines = vec![
                    Line::from(Span::styled(
                        format!(" \"{}\"", pop.lyric_line),
                        Style::default()
                            .fg(Color::Rgb(118, 238, 148))
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                ];
                if !pop.fragment.is_empty() && pop.fragment != pop.lyric_line {
                    plines.push(Line::from(Span::styled(
                        format!(" Genius bar: {}", pop.fragment.replace('\n', " / ")),
                        Style::default().fg(Color::Yellow),
                    )));
                    plines.push(Line::from(""));
                }
                for para in pop.explanation.lines() {
                    plines.push(Line::from(Span::styled(
                        format!(" {para}"),
                        Style::default().fg(Color::Rgb(230, 232, 238)),
                    )));
                }
                if !pop.url.is_empty() {
                    plines.push(Line::from(""));
                    plines.push(Line::from(Span::styled(
                        format!(" {}", pop.url),
                        Style::default().fg(Color::DarkGray),
                    )));
                }

                let pwidget = Paragraph::new(plines)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(Color::Rgb(20, 22, 26)))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(format!(" Genius - {} [ Esc / Close ] ", pop.song_title)),
                    );
                f.render_widget(pwidget, prect);
            }
        })?;

        let poll_ms = if app.pb.active && !app.pb.paused { 16 } else { 45 };
        if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                Event::Mouse(me) => app.handle_mouse(me.kind, me.column, me.row),
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        break;
                    }
                    // Dismiss Genius popup or Downloads dropdown cleanly on Esc
                    if key.code == KeyCode::Esc {
                        if app.genius_popup.is_some() {
                            app.genius_popup = None;
                            continue;
                        }
                        if app.show_downloads_menu {
                            app.show_downloads_menu = false;
                            continue;
                        }
                    }
                    if app.genius_popup.is_some() && matches!(key.code, KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('?')) {
                        app.genius_popup = None;
                        continue;
                    }
                    match &app.input_mode {
                        InputMode::Search => match key.code {
                            KeyCode::Esc => {
                                app.input_buf.clear();
                                if !app.search_results.is_empty() || !app.search_albums_list.is_empty() {
                                    app.input_mode = InputMode::SearchResults;
                                } else {
                                    app.view = View::AllTracks;
                                    app.sidebar_state.select(Some(0));
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
                                app.sidebar_state.select(Some(0));
                                app.input_mode = InputMode::Normal;
                                app.rebuild_track_list();
                                app.set_notice("Returned to All Tracks");
                            }
                            KeyCode::Char('/') | KeyCode::Char('s') => {
                                app.trigger_button(ButtonAction::SearchBtn);
                            }
                            KeyCode::Char(',') => app.open_settings_view(),
                            KeyCode::Char(' ') => app.trigger_button(ButtonAction::PlayPause),
                            KeyCode::Char('A') => app.open_selected_or_playing_artist(),
                            KeyCode::Char('1') => app.trigger_button(ButtonAction::SearchTab(SearchTab::Songs)),
                            KeyCode::Char('2') => app.trigger_button(ButtonAction::SearchTab(SearchTab::AllReleases)),
                            KeyCode::Char('3') => app.trigger_button(ButtonAction::SearchTab(SearchTab::Albums)),
                            KeyCode::Char('4') => app.trigger_button(ButtonAction::SearchTab(SearchTab::Eps)),
                            KeyCode::Char('5') => app.trigger_button(ButtonAction::SearchTab(SearchTab::Singles)),
                            KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                            KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                            KeyCode::Enter => {
                                if app.search_tab == SearchTab::Songs {
                                    app.start_download_search_result(true);
                                } else {
                                    let albums = app.filtered_search_albums();
                                    if let Some(i) = app.search_album_state.selected() {
                                        if let Some(al) = albums.get(i) {
                                            app.open_album_by_id(al.id, &al.title);
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('d') => {
                                if app.search_tab == SearchTab::Songs {
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
                                            app.status = format!("Added to playlist '{pl_name}'");
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
                            KeyCode::PageUp => app.scroll_lyrics(-3),
                            KeyCode::PageDown => app.scroll_lyrics(3),
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
                            KeyCode::Char(',') => app.open_settings_view(),
                            KeyCode::Char('D') => app.trigger_button(ButtonAction::ToggleDownloads),
                            KeyCode::Char('?') => {
                                let off = app
                                    .current_track_entry()
                                    .map(|e| e.lrc_offset_ms as f64 / 1000.0)
                                    .unwrap_or(0.0);
                                let pos = app.smooth_pos + off;
                                if let Some(idx) = app.lrc_lines.iter().rposition(|l| l.time_sec <= pos) {
                                    app.open_genius_for_lyric(idx);
                                }
                            }
                            KeyCode::Char('i') => app.trigger_button(ButtonAction::Import),
                            KeyCode::Char('/') | KeyCode::Char('s') => app.trigger_button(ButtonAction::SearchBtn),
                            KeyCode::Char('l') => app.trigger_button(ButtonAction::Like),
                            KeyCode::Char('A') => app.open_selected_or_playing_artist(),
                            KeyCode::Char('f') if matches!(app.view, View::Artist) => {
                                app.toggle_like_current_artist();
                            }
                            KeyCode::Char('a') if matches!(app.view, View::Artist) => {
                                app.start_artist_import_all();
                            }
                            KeyCode::Char('a') if matches!(app.view, View::Album) => {
                                app.start_album_import_all();
                            }
                            KeyCode::Char('1') if matches!(app.view, View::Artist) => {
                                app.trigger_button(ButtonAction::ArtistTab(ArtistTab::TopTracks));
                            }
                            KeyCode::Char('2') if matches!(app.view, View::Artist) => {
                                app.trigger_button(ButtonAction::ArtistTab(ArtistTab::All));
                            }
                            KeyCode::Char('3') if matches!(app.view, View::Artist) => {
                                app.trigger_button(ButtonAction::ArtistTab(ArtistTab::Albums));
                            }
                            KeyCode::Char('4') if matches!(app.view, View::Artist) => {
                                app.trigger_button(ButtonAction::ArtistTab(ArtistTab::Eps));
                            }
                            KeyCode::Char('5') if matches!(app.view, View::Artist) => {
                                app.trigger_button(ButtonAction::ArtistTab(ArtistTab::Singles));
                            }
                            KeyCode::Char('1') if matches!(app.view, View::Search) => {
                                app.trigger_button(ButtonAction::SearchTab(SearchTab::Songs));
                            }
                            KeyCode::Char('2') if matches!(app.view, View::Search) => {
                                app.trigger_button(ButtonAction::SearchTab(SearchTab::AllReleases));
                            }
                            KeyCode::Char('3') if matches!(app.view, View::Search) => {
                                app.trigger_button(ButtonAction::SearchTab(SearchTab::Albums));
                            }
                            KeyCode::Char('4') if matches!(app.view, View::Search) => {
                                app.trigger_button(ButtonAction::SearchTab(SearchTab::Eps));
                            }
                            KeyCode::Char('5') if matches!(app.view, View::Search) => {
                                app.trigger_button(ButtonAction::SearchTab(SearchTab::Singles));
                            }
                            KeyCode::Char('p') if matches!(app.view, View::Settings) => {
                                app.trigger_button(ButtonAction::SpotifyAddUrl);
                            }
                            KeyCode::Char('c') if matches!(app.view, View::Settings) && !app.spotify.is_logged_in() => {
                                app.trigger_button(ButtonAction::SpotifyLoginPrompt);
                            }
                            KeyCode::Char('o') if matches!(app.view, View::Settings) => {
                                app.trigger_button(ButtonAction::SpotifyDashboard);
                            }
                            KeyCode::Char('R') if matches!(app.view, View::Settings) && app.spotify.is_logged_in() => {
                                app.trigger_button(ButtonAction::SpotifyRefresh);
                            }
                            KeyCode::Char('X') if matches!(app.view, View::Settings) && app.spotify.is_logged_in() => {
                                app.trigger_button(ButtonAction::SpotifyLogout);
                            }
                            KeyCode::Char('a')
                                if matches!(app.view, View::Settings) && app.spotify_open_col.is_some() =>
                            {
                                app.start_spotify_import_all();
                            }
                            KeyCode::Esc | KeyCode::Backspace => {
                                if matches!(app.view, View::Album) {
                                    app.trigger_button(ButtonAction::AlbumBack);
                                } else if matches!(app.view, View::Settings) && app.spotify_open_col.is_some() {
                                    app.spotify_open_col = None;
                                    app.spotify_tracks.clear();
                                    app.status = "Back to Settings.".into();
                                } else if matches!(app.view, View::Artist | View::Settings) {
                                    app.trigger_button(ButtonAction::Home);
                                }
                            }
                            KeyCode::Enter => {
                                if app.focus == Focus::Sidebar {
                                    if let Some(idx) = app.sidebar_state.selected() {
                                        app.activate_sidebar_index(idx);
                                        app.focus = Focus::Main;
                                    }
                                } else if matches!(app.view, View::Artist) {
                                    if app.artist_tab == ArtistTab::TopTracks {
                                        if let Some(i) = app.artist_track_state.selected() {
                                            app.start_artist_download_track(i, true);
                                        }
                                    } else {
                                        let albums = app.filtered_artist_albums();
                                        if let Some(i) = app.artist_album_state.selected() {
                                            if let Some(al) = albums.get(i) {
                                                app.open_album_by_id(al.id, &al.title);
                                            }
                                        }
                                    }
                                } else if matches!(app.view, View::Album) {
                                    if let Some(i) = app.album_track_state.selected() {
                                        app.start_album_download_track(i, true);
                                    }
                                } else if matches!(app.view, View::Search) {
                                    if app.search_tab == SearchTab::Songs {
                                        app.start_download_search_result(true);
                                    } else {
                                        let albums = app.filtered_search_albums();
                                        if let Some(i) = app.search_album_state.selected() {
                                            if let Some(al) = albums.get(i) {
                                                app.open_album_by_id(al.id, &al.title);
                                            }
                                        }
                                    }
                                } else if matches!(app.view, View::Settings) {
                                    if app.spotify_open_col.is_some() {
                                        if let Some(i) = app.spotify_track_state.selected() {
                                            app.start_spotify_download_track(i, true);
                                        }
                                    } else if let Some(i) = app.spotify_col_state.selected() {
                                        app.start_spotify_open_collection(i);
                                    } else {
                                        app.trigger_button(ButtonAction::SpotifyAddUrl);
                                    }
                                } else if let Some(fname) = app.selected_track() {
                                    if matches!(app.view, View::Queue) {
                                        if let Some(i) = app.track_state.selected() {
                                            app.play_from_queue_index(i);
                                        }
                                    } else {
                                        app.play_filename(&fname, true);
                                    }
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
                                    if matches!(app.view, View::Queue) {
                                        app.rebuild_track_list();
                                    }
                                    app.set_notice(format!("Queued next: {t}"));
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
                                if matches!(app.view, View::Artist) {
                                    if app.artist_tab == ArtistTab::TopTracks {
                                        if let Some(i) = app.artist_track_state.selected() {
                                            app.start_artist_download_track(i, false);
                                        }
                                    }
                                } else if matches!(app.view, View::Album) {
                                    if let Some(i) = app.album_track_state.selected() {
                                        app.start_album_download_track(i, false);
                                    }
                                } else if matches!(app.view, View::Settings) {
                                    if app.spotify_open_col.is_some() {
                                        if let Some(i) = app.spotify_track_state.selected() {
                                            app.start_spotify_download_track(i, false);
                                        }
                                    } else if let Some(i) = app.spotify_col_state.selected() {
                                        if let Some(SpotifyCollection {
                                            kind: SpotifyCollectionKind::PublicUrl { url },
                                            name,
                                            ..
                                        }) = app.spotify_cols.get(i).cloned()
                                        {
                                            app.spotify.saved_links.retain(|l| l.url != url);
                                            let _ = save_spotify_session(&app.spotify);
                                            app.spotify_cols.remove(i);
                                            if app.spotify_cols.is_empty() {
                                                app.spotify_col_state.select(None);
                                            } else {
                                                app.spotify_col_state.select(Some(i.min(app.spotify_cols.len() - 1)));
                                            }
                                            app.status = format!("Unlinked Spotify playlist '{name}'");
                                        }
                                    }
                                } else if matches!(app.view, View::Queue) {
                                    if let Some(sel) = app.track_state.selected() {
                                        if sel < app.queue.len() {
                                            app.queue.remove(sel);
                                        } else {
                                            let u_idx = sel - app.queue.len();
                                            if u_idx < app.upcoming.len() {
                                                app.upcoming.remove(u_idx);
                                            }
                                        }
                                        app.rebuild_track_list();
                                        app.set_notice("Removed from queue");
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
                            KeyCode::Char('c') => app.trigger_button(ButtonAction::PruneCache),
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

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let env_max_mb: Option<u64> = env::var("ITMP_CACHE_MB").ok().and_then(|v| v.parse().ok());
    let dir = audio_dir();

    if args.is_empty() {
        if let Err(e) = run_tui(env_max_mb) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let max_mb = env_max_mb
        .or_else(|| load_and_sync_library(&dir).max_cache_mb)
        .unwrap_or(DEFAULT_MAX_MB);
    let max_bytes = max_mb * 1_048_576;

    match args[0].as_str() {
        "import" => {
            let raw = args[1..].join(" ");
            if raw.is_empty() {
                eprintln!("Usage: itmp import <spotify-url | youtube-url | local-file-or-folder | export.csv>");
                std::process::exit(1);
            }
            if expand_local_path(&raw).exists() {
                let p = expand_local_path(&raw);
                if let Some(bundle) = parse_spotify_export_file(&p) {
                    println!(
                        "Importing '{}' ({} tracks)...",
                        bundle.collection_name,
                        bundle.tracks.len()
                    );
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
                            None,
                            tr.duration_sec,
                            Some(&bundle.collection_name),
                            max_bytes,
                        ) {
                            Ok((_, msg)) => println!("{msg}"),
                            Err(e) => println!("FAIL ({e})"),
                        }
                    }
                } else {
                    match import_local_music(&dir, &raw, max_bytes) {
                        Ok(m) => println!("{m}"),
                        Err(e) => {
                            eprintln!("error: {e}");
                            std::process::exit(1);
                        }
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
                "Total: {} / {} ({} tracks, {} playlists, {} liked artists)",
                fmt_mb(total_cache_bytes(&lib)),
                fmt_mb(max_bytes),
                entries.len(),
                lib.playlists.len(),
                lib.liked_artists.len()
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
                    println!("Spotify: not logged in (open Settings -> Link Spotify URL/CSV or run `itmp spotify login <client-id>`)");
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
