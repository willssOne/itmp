mod art;
mod artist;
mod cache;
mod censor;
mod downloader;
mod genius;
mod lyrics;
mod names;
mod player;
mod repair;
mod spotify;
mod sync;
mod tempo;
mod theme;
mod ui;

use artist::{
    fetch_album_detail, fetch_artist_profile, find_and_fetch_album_by_name,
    parse_spotify_artist_id, search_albums, AlbumDetail, AlbumSummary, ArtistProfile,
};
use cache::{
    add_track_to_playlist, audio_dir, clean_artist_name, edit_library, fmt_mb, fmt_time, has_lrc,
    load_and_sync_library, now_ts, prune_lru, remove_track_files, total_cache_bytes, Library,
    LikedArtist, Playlist, TrackEntry, DEFAULT_MAX_MB,
};
use art::{fetch_remote_image_to_temp, Art};
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
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    ThumbReady(String, PathBuf),
    DownloadDone(Box<Job>, Result<(String, String), String>), // (filename, msg)
    ImportDone(Result<String, String>),
    Jobs(String, Vec<Job>),
    Repaired(usize),
    Synced(String, Result<(u64, u64), String>),
    LyricsFetched(String, bool),
    LyricsAligned(String, Vec<LrcLine>),
    SpotifyLoginDone(Result<SpotifySession, String>),
    SpotifyCollectionsDone(Result<Vec<SpotifyCollection>, String>),
    SpotifyTracksDone(Result<(SpotifyCollection, Vec<SpotifyTrackSpec>), String>),
    SpotifyPublicUrlAdded(Result<(SavedSpotifyLink, Vec<SpotifyTrackSpec>), String>),
    ArtistLoaded(Result<(ArtistProfile, Option<PathBuf>), String>),
    AlbumLoaded(Result<(AlbumDetail, Option<PathBuf>), String>),
    GeniusLoaded(String, String, Result<GeniusSongAnnotations, String>), // (song_key, lyric_line, res)
}

/// One song waiting to be downloaded.
#[derive(Debug, Clone, Default)]
struct Job {
    id: u64,
    /// A watch URL, or `ytsearch1:...` to let the downloader pick the best upload.
    source: String,
    title: String,
    artist: String,
    album: String,
    cover: Option<String>,
    duration_sec: u64,
    playlist: Option<String>,
    liked: bool,
    play: bool,
}

impl Job {
    fn from_spec(tr: &SpotifyTrackSpec, cover: Option<&str>) -> Self {
        Job {
            source: "ytsearch1:".into(),
            title: tr.title.clone(),
            artist: tr.artist.clone(),
            album: tr.album.clone(),
            cover: tr.cover_url.clone().or_else(|| cover.map(str::to_string)),
            duration_sec: tr.duration_sec,
            ..Job::default()
        }
    }

    fn from_upload(item: &SearchResult) -> Self {
        let (title, artist) = item.song_name();
        Job {
            source: format!("https://www.youtube.com/watch?v={}", item.id),
            title,
            artist,
            // The search thumbnail is tiny; the download supplies a large one.
            cover: None,
            duration_sec: item.seconds(),
            ..Job::default()
        }
    }

    fn label(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} — {}", self.title, self.artist)
        }
    }

    fn run(&self, dir: &std::path::Path, max_bytes: u64) -> Result<(String, String), String> {
        download_and_cache_track(
            dir,
            &self.source,
            &self.title,
            &self.artist,
            &self.album,
            self.cover.as_deref(),
            self.duration_sec,
            self.playlist.as_deref(),
            max_bytes,
        )
    }
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
    ToggleCensor,
    CensorStyle,
    Speed(bool),
    Pitch(bool),
    ToggleTuneLink,
    ResetTune,
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
    modal: Rect,
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
    loaded_at: Instant,
    seen_active: bool,

    theme: theme::Theme,
    theme_stamp: Option<std::time::SystemTime>,
    theme_checked: Instant,

    jobs: VecDeque<Job>,
    active_job: Option<Job>,
    next_job_id: u64,
    /// The download that should start playing when it lands (the latest click wins).
    play_when_ready: Option<u64>,

    aligner: sync::Aligner,
    /// Title key -> (artist, seconds) of everything downloaded, for "saved" marks.
    saved: HashMap<String, Vec<(String, u64)>>,
    saved_ids: HashSet<String>,

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
    art: Art,
    /// Downloaded search / artist / album thumbnails by lookup key.
    thumbs: HashMap<String, PathBuf>,
    censor: censor::Censor,

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
    artist_tab: ArtistTab,
    artist_track_state: ListState,
    artist_album_state: ListState,

    // Album / EP / Single Page
    album_detail: Option<AlbumDetail>,
    album_loading_name: Option<String>,
    album_cover_path: Option<PathBuf>,
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
        let aligner = {
            let tx = tx.clone();
            sync::Aligner::start(dir.clone(), move |file, res| {
                let _ = tx.send(WorkerMsg::Synced(file, res));
            })
        };
        {
            let dir = dir.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let _ = tx.send(WorkerMsg::Repaired(repair::repair_library(&dir)));
            });
        }
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
            loaded_at: now,
            seen_active: false,
            theme: theme::Theme::load(),
            theme_stamp: theme::Theme::stamp(),
            theme_checked: now,
            jobs: VecDeque::new(),
            active_job: None,
            next_job_id: 0,
            play_when_ready: None,
            aligner,
            saved: HashMap::new(),
            saved_ids: HashSet::new(),
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
            art: Art::default(),
            thumbs: HashMap::new(),
            censor: censor::Censor::load(),
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
            artist_tab: ArtistTab::TopTracks,
            artist_track_state: ListState::default(),
            artist_album_state: ListState::default(),
            album_detail: None,
            album_loading_name: None,
            album_cover_path: None,
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
        app.lib.tune = app.lib.tune.sanitized();
        app.player.set_tune(app.lib.tune);
        app.index_saved();
        app.rebuild_track_list();
        app.play_context = app.track_list.clone();
        app.rebuild_upcoming(None);
        app
    }

    fn index_saved(&mut self) {
        self.saved.clear();
        self.saved_ids.clear();
        for e in self.lib.entries.values() {
            self.saved
                .entry(names::title_key(&e.title, &e.artist))
                .or_default()
                .push((e.artist.clone(), e.duration_sec));
            self.saved_ids.extend(e.source_id.clone());
        }
    }

    /// Same rule as `find_cached_track`, but cheap enough to run every frame.
    fn is_saved(&self, title: &str, artist: &str, secs: u64) -> bool {
        self.saved
            .get(&names::title_key(title, artist))
            .map(|found| {
                found.iter().any(|(a, d)| {
                    (artist.trim().is_empty() || names::same_artist(a, artist))
                        && (secs == 0 || *d == 0 || secs.abs_diff(*d) <= 20)
                })
            })
            .unwrap_or(false)
    }

    fn find_saved(&self, job: &Job) -> Option<String> {
        let id = job.source.split_once("watch?v=").map(|(_, id)| id);
        cache::find_cached_track(&self.lib, &job.title, &job.artist, job.duration_sec, id)
    }

    /// Applies a change to the on-disk library and adopts the result.
    fn edit<R>(&mut self, change: impl FnOnce(&mut Library) -> R) -> R {
        let (lib, out) = edit_library(&self.dir, change);
        self.lib = lib;
        self.index_saved();
        out
    }

    /// Plays a song if we have it, otherwise queues its download.
    fn request_track(&mut self, mut job: Job) {
        if let Some(file) = self.find_saved(&job) {
            if job.playlist.is_some() || job.liked {
                self.edit(|lib| {
                    if let Some(name) = &job.playlist {
                        add_track_to_playlist(lib, name, &file);
                    }
                    if let Some(e) = lib.entries.get_mut(&file) {
                        e.liked |= job.liked;
                    }
                });
                self.rebuild_track_list();
            }
            if job.play {
                self.play_filename(&file, true);
            } else {
                self.set_notice(format!("'{}' is already downloaded", job.title));
            }
            return;
        }
        self.next_job_id += 1;
        job.id = self.next_job_id;
        if job.play {
            self.play_when_ready = Some(job.id);
        }
        if let Some(active) = self.active_job.as_ref() {
            if active.source == job.source && active.label() == job.label() {
                if job.play {
                    self.play_when_ready = Some(active.id);
                }
                return;
            }
        }
        let same = |j: &Job| j.source == job.source && j.label() == job.label();
        if let Some(old) = self.jobs.iter().position(same).and_then(|i| self.jobs.remove(i)) {
            job.playlist = job.playlist.or(old.playlist);
            job.liked |= old.liked;
        }
        let waiting = self.active_job.is_some();
        let label = job.label();
        if job.play {
            self.jobs.push_front(job);
        } else {
            self.jobs.push_back(job);
        }
        if waiting {
            self.set_notice(format!("Queued download: {label}"));
        }
        self.start_next_job();
    }

    fn start_next_job(&mut self) {
        if self.active_job.is_some() {
            return;
        }
        let Some(job) = self.jobs.pop_front() else {
            return;
        };
        let left = self.jobs.len();
        self.set_notice(format!(
            "Downloading {}{}",
            job.label(),
            if left > 0 { format!("  (+{left} queued)") } else { String::new() }
        ));
        self.active_job = Some(job.clone());
        let dir = self.dir.clone();
        let max_bytes = self.max_bytes;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = job.run(&dir, max_bytes);
            let _ = tx.send(WorkerMsg::DownloadDone(Box::new(job), res));
        });
    }

    fn lyric_position(&self) -> f64 {
        let nudge = self
            .current_track_entry()
            .map(|e| e.lrc_offset_ms as f64 / 1000.0)
            .unwrap_or(0.0);
        self.smooth_pos + nudge
    }

    /// Throws away this song's lyrics and timing and fetches them again.
    fn resync_lyrics(&mut self) {
        let Some(e) = self.current_track_entry().cloned() else {
            return;
        };
        let dir = self.dir.clone();
        let tx = self.tx.clone();
        self.set_notice(format!("Re-syncing lyrics for '{}'...", e.title));
        self.lrc_lines.clear();
        thread::spawn(move || {
            let words = sync::words_path(&dir, &e.filename);
            let _ = fs::remove_file(&words);
            let lrc = words.with_extension("").with_extension("lrc");
            let backup = lrc.with_extension("lrc.bak");
            let had = fs::rename(&lrc, &backup).is_ok();
            let stem = e.filename.rsplit_once('.').map(|(s, _)| s).unwrap_or(&e.filename);
            let found = downloader::fetch_synced_lrc(&dir, stem, &e.title, &e.artist, e.duration_sec);
            if had {
                if found {
                    let _ = fs::remove_file(&backup);
                } else {
                    let _ = fs::rename(&backup, &lrc);
                }
            }
            let _ = tx.send(WorkerMsg::LyricsFetched(e.filename, found || had));
        });
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
                format!("Liked Songs ({})", liked_count),
                SidebarEntry::Liked,
            ),
            (
                format!("Queue ({})", self.queue.len() + self.upcoming.len()),
                SidebarEntry::Queue,
            ),
        ];

        rows.push(("ARTISTS".to_string(), SidebarEntry::Header));
        if self.lib.liked_artists.is_empty() {
            rows.push(("  none followed".to_string(), SidebarEntry::Header));
        } else {
            for (idx, ar) in self.lib.liked_artists.iter().enumerate() {
                rows.push((format!("  {}", ar.name), SidebarEntry::Artist(idx)));
            }
        }

        rows.push(("PLAYLISTS".to_string(), SidebarEntry::Header));
        for (idx, pl) in self.lib.playlists.iter().enumerate() {
            rows.push((
                format!("  {} ({})", pl.name, pl.tracks.len()),
                SidebarEntry::Playlist(idx),
            ));
        }
        rows.push(("+ New playlist".to_string(), SidebarEntry::NewPlaylist));
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
        self.index_saved();
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
        if !matches!(
            self.view,
            View::AllTracks | View::Liked | View::Queue | View::Playlist(_)
        ) {
            return None;
        }
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
        self.play_when_ready = None;
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
        // Lyrics first, so the filter is in place before the first sample plays.
        self.refresh_lyrics_for(fname);
        match self.player.play_file(&full, fname) {
            Ok(()) => {
                self.was_playing_file = true;
                self.smooth_pos = 0.0;
                self.loaded_at = Instant::now();
                self.last_pb_poll = self.loaded_at;
                self.seen_active = false;
                self.pb = PlaybackState {
                    active: true,
                    ..PlaybackState::default()
                };
                let label = self.edit(|lib| {
                    let e = lib.entries.get_mut(fname)?;
                    e.last_used = now_ts();
                    Some(if e.artist.is_empty() {
                        e.title.clone()
                    } else {
                        format!("{} - {}", e.title, e.artist)
                    })
                });
                if let Some(label) = label {
                    let unfiltered = if self.lib.censor && self.lrc_lines.is_empty() {
                        "  (no lyrics yet: audio not censored)"
                    } else {
                        ""
                    };
                    self.set_notice(format!("Playing: {label}{unfiltered}"));
                }
                if rebuild_up {
                    self.rebuild_upcoming(Some(fname));
                }
                self.prepare_next();
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
        let playing = self.player.current_file.clone();
        let target = if self.focus == Focus::Main {
            self.selected_track().or(playing)
        } else {
            playing.or_else(|| self.selected_track())
        };
        let Some(fname) = target else { return };
        let done = self.edit(|lib| {
            let e = lib.entries.get_mut(&fname)?;
            e.liked = !e.liked;
            Some((e.liked, e.title.clone()))
        });
        if let Some((liked, title)) = done {
            self.rebuild_track_list();
            let st = if liked { "Liked (never auto-removed)" } else { "Unliked" };
            self.set_notice(format!("{st}: {title}"));
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
        let entry = LikedArtist {
            name: prof.name.clone(),
            spotify_id: prof.spotify_id.clone(),
            image_url: prof.image_url.clone(),
        };
        let removed = self.edit(|lib| {
            let before = lib.liked_artists.len();
            lib.liked_artists
                .retain(|a| a.name.trim().to_ascii_lowercase() != target);
            if lib.liked_artists.len() != before {
                return true;
            }
            lib.liked_artists.push(entry);
            lib.liked_artists
                .sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
            false
        });
        self.set_notice(if removed {
            format!("Unfollowed {}", prof.name)
        } else {
            format!("Following {}", prof.name)
        });
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
        self.edit(|lib| {
            lib.entries.remove(fname);
            for pl in &mut lib.playlists {
                pl.tracks.retain(|x| x != fname);
            }
        });
        self.queue.retain(|x| x != fname);
        self.upcoming.retain(|x| x != fname);
        self.play_context.retain(|x| x != fname);
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
        self.apply_censor();

        let dir = self.dir.clone();
        let tx = self.tx.clone();
        let file = fname.to_string();
        if self.lrc_lines.is_empty() {
            // Older downloads may have no lyrics yet.
            if let Some(e) = self.lib.entries.get(fname).cloned() {
                thread::spawn(move || {
                    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(&file);
                    if downloader::fetch_synced_lrc(&dir, stem, &e.title, &e.artist, e.duration_sec) {
                        let _ = tx.send(WorkerMsg::LyricsFetched(file, true));
                    }
                });
            }
            return;
        }
        if self.lrc_lines.iter().any(|l| l.aligned) {
            return;
        }
        if sync::available() {
            self.aligner.request(fname);
        }
        // A quick estimate to show while (or if) the real alignment isn't ready.
        thread::spawn(move || {
            if let Some(env) = AudioEnvelope::extract(&dir.join(&file)) {
                let estimated = load_lrc(&dir, &file, Some(&env));
                if !estimated.is_empty() {
                    let _ = tx.send(WorkerMsg::LyricsAligned(file, estimated));
                }
            }
        });
    }

    fn adjust_lrc_offset(&mut self, delta_ms: Option<i32>) {
        let Some(fname) = self.player.current_file.clone() else {
            return;
        };
        let off = self.edit(|lib| {
            let e = lib.entries.get_mut(&fname)?;
            e.lrc_offset_ms = match delta_ms {
                Some(d) => (e.lrc_offset_ms + d).clamp(-5000, 5000),
                None => 0,
            };
            Some(e.lrc_offset_ms)
        });
        if let Some(off) = off {
            self.apply_censor();
            self.set_notice(format!("Lyrics offset {:+.2}s", off as f64 / 1000.0));
        }
    }

    fn adjust_cache_limit(&mut self, delta_mb: i64) {
        let cur_mb = (self.max_bytes / 1_048_576) as i64;
        let next_mb = (cur_mb + delta_mb).clamp(100, 10_000) as u64;
        self.max_bytes = next_mb * 1_048_576;
        self.edit(|lib| lib.max_cache_mb = Some(next_mb));
        self.status = format!("Cache limit set to {} MB (saved)", next_mb);
    }

    /// Changes speed or pitch, saves it and applies it to what is playing.
    fn change_tune(&mut self, change: impl FnOnce(&mut tempo::Tune)) {
        let tune = self.edit(|lib| {
            change(&mut lib.tune);
            lib.tune
        });
        if !self.player.set_tune(tune) {
            self.set_notice("mpv rejected the speed / pitch change (needs mpv 0.40 or newer)");
            return;
        }
        self.set_notice(format!(
            "Speed {}  ·  pitch {}  ({})",
            tune.speed_label(),
            tune.pitch_label(),
            if tune.linked { "linked" } else { "independent" }
        ));
    }

    /// Muffles the explicit words of the loaded song (or lifts the filter).
    fn apply_censor(&mut self) {
        let ranges = if self.lib.censor {
            let nudge = self
                .lrc_file
                .as_ref()
                .and_then(|f| self.lib.entries.get(f))
                .map(|e| e.lrc_offset_ms as f64 / 1000.0)
                .unwrap_or(0.0);
            self.censor.audio_ranges(&self.lrc_lines, nudge)
        } else {
            Vec::new()
        };
        if !self.player.set_censored(ranges, self.lib.censor_style) {
            self.set_notice("Censor filter failed: mpv rejected the audio filter");
        }
    }

    /// With the filter on, time the next song's words before it starts.
    fn prepare_next(&mut self) {
        let next = self.queue.front().or_else(|| self.upcoming.front());
        if let Some(file) = next {
            if self.lib.censor && sync::available() && sync::needs_alignment(&self.dir, file) {
                self.aligner.request_later(file);
            }
        }
    }

    fn cycle_censor_style(&mut self) {
        let style = self.edit(|lib| {
            lib.censor_style = lib.censor_style.next();
            lib.censor_style
        });
        self.apply_censor();
        self.set_notice(format!("Censored words sound: {}", style.label()));
    }

    fn toggle_censor(&mut self) {
        let on = self.edit(|lib| {
            lib.censor = !lib.censor;
            lib.censor
        });
        self.censor = censor::Censor::load();
        self.apply_censor();
        self.prepare_next();
        self.set_notice(if on {
            "Censor filter on: explicit words are starred out and softened"
        } else {
            "Censor filter off"
        });
    }

    fn scroll_lyrics(&mut self, delta: isize) {
        if self.lrc_lines.is_empty() {
            self.lyrics_scroll_offset = 0;
            return;
        }
        let pos = self.lyric_position();
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

    /// `ended` is true when the song finished by itself.
    fn play_next(&mut self, ended: bool) {
        if let Some(q) = self.queue.pop_front() {
            self.play_filename_internal(&q, true, false);
            if matches!(self.view, View::Queue) {
                self.rebuild_track_list();
            }
            return;
        }
        if ended && self.repeat == RepeatMode::One {
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
        if self.theme_checked.elapsed() > Duration::from_secs(2) {
            self.theme_checked = now;
            let stamp = theme::Theme::stamp();
            if stamp != self.theme_stamp {
                self.theme_stamp = stamp;
                self.theme = theme::Theme::load();
            }
        }
        if self.pb.active && !self.pb.paused {
            self.smooth_pos += dt * self.lib.tune.speed;
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
                let unknown = self
                    .lib
                    .entries
                    .get(&cur)
                    .map(|e| e.duration_sec == 0)
                    .unwrap_or(false);
                if unknown {
                    let secs = self.pb.dur as u64;
                    self.edit(|lib| {
                        if let Some(e) = lib.entries.get_mut(&cur) {
                            e.duration_sec = secs;
                        }
                    });
                }
            }
        }
        if self.pb.active && self.loaded_at.elapsed() > Duration::from_millis(400) {
            self.seen_active = true;
        }
        // mpv can still report "idle" right after a load; don't skip the song for that.
        let settled = self.seen_active || self.loaded_at.elapsed() > Duration::from_secs(3);
        if self.was_playing_file && self.pb.idle && settled {
            self.was_playing_file = false;
            self.play_next(true);
        }
    }

    fn poll_worker(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Progress(p) => self.set_notice(p),
                WorkerMsg::ThumbReady(key, path) => {
                    self.thumbs.insert(key, path);
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
                WorkerMsg::DownloadDone(job, res) => {
                    self.active_job = None;
                    match res {
                        Ok((fname, info)) => {
                            if job.liked {
                                self.edit(|lib| {
                                    if let Some(e) = lib.entries.get_mut(&fname) {
                                        e.liked = true;
                                    }
                                });
                            }
                            self.reload_library();
                            self.set_notice(info);
                            if self.play_when_ready == Some(job.id) {
                                self.play_filename(&fname, true);
                            } else if sync::available() && sync::needs_alignment(&self.dir, &fname) {
                                self.aligner.request(&fname);
                            }
                        }
                        Err(e) => self.set_notice(format!("Download failed: {e}")),
                    }
                    self.start_next_job();
                }
                WorkerMsg::Jobs(label, jobs) => {
                    self.busy = false;
                    let n = jobs.len();
                    for job in jobs {
                        self.request_track(job);
                    }
                    self.set_notice(format!("Importing {n} songs from '{label}'"));
                }
                WorkerMsg::Repaired(n) => {
                    if n > 0 {
                        self.reload_library();
                        self.art.forget();
                        self.set_notice(format!("Improved {n} song names and covers"));
                    }
                }
                WorkerMsg::Synced(file, res) => {
                    if self.lrc_file.as_deref() == Some(file.as_str()) {
                        match res {
                            Ok((0, _)) => self.set_notice(
                                "These lyrics don't match this audio (press S to search again)",
                            ),
                            Ok((n, total)) => {
                                self.lrc_lines = load_lrc(&self.dir, &file, None);
                                self.apply_censor();
                                self.set_notice(format!("Lyrics synced to audio ({n}/{total} lines)"));
                            }
                            Err(e) => self.set_notice(format!("Lyric sync failed: {e}")),
                        }
                    }
                }
                WorkerMsg::LyricsFetched(file, found) => {
                    if self.lrc_file.as_deref() == Some(file.as_str()) {
                        if found {
                            self.lrc_file = None;
                            self.refresh_lyrics_for(&file);
                        } else {
                            self.set_notice("No matching synced lyrics found");
                        }
                    }
                }
                WorkerMsg::ImportDone(Ok(info)) => {
                    self.busy = false;
                    self.reload_library();
                    self.set_notice(info);
                }
                WorkerMsg::ImportDone(Err(e)) => {
                    self.busy = false;
                    self.set_notice(format!("Import failed: {e}"));
                }
                WorkerMsg::LyricsAligned(fname, lines) => {
                    if self.lrc_file.as_deref() == Some(fname.as_str())
                        && !self.lrc_lines.iter().any(|l| l.aligned)
                    {
                        self.lrc_lines = lines;
                        self.apply_censor();
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
        let col = self.spotify_open_col.clone();
        let mut job = Job::from_spec(&tr, col.as_ref().and_then(|c| c.cover_url.as_deref()));
        job.play = auto_play;
        if let Some(c) = col {
            match c.kind {
                SpotifyCollectionKind::LikedSongs => job.liked = true,
                SpotifyCollectionKind::Playlist { .. }
                | SpotifyCollectionKind::Album { .. }
                | SpotifyCollectionKind::PublicUrl { .. } => job.playlist = Some(c.name),
                _ => {}
            }
        }
        self.request_track(job);
    }

    fn start_artist_download_track(&mut self, idx: usize, auto_play: bool) {
        let Some(prof) = self.artist_profile.as_ref() else {
            return;
        };
        let Some(tr) = prof.top_tracks.get(idx) else {
            return;
        };
        let mut job = Job::from_spec(tr, prof.image_url.as_deref());
        job.play = auto_play;
        self.request_track(job);
    }

    fn start_album_download_track(&mut self, idx: usize, auto_play: bool) {
        let Some(det) = self.album_detail.as_ref() else {
            return;
        };
        let Some(tr) = det.tracks.get(idx) else {
            return;
        };
        let mut job = Job::from_spec(tr, det.cover_url.as_deref());
        job.play = auto_play;
        self.request_track(job);
    }

    fn start_album_import_all(&mut self) {
        let Some(det) = self.album_detail.clone() else {
            return;
        };
        self.queue_collection(det.tracks, det.cover_url, Some(det.title.clone()), false, det.title);
    }

    fn start_spotify_import_all(&mut self) {
        let Some(col) = self.spotify_open_col.clone() else {
            return;
        };
        let liked = matches!(col.kind, SpotifyCollectionKind::LikedSongs);
        let playlist = (!liked).then(|| col.name.clone());
        self.queue_collection(self.spotify_tracks.clone(), col.cover_url, playlist, liked, col.name);
    }

    fn start_artist_import_all(&mut self) {
        let Some(prof) = self.artist_profile.clone() else {
            return;
        };
        let label = format!("{} Top Tracks", prof.name);
        self.queue_collection(prof.top_tracks, prof.image_url, Some(label.clone()), false, label);
    }

    fn queue_collection(
        &mut self,
        tracks: Vec<SpotifyTrackSpec>,
        cover: Option<String>,
        playlist: Option<String>,
        liked: bool,
        label: String,
    ) {
        if tracks.is_empty() {
            return;
        }
        let jobs = collection_jobs(&tracks, cover.as_deref(), playlist.as_deref(), liked);
        // Songs already on disk only need adding to the playlist.
        let have: Vec<String> = jobs.iter().filter_map(|j| self.find_saved(j)).collect();
        self.edit(|lib| {
            for file in &have {
                if let Some(name) = &playlist {
                    add_track_to_playlist(lib, name, file);
                }
                if liked {
                    if let Some(e) = lib.entries.get_mut(file) {
                        e.liked = true;
                    }
                }
            }
        });
        let missing: Vec<Job> = jobs.into_iter().filter(|j| self.find_saved(j).is_none()).collect();
        let n = missing.len();
        for job in missing {
            self.request_track(job);
        }
        self.set_notice(format!("'{label}': {n} to download, {} already saved", have.len()));
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
        let Some(item) = self.search_results.get(i) else {
            return;
        };
        let mut job = Job::from_upload(item);
        job.play = auto_play;
        self.request_track(job);
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
                let jobs = collection_jobs(&bundle.tracks, None, Some(&bundle.collection_name), false);
                let _ = tx.send(WorkerMsg::Jobs(bundle.collection_name, jobs));
                return;
            }
            self.set_notice(format!("Importing local music from '{input}'..."));
            thread::spawn(move || {
                let res = import_local_music(&dir, &input, max_bytes);
                let _ = tx.send(WorkerMsg::ImportDone(res));
            });
            return;
        }

        if input.contains("open.spotify.com/") || input.starts_with("spotify:") {
            self.set_notice("Fetching Spotify metadata...");
            thread::spawn(move || {
                let msg = match fetch_spotify_metadata(&input) {
                    Ok(b) => {
                        let playlist = (b.kind != "track").then_some(b.collection_name.as_str());
                        let jobs = collection_jobs(&b.tracks, b.cover_url.as_deref(), playlist, false);
                        WorkerMsg::Jobs(b.collection_name.clone(), jobs)
                    }
                    Err(e) => WorkerMsg::ImportDone(Err(e)),
                };
                let _ = tx.send(msg);
            });
            return;
        }

        self.set_notice(format!("Looking up '{input}'..."));
        thread::spawn(move || {
            let msg = match search_youtube(&input) {
                Ok(list) if !list.is_empty() => {
                    let take = if input.starts_with("http") { list.len() } else { 1 };
                    let jobs = list.iter().take(take).map(Job::from_upload).collect();
                    WorkerMsg::Jobs(input, jobs)
                }
                Ok(_) => WorkerMsg::ImportDone(Err(format!("Nothing found for '{input}'"))),
                Err(e) => WorkerMsg::ImportDone(Err(e)),
            };
            let _ = tx.send(msg);
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
            ButtonAction::Next => self.play_next(false),
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
                let (dir, max) = (self.dir.clone(), self.max_bytes);
                let (n, freed) = self.edit(|lib| prune_lru(&dir, lib, max));
                self.rebuild_track_list();
                self.set_notice(if n > 0 {
                    format!("Pruned {n} tracks (freed {})", fmt_mb(freed))
                } else {
                    format!(
                        "Cache under {} ({})",
                        fmt_mb(self.max_bytes),
                        fmt_mb(total_cache_bytes(&self.lib))
                    )
                });
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
            ButtonAction::ToggleCensor => self.toggle_censor(),
            ButtonAction::CensorStyle => self.cycle_censor_style(),
            ButtonAction::Speed(up) => self.change_tune(|t| t.step_speed(up)),
            ButtonAction::Pitch(up) => self.change_tune(|t| t.step_pitch(up)),
            ButtonAction::ToggleTuneLink => self.change_tune(tempo::Tune::toggle_link),
            ButtonAction::ResetTune => self.change_tune(tempo::Tune::reset),
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

                // List borders are not rows.
                let list = self.hits.track_list;
                if rect_contains(list, col, row) && (row == list.y || row + 1 == list.bottom()) {
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
                    match &self.input_mode {
                        InputMode::SearchResults => {
                            if self.search_tab == SearchTab::Songs {
                                // Search rows are 2 lines tall with 4x2 thumbnail
                                let idx = self.search_state.offset() + (rel_y / 2);
                                if idx < self.search_results.len() {
                                    self.search_state.select(Some(idx));
                                    self.start_download_search_result(true);
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
                                self.edit(|lib| add_track_to_playlist(lib, &pl_name, &fname));
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
                                    if idx < self.search_results.len() {
                                        self.search_state.select(Some(idx));
                                        self.start_download_search_result(true);
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
                            if idx < self.track_list.len() {
                                self.focus = Focus::Main;
                                let fname = self.track_list[idx].clone();
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

fn collection_jobs(
    tracks: &[SpotifyTrackSpec],
    cover: Option<&str>,
    playlist: Option<&str>,
    liked: bool,
) -> Vec<Job> {
    tracks
        .iter()
        .map(|tr| {
            let mut job = Job::from_spec(tr, cover);
            job.playlist = playlist.map(str::to_string);
            job.liked = liked;
            job
        })
        .collect()
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
                    let _ = tx_c.send(WorkerMsg::ThumbReady(key, path));
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

/// Speed and pitch shortcuts.
fn tune_key(c: char) -> Option<ButtonAction> {
    Some(match c {
        '<' => ButtonAction::Speed(false),
        '>' => ButtonAction::Speed(true),
        '{' => ButtonAction::Pitch(false),
        '}' => ButtonAction::Pitch(true),
        't' => ButtonAction::ToggleTuneLink,
        '0' => ButtonAction::ResetTune,
        _ => return None,
    })
}

fn run_tui(env_max_mb: Option<u64>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(env_max_mb);
    app.art = Art::detect(app.theme.bg);

    loop {
        app.poll_worker();
        app.tick_playback();

        terminal.draw(|f| ui::draw(f, &mut app))?;
        app.art.present(terminal.backend_mut())?;

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
                                    app.edit(|lib| {
                                        lib.playlists.push(Playlist {
                                            name: name.clone(),
                                            tracks: Vec::new(),
                                        })
                                    });
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
                            KeyCode::Char(c) if tune_key(c).is_some() => {
                                app.trigger_button(tune_key(c).unwrap());
                            }
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
                                            app.edit(|lib| add_track_to_playlist(lib, &pl_name, &fname));
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
                            KeyCode::Char(c) if tune_key(c).is_some() => {
                                app.trigger_button(tune_key(c).unwrap());
                            }
                            KeyCode::Char('n') => app.trigger_button(ButtonAction::Next),
                            KeyCode::Char('b') => app.trigger_button(ButtonAction::Prev),
                            KeyCode::Char('+') | KeyCode::Char('=') => app.trigger_button(ButtonAction::VolUp),
                            KeyCode::Char('-') | KeyCode::Char('_') => app.trigger_button(ButtonAction::VolDown),
                            KeyCode::Char('z') => app.trigger_button(ButtonAction::Shuffle),
                            KeyCode::Char('r') => app.trigger_button(ButtonAction::Repeat),
                            KeyCode::Char('L') => app.trigger_button(ButtonAction::ToggleLyrics),
                            KeyCode::Char('S') => app.resync_lyrics(),
                            KeyCode::Char('[') => app.adjust_lrc_offset(Some(-50)),
                            KeyCode::Char(']') => app.adjust_lrc_offset(Some(50)),
                            KeyCode::Char('\\') => app.adjust_lrc_offset(None),
                            KeyCode::Char(',') => app.open_settings_view(),
                            KeyCode::Char('D') => app.trigger_button(ButtonAction::ToggleDownloads),
                            KeyCode::Char('?') => {
                                let pos = app.lyric_position();
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
                            KeyCode::Char('x') if matches!(app.view, View::Settings) => {
                                app.trigger_button(ButtonAction::ToggleCensor);
                            }
                            KeyCode::Char('v') if matches!(app.view, View::Settings) => {
                                app.trigger_button(ButtonAction::CensorStyle);
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
                                        let removed = app.edit(|lib| match lib.playlists.get_mut(idx) {
                                            Some(pl) if sel < pl.tracks.len() => {
                                                pl.tracks.remove(sel);
                                                true
                                            }
                                            _ => false,
                                        });
                                        if removed {
                                            app.rebuild_track_list();
                                            app.set_notice("Removed from playlist");
                                        }
                                    }
                                } else if let Some(fname) = app.selected_track() {
                                    app.delete_track_by_filename(&fname);
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
        "setup-sync" => {
            if let Err(e) = sync::setup() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            println!("Lyric sync is ready. The speech model downloads on first use (~460 MB).");
        }
        "sync" => {
            if !sync::available() {
                eprintln!("Run `itmp setup-sync` first.");
                std::process::exit(1);
            }
            let lib = load_and_sync_library(&dir);
            let mut todo: Vec<&String> = lib
                .entries
                .keys()
                .filter(|f| sync::needs_alignment(&dir, f))
                .collect();
            todo.sort();
            for (i, file) in todo.iter().enumerate() {
                match sync::align(&dir, file) {
                    Ok((n, total)) => println!("[{}/{}] {file}: {n}/{total} lines", i + 1, todo.len()),
                    Err(e) => println!("[{}/{}] {file}: {e}", i + 1, todo.len()),
                }
            }
            println!("All lyrics are synced.");
        }
        "repair" => {
            println!("Fixed {} names and covers.", repair::repair_library(&dir));
        }
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
                        let take = if raw.starts_with("http") { list.len() } else { 1 };
                        for item in list.iter().take(take) {
                            let (title, artist) = item.song_name();
                            let url = format!("https://www.youtube.com/watch?v={}", item.id);
                            match download_and_cache_track(
                                &dir,
                                &url,
                                &title,
                                &artist,
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
                    let (title, artist) = first.song_name();
                    let url = format!("https://www.youtube.com/watch?v={}", first.id);
                    match download_and_cache_track(
                        &dir,
                        &url,
                        &title,
                        &artist,
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
            println!("  itmp repair                      Restore lost or mangled song names");
            println!("  itmp setup-sync                  Install word-level lyric sync (local Whisper)");
            println!("  itmp sync                        Sync lyrics for every downloaded song now");
            println!("  itmp prune                       Prune least-recently-used tracks to <=500 MB");
        }
    }
}
