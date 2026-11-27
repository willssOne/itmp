# itmp — In-Terminal Music Player

A lightweight, fast, low-memory Spotify-style terminal music player written in Rust (`~1.5 MB` stripped binary).

## Features

- **Spotify-Style Top Bar**: `[ itmp ]` home button, always-visible Search bar + `[ Search ]` button, and `[ Settings ]` button right at the top.
- **Visual In-App Search**: Search songs and artists with `4x2` truecolor thumbnails next to every result, `[CACHED]` indicators, and clickable artist names.
- **Spotify Artist Profiles & Liked Artists**: Click any artist name (in Library, Search, Now Playing, or press `A`) to load their profile, avatar, and Top Tracks from Spotify; like/follow artists (`f`) to pin them under `Artists` in your Library sidebar.
- **Settings Menu (`,`)**: Manage your LRU audio cache (gauge, `-100 MB` / `+100 MB` limit adjuster, manual prune) and link Spotify playlists/albums, CSV/JSON exports, or OAuth PKCE login in one place.
- **Live Word-by-Word Synced Lyrics**:
  - Optional word-accurate sync (`itmp setup-sync`): a local Whisper model force-aligns the lyrics to the audio (about 15 s per song on CPU, cached as `.words.json`). Press `S` to re-fetch and re-sync.
  - Without it, an audio-envelope estimate (via `ffmpeg`) is used.
  - The characters just ahead of the one being sung glow softly, so you can read into the line.
  - **Parallel same-line adlibs**: Parenthesized adlibs `( ... )` animate on their own timeline simultaneously alongside main vocals on the same line.
  - Live per-track sync nudge (`[` / `]` / `\`) saved automatically.
- **Speed & Pitch**: Slow songs down or speed them up from the player bar or keyboard (`0.50x`-`2.00x`, up to an octave either way). Linked by default, so changing one automatically sets the other, like a record player (`pitch = 12 * log2(speed)` semitones); unlink to change them independently. The setting is saved, and lyrics and the censor filter stay in sync. Needs mpv 0.40 or newer.
- **Album Art in Terminal**: Real pixels in terminals with sixel or kitty graphics (foot, kitty, ghostty, wezterm); 2x3 sextant block characters elsewhere (Alacritty, tmux). `ITMP_GRAPHICS=blocks` forces the latter.
- **Censor Filter**: Toggle in Settings (`x`). Stars out explicit words on screen and briefly softens them in the audio using the synced lyric timings: a quick crossfade into a **Vocal dip** (the voice ducks, the beat stays) or **Underwater** sound, switchable in Settings (`v`). It deliberately covers little, so the end of a long word can slip through; songs without lyrics play unfiltered and are marked. Add your own words, one per line, in `~/.config/itmp/censor.txt`.
- **Mouse & Keyboard Controls**: Clickable Play/Pause, Prev, Next, Shuffle, Repeat, Like, Artist, Lyrics toggle, Volume `[-]`/`[+]`, and click-to-seek progress bar with elapsed, total, and `-MM:SS left` countdown.
- **Themed UI**: Follows the active Omarchy theme live, with a built-in fallback palette. No emojis or heavy GUI frameworks.

## Requirements

- Rust toolchain (`cargo`) to build
- Runtime CLI tools:
  - [`mpv`](https://mpv.io/) 0.40+ (audio playback via Unix IPC socket)
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

### Maintenance

```bash
itmp setup-sync   # install word-level lyric sync (venv in ~/.local/share/itmp-align)
itmp sync         # sync every downloaded song now
itmp repair       # restore lost or mangled song names (also runs at startup)
```

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
| `<` / `>` | Speed down / up (`0.05x` steps) |
| `{` / `}` | Pitch down / up (semitone steps) |
| `t` | Link / unlink speed and pitch |
| `0` | Reset speed and pitch |
| `z` | Toggle Shuffle |
| `r` | Cycle Repeat (`off` / `all` / `one`) |
| `l` | Like / Pin track (protects from 500 MB LRU eviction) |
| `/` or `s` | Focus top Search bar |
| `A` or Click Artist | Open Spotify Artist Profile (avatar + top tracks) |
| `f` | Like / Follow artist (on Artist Profile -> pins to Library sidebar) |
| `,` | Open Settings (Cache gauge/limit + Spotify playlist linking & OAuth) |
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
