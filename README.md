# itmp — In-Terminal Music Player

A lightweight, fast, low-memory Spotify-style terminal music player written in Rust (`~1.5 MB` stripped binary).

## Features

- **In-app Search & Download**: Always-visible search bar (`/`) to search YouTube, download audio + album cover art + synced `.lrc` lyrics, and play immediately.
- **500 MB LRU Audio Cache**: Automatically prunes least-recently-used tracks when the cache exceeds the limit (`ITMP_CACHE_MB`, default `500 MB`). Liked songs (`*`) are pinned and protected from eviction.
- **Live Word-by-Word Synced Lyrics**:
  - Audio-envelope vocal alignment (`300–1900 Hz` via `ffmpeg`) tracks vocal bursts and breath pauses.
  - **Parallel same-line adlibs**: Parenthesized adlibs `( ... )` animate on their own timeline simultaneously alongside main vocals on the same line.
  - Live per-track sync nudge (`[` / `]` / `\`) saved automatically.
- **Album Art in Terminal**: Truecolor half-block (`▀`) album cover art plus compact `4x2` thumbnails next to every track.
- **Spotify Import & OAuth PKCE Login**:
  - **Free (no Premium needed)**: Paste any public Spotify playlist, album, or track URL (or Exportify `.csv` / Spotify Privacy `.json` export) directly in the `Spotify` tab (`p`) to browse, play, or bulk-import.
  - **OAuth 2.0 PKCE Login** (for Spotify Premium developer accounts): Browse Liked Songs, Private Playlists, Saved Albums, Recently Played, and Top Tracks.
- **Mouse & Keyboard Controls**: Clickable Play/Pause, Prev, Next, Shuffle, Repeat, Like, Import, Lyrics toggle, Volume `[-]`/`[+]`, and click-to-seek progress bar with elapsed, total, and `-MM:SS left` countdown.
- **Minimal ASCII UI**: Clean terminal interface with no emojis or heavy GUI frameworks.

## Requirements

- Rust toolchain (`cargo`) to build
- Runtime CLI tools:
  - [`mpv`](https://mpv.io/) (audio playback via Unix IPC socket)
  - [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) (song search & download)
  - [`ffmpeg`](https://ffmpeg.org/) (vocal-band envelope extraction for lyric sync)
  - `curl` (LRCLIB lyrics, cover art, Spotify API)

## Install

```bash
git clone https://github.com/willssOne/itmp.git
cd itmp
cargo build --release
install -Dm755 target/release/itmp ~/.local/bin/itmp
```

Or install directly with Cargo:

```bash
cargo install --path .
```

## Usage

### Launch TUI

```bash
itmp
```

### Keybindings

| Key | Action |
|---|---|
| `Space` | Play / Pause |
| `Enter` / Click | Play selected track (or download & play in Search / Spotify) |
| `n` / `b` | Next / Previous track |
| `Left` / `Right` | Seek `-5s` / `+5s` (or click the progress bar) |
| `+` / `-` | Volume up / down |
| `z` | Toggle Shuffle |
| `r` | Cycle Repeat (`off` / `all` / `one`) |
| `l` | Like / Pin track (protects from 500 MB LRU eviction) |
| `/` or `s` | Focus top Search bar |
| `S` | Open Spotify tab (`p` add playlist/album URL or CSV/JSON, `a` import all) |
| `i` | Import Spotify URL, YouTube URL, CSV/JSON export, or local files/folder |
| `L` | Toggle live synced lyrics pane |
| `[` / `]` / `\` | Nudge lyric sync `-50ms` / `+50ms` / reset (saved per track) |
| `Q` | Add track to Up Next queue |
| `N` / `P` | Create playlist / Add track to playlist |
| `d` | Delete track from cache (or download in background in Search / Spotify) |
| `c` | Manually prune LRU cache |
| `Tab` | Switch focus between Library sidebar and main pane |
| `q` | Quit |

### CLI Commands

```bash
itmp                                 # Launch TUI
itmp add <query or URL>              # Download song + cover art + synced .lrc
itmp import <spotify|yt|path|csv>    # Import Spotify URL, YouTube, CSV/JSON, or local folder
itmp spotify login <client-id>       # OAuth 2.0 PKCE login (Redirect URI: http://127.0.0.1:8888/callback)
itmp spotify status                  # Check Spotify login status
itmp spotify logout                  # Remove saved Spotify tokens
itmp list                            # List cached tracks, lyrics, covers & playlists
itmp prune                           # Prune cache to <= ITMP_CACHE_MB
```

### Configuration

- Cache directory: `$ITMP_CACHE_DIR` or `$XDG_DATA_HOME/itmp` (default `~/.local/share/itmp`)
- Cache size limit: `ITMP_CACHE_MB` (default `500`)
- Spotify session & saved links: `$XDG_CONFIG_HOME/itmp/spotify.json` (default `~/.config/itmp/spotify.json`, `0600`)

## License

MIT
