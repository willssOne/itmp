//! Everything drawn on screen. Also records the clickable areas in `app.hits`.

use super::*;
use crate::theme::{mix, Theme};
use ratatui::{widgets::BorderType, Frame};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Truncates to a display width, adding an ellipsis.
fn fit(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = width(c.encode_utf8(&mut [0; 4]));
        if used + w + 1 > max {
            break;
        }
        used += w;
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    out
}

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

fn pill(t: &Theme, on: bool, color: Color) -> Style {
    if on {
        Style::default().fg(t.bg).bg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.text).bg(t.panel)
    }
}

fn panel<'a>(t: &Theme, title: impl Into<String>, focused: bool) -> Block<'a> {
    let title = title.into();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(if focused { t.accent } else { t.faint }));
    if title.is_empty() {
        block
    } else {
        block.title(Span::styled(
            format!(" {title} "),
            bold(if focused { t.accent } else { t.dim }),
        ))
    }
}

fn list<'a>(t: &Theme, items: Vec<ListItem<'a>>, block: Block<'a>) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(t.select))
        .highlight_symbol("▌ ")
        .repeat_highlight_symbol(true)
}

/// A button that is also registered as a hit area. Returns its width.
fn button(
    f: &mut Frame,
    hits: &mut Vec<(Rect, ButtonAction)>,
    x: u16,
    y: u16,
    label: &str,
    style: Style,
    action: ButtonAction,
) -> u16 {
    let w = width(label) as u16;
    let area = f.area();
    if x + w <= area.right() && y < area.bottom() {
        let r = Rect::new(x, y, w, 1);
        f.render_widget(Paragraph::new(Span::styled(label.to_string(), style)), r);
        hits.push((r, action));
    }
    w
}

/// A horizontal run of buttons starting inside `area`.
fn button_row(
    f: &mut Frame,
    hits: &mut Vec<(Rect, ButtonAction)>,
    area: Rect,
    defs: Vec<(String, Style, Option<ButtonAction>)>,
) {
    let mut x = area.x;
    for (label, style, action) in defs {
        let w = width(&label) as u16;
        if x + w > area.right() {
            break;
        }
        let r = Rect::new(x, area.y, w, 1);
        f.render_widget(Paragraph::new(Span::styled(label, style)), r);
        if let Some(a) = action {
            hits.push((r, a));
        }
        x += w + 1;
    }
}

fn inner(r: Rect) -> Rect {
    Rect::new(
        r.x + 1,
        r.y + 1,
        r.width.saturating_sub(2),
        r.height.saturating_sub(2),
    )
}

fn empty_state(f: &mut Frame, t: &Theme, area: Rect, block: Block, headline: &str, hint: &str) {
    let pad = area.height.saturating_sub(6) / 2;
    let mut lines: Vec<Line> = (0..pad).map(|_| Line::from("")).collect();
    lines.push(Line::from(Span::styled(headline.to_string(), bold(t.text))));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(hint.to_string(), fg(t.dim))));
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(block),
        area,
    );
}

// ---------------------------------------------------------------------------
// Two-line media rows (tracks and releases)
// ---------------------------------------------------------------------------

struct Row {
    thumb: Option<PathBuf>,
    /// Up to 4 columns before the title: number, play state, ...
    lead: (String, Style),
    title: String,
    title_color: Color,
    artist: String,
    album: String,
    meta: String,
    right: (String, Style),
    badge: (String, Style),
}

impl Row {
    fn new(t: &Theme, title: String) -> Self {
        Row {
            thumb: None,
            lead: (String::new(), fg(t.dim)),
            title,
            title_color: t.bright,
            artist: String::new(),
            album: String::new(),
            meta: String::new(),
            right: (String::new(), fg(t.dim)),
            badge: (String::new(), fg(t.accent)),
        }
    }

    /// `y` is the screen row of the second line when the item is visible.
    fn build(
        self,
        t: &Theme,
        area: Rect,
        y: Option<u16>,
        mouse: (u16, u16),
        hits: &mut HitAreas,
        art: &mut Art,
    ) -> ListItem<'static> {
        const LEFT: usize = 4 + 2 + 4; // art, gap, lead
        let total = (area.width as usize).saturating_sub(2 + 2 + 1);
        let text_w = |right: &str| total.saturating_sub(LEFT + width(right) + 2);

        let mut l1 = Vec::new();
        let mut l2 = Vec::new();
        match (&self.thumb, y) {
            (Some(path), Some(y)) => {
                art.queue(path.clone(), Rect::new(area.x + 3, y - 1, 4, 2));
                l1.push(Span::raw("    "));
                l2.push(Span::raw("    "));
            }
            _ => {
                let blank = Span::styled(" ·  ", Style::default().fg(t.faint).bg(t.panel));
                l1.push(blank);
                l2.push(Span::styled("    ", Style::default().bg(t.panel)));
            }
        }
        l1.push(Span::raw("  "));
        l2.push(Span::raw("  "));
        l1.push(Span::styled(format!("{:<4}", self.lead.0), self.lead.1));
        l2.push(Span::raw("    "));

        let w1 = text_w(&self.right.0);
        let title = fit(&self.title, w1);
        let pad = w1.saturating_sub(width(&title)) + 2;
        l1.push(Span::styled(title, bold(self.title_color)));
        l1.push(Span::raw(" ".repeat(pad)));
        l1.push(Span::styled(self.right.0, self.right.1));

        let w2 = text_w(&self.badge.0);
        let mut used = 0;
        let mut x = area.x + 3 + LEFT as u16;
        if !self.artist.is_empty() {
            let shown = fit(&self.artist, w2);
            let w = width(&shown);
            let mut style = fg(t.warm);
            if let Some(y) = y {
                let r = Rect::new(x, y, w as u16, 1);
                if rect_contains(r, mouse.0, mouse.1) {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                hits.artist_links.push((r, self.artist.clone()));
            }
            l2.push(Span::styled(shown, style));
            used += w;
            x += w as u16;
        }
        if !self.album.is_empty() && used + 6 < w2 {
            let sep = if used > 0 { " · " } else { "" };
            l2.push(Span::styled(sep, fg(t.faint)));
            used += width(sep);
            x += width(sep) as u16;
            let shown = fit(&self.album, w2 - used);
            let w = width(&shown);
            let mut style = fg(t.dim);
            if let Some(y) = y {
                let r = Rect::new(x, y, w as u16, 1);
                if rect_contains(r, mouse.0, mouse.1) {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                hits.album_links
                    .push((r, self.artist.clone(), self.album.clone()));
            }
            l2.push(Span::styled(shown, style));
            used += w;
        }
        if !self.meta.is_empty() && used + 4 < w2 {
            let sep = if used > 0 { " · " } else { "" };
            let shown = fit(&format!("{sep}{}", self.meta), w2 - used);
            used += width(&shown);
            l2.push(Span::styled(shown, fg(t.dim)));
        }
        l2.push(Span::raw(" ".repeat(w2.saturating_sub(used) + 2)));
        l2.push(Span::styled(self.badge.0, self.badge.1));

        ListItem::new(vec![Line::from(l1), Line::from(l2)])
    }
}

/// Screen row of the second line of item `i` in a two-line list, if visible.
fn second_line(area: Rect, offset: usize, i: usize) -> Option<u16> {
    let rel = i.checked_sub(offset)?;
    let y = area.y as usize + 2 + rel * 2;
    (y + 1 < area.bottom() as usize).then_some(y as u16)
}

fn duration(secs: u64) -> String {
    if secs > 0 {
        fmt_time(secs as f64)
    } else {
        "–:––".to_string()
    }
}

/// Rows for a list of not-necessarily-downloaded tracks (artist and album pages).
fn spec_rows(
    app: &mut App,
    area: Rect,
    tracks: &[SpotifyTrackSpec],
    offset: usize,
    shared_cover: Option<&str>,
    show_album: bool,
) -> Vec<ListItem<'static>> {
    let t = app.theme;
    tracks
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            let mut row = Row::new(&t, tr.title.clone());
            row.thumb = shared_cover
                .or(tr.cover_url.as_deref())
                .and_then(|u| app.thumbs.get(&format!("url:{u}")))
                .cloned();
            row.lead = (format!("{:>2}", i + 1), fg(t.dim));
            row.artist = tr.artist.clone();
            if show_album {
                row.album = tr.album.clone();
            }
            row.right = (duration(tr.duration_sec), fg(t.dim));
            if app.is_saved(&tr.title, &tr.artist, tr.duration_sec) {
                row.badge = ("✓ saved".into(), fg(t.accent));
            }
            row.build(&t, area, second_line(area, offset, i), app.mouse_pos, &mut app.hits, &mut app.art)
        })
        .collect()
}

fn release_rows(
    app: &mut App,
    area: Rect,
    albums: &[AlbumSummary],
    offset: usize,
) -> Vec<ListItem<'static>> {
    let t = app.theme;
    albums
        .iter()
        .enumerate()
        .map(|(i, al)| {
            let mut row = Row::new(&t, al.title.clone());
            row.thumb = al
                .cover_url
                .as_ref()
                .and_then(|u| app.thumbs.get(&format!("url:{u}")))
                .cloned();
            row.artist = al.artist.clone();
            let mut meta = Vec::new();
            if !al.release_year.is_empty() {
                meta.push(al.release_year.clone());
            }
            if al.track_count > 0 {
                let unit = if al.track_count == 1 { "track" } else { "tracks" };
                meta.push(format!("{} {unit}", al.track_count));
            }
            row.meta = meta.join(" · ");
            let color = match al.record_type.as_str() {
                "ep" => t.warm,
                "single" => t.accent,
                _ => t.gold,
            };
            row.right = (al.type_badge().to_string(), fg(color));
            row.build(&t, area, second_line(area, offset, i), app.mouse_pos, &mut app.hits, &mut app.art)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits = HitAreas::default();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(4),
        ])
        .split(f.area());

    header(f, app, outer[0]);

    let side_w = match outer[1].width {
        180.. => 42,
        140.. => 36,
        96.. => 28,
        _ => 22,
    };
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(side_w), Constraint::Min(30)])
        .split(outer[1]);
    sidebar(f, app, body[0]);

    let lyrics_on = app.show_lyrics_pane && !matches!(app.view, View::Settings);
    let (main, lyrics) = if !lyrics_on {
        (body[1], None)
    } else if body[1].width >= 84 {
        let c = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(body[1]);
        (c[0], Some(c[1]))
    } else {
        let r = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(body[1]);
        (r[0], Some(r[1]))
    };

    app.hits.track_list = main;
    if matches!(app.input_mode, InputMode::AddToPlaylist(_)) {
        playlist_picker(f, app, main);
    } else if matches!(app.input_mode, InputMode::SearchResults)
        || matches!(app.view, View::Search)
    {
        search_view(f, app, main);
    } else {
        match app.view {
            View::Artist => artist_view(f, app, main),
            View::Album => album_view(f, app, main),
            View::Settings => settings_view(f, app, main),
            _ => library_view(f, app, main),
        }
    }

    if let Some(area) = lyrics {
        lyrics_view(f, app, area);
    }
    player_bar(f, app, outer[2]);

    if app.show_downloads_menu {
        downloads_menu(f, app, outer[0]);
    }
    input_modal(f, app);
    genius_popup(f, app);

    if app.lib.censor {
        // What the user is typing stays readable.
        app.censor
            .scrub(f.buffer_mut(), &[app.hits.search_box, app.hits.modal]);
    }
    let covered: Vec<Rect> = [app.hits.dl_dropdown, app.hits.modal, app.hits.popup_rect]
        .into_iter()
        .filter(|r| !r.is_empty())
        .collect();
    app.art.flush(f, &covered);
}

fn header(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let searching = matches!(app.input_mode, InputMode::Search);
    f.render_widget(panel(&t, "", searching), area);
    let row = inner(area);

    let home = matches!(
        app.view,
        View::AllTracks | View::Liked | View::Queue | View::Playlist(_)
    );
    let mut x = row.x + 1;
    x += button(
        f,
        &mut app.hits.buttons,
        x,
        row.y,
        " ▶ itmp ",
        pill(&t, home, t.accent),
        ButtonAction::Home,
    ) + 2;

    let dl_count = app.jobs.len() + app.active_job.is_some() as usize;
    let dl_label = if dl_count > 0 {
        format!(" ↓ {dl_count} ")
    } else {
        " ↓ Downloads ".to_string()
    };
    let buttons = [
        (
            " Search ".to_string(),
            pill(&t, searching || matches!(app.view, View::Search), t.accent),
            ButtonAction::SearchBtn,
        ),
        (
            dl_label,
            if dl_count > 0 && !app.show_downloads_menu {
                Style::default().fg(t.gold).bg(t.panel).add_modifier(Modifier::BOLD)
            } else {
                pill(&t, app.show_downloads_menu, t.gold)
            },
            ButtonAction::ToggleDownloads,
        ),
        (
            " Settings ".to_string(),
            pill(&t, matches!(app.view, View::Settings), t.warm),
            ButtonAction::Settings,
        ),
    ];
    let total: u16 = buttons.iter().map(|(l, _, _)| width(l) as u16 + 1).sum();
    let mut bx = row.right().saturating_sub(total);
    app.hits.search_box = Rect::new(x, row.y, bx.saturating_sub(x + 1), 1);
    for (label, style, action) in buttons {
        let w = button(f, &mut app.hits.buttons, bx, row.y, &label, style, action);
        if matches!(action, ButtonAction::ToggleDownloads) {
            app.hits.dl_btn = Rect::new(bx, row.y, w, 1);
        }
        bx += w + 1;
    }

    let sw = app.hits.search_box.width as usize;
    let (text, style) = if searching {
        let shown: String = {
            // Keep the end of a long query (or pasted URL) visible.
            let n = app.input_buf.chars().count();
            app.input_buf.chars().skip(n.saturating_sub(sw.saturating_sub(5))).collect()
        };
        (format!(" / {shown}▏"), bold(t.bright))
    } else if !app.last_search_query.is_empty() && matches!(app.view, View::Search) {
        (format!(" / {}", app.last_search_query), fg(t.text))
    } else {
        (
            " /  Search songs, artists, albums or paste a link".to_string(),
            fg(t.dim),
        )
    };
    f.render_widget(
        Paragraph::new(fit(&text, sw)).style(style.bg(t.panel)),
        app.hits.search_box,
    );
}

fn sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    // The art box is square: two columns per row.
    let art_h = (area.width.saturating_sub(2) / 2 + 2).min(area.height.saturating_sub(9));
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(art_h)])
        .split(area);

    app.hits.sidebar = parts[0];
    let w = parts[0].width.saturating_sub(5) as usize;
    let items: Vec<ListItem> = app
        .sidebar_rows()
        .into_iter()
        .map(|(label, entry)| {
            let style = match entry {
                SidebarEntry::Header => bold(t.faint),
                SidebarEntry::Artist(_) => fg(t.warm),
                SidebarEntry::NewPlaylist => fg(t.accent),
                _ => fg(t.text),
            };
            ListItem::new(Span::styled(fit(&label, w), style))
        })
        .collect();
    let focused = app.focus == Focus::Sidebar;
    let widget = list(&t, items, panel(&t, "Library", focused));
    f.render_stateful_widget(widget, parts[0], &mut app.sidebar_state);

    if parts[1].height >= 6 {
        let album = app
            .current_track_entry()
            .map(|e| e.album.clone())
            .unwrap_or_default();
        let art = inner(parts[1]);
        let block = panel(&t, fit(&album, art.width.saturating_sub(2) as usize), false);
        let file = app.player.current_file.clone().or_else(|| app.selected_track());
        match file.and_then(|f| app.cover_path_for(&f)) {
            Some(path) => {
                f.render_widget(block, parts[1]);
                app.art.queue(path, art);
            }
            None => empty_state(f, &t, parts[1], block, "itmp", "No cover"),
        }
    }
}

fn playlist_picker(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let items: Vec<ListItem> = app
        .lib
        .playlists
        .iter()
        .map(|p| ListItem::new(format!("{}  ({})", p.name, p.tracks.len())))
        .collect();
    let widget = list(&t, items, panel(&t, "Add to playlist · Enter select · Esc cancel", true));
    f.render_stateful_widget(widget, area, &mut app.playlist_pick_state);
}

fn library_view(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let name = match &app.view {
        View::AllTracks => "All Tracks".to_string(),
        View::Liked => "Liked Songs".to_string(),
        View::Queue => "Up Next".to_string(),
        View::Playlist(i) => app
            .lib
            .playlists
            .get(*i)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Playlist".into()),
        _ => "Tracks".to_string(),
    };
    let focused = app.focus == Focus::Main;
    let block = panel(&t, format!("{name} · {}", app.track_list.len()), focused);
    if app.track_list.is_empty() {
        empty_state(f, &t, area, block, "Nothing here yet", "Press / to search and download music");
        return;
    }

    let offset = app.track_state.offset();
    let queue_view = matches!(app.view, View::Queue);
    let files = app.track_list.clone();
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .filter_map(|(i, file)| {
            let e = app.lib.entries.get(file)?.clone();
            let playing = app.player.current_file.as_deref() == Some(file.as_str());
            let mut row = Row::new(&t, e.title.clone());
            // Pictures are only decoded for rows that end up on screen.
            if second_line(area, offset, i).is_some() {
                row.thumb = app.cover_path_for(file);
            }
            row.lead = if playing {
                let icon = if app.pb.paused { "▐▐" } else { "▶" };
                (format!(" {icon}"), bold(t.accent))
            } else if queue_view && i < app.queue.len() {
                (format!("{:>2}", i + 1), bold(t.gold))
            } else {
                (format!("{:>2}", i + 1), fg(t.faint))
            };
            if playing {
                row.title_color = t.accent;
            }
            row.artist = clean_artist_name(&e.artist);
            row.album = e.album.clone();
            if row.artist.is_empty() {
                row.meta = "unknown artist".into();
            }
            row.right = (duration(e.duration_sec), fg(t.dim));
            let mut badge = String::new();
            if queue_view && i < app.queue.len() {
                badge.push_str("queued ");
            }
            if e.liked {
                badge.push('♥');
            }
            row.badge = (badge, fg(t.gold));
            Some(row.build(&t, area, second_line(area, offset, i), app.mouse_pos, &mut app.hits, &mut app.art))
        })
        .collect();
    f.render_stateful_widget(list(&t, items, block), area, &mut app.track_state);
}

fn search_view(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(6)])
        .split(area);

    let count = |kind: &[&str]| {
        app.search_albums_list
            .iter()
            .filter(|a| kind.contains(&a.record_type.as_str()))
            .count()
    };
    let tabs = [
        ("Songs", SearchTab::Songs, app.search_results.len()),
        ("All", SearchTab::AllReleases, app.search_albums_list.len()),
        ("Albums", SearchTab::Albums, count(&["album", "compile"])),
        ("EPs", SearchTab::Eps, count(&["ep"])),
        ("Singles", SearchTab::Singles, count(&["single"])),
    ];
    let wide = rows[0].width >= 62;
    let defs = tabs
        .iter()
        .map(|&(name, tab, n)| {
            let label = if wide { format!(" {name} {n} ") } else { format!(" {name} ") };
            (
                label,
                pill(&t, app.search_tab == tab, t.accent),
                Some(ButtonAction::SearchTab(tab)),
            )
        })
        .collect();
    button_row(f, &mut app.hits.buttons, rows[0], defs);

    let area = rows[1];
    app.hits.track_list = area;
    let block = panel(&t, "Results", true);
    let waiting = app.busy;
    let query = app.last_search_query.clone();
    let empty = |f: &mut Frame, block: Block| {
        if waiting {
            empty_state(f, &t, area, block, "Searching…", &query);
        } else if query.is_empty() {
            empty_state(f, &t, area, block, "Find something to play", "Type a song, artist or album above");
        } else {
            empty_state(f, &t, area, block, "No results", "Try another search or tab");
        }
    };

    if app.search_tab == SearchTab::Songs {
        if app.search_results.is_empty() {
            return empty(f, block);
        }
        let offset = app.search_state.offset();
        let results = app.search_results.clone();
        let items: Vec<ListItem> = results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (title, artist) = r.song_name();
                let saved = app.saved_ids.contains(&r.id) || app.is_saved(&title, &artist, r.seconds());
                let mut row = Row::new(&t, title);
                row.thumb = app.thumbs.get(&format!("yt:{}", r.id)).cloned();
                row.artist = r.uploader.clone();
                row.right = (r.duration.clone(), fg(t.dim));
                if saved {
                    row.badge = ("✓ saved".into(), fg(t.accent));
                }
                row.build(&t, area, second_line(area, offset, i), app.mouse_pos, &mut app.hits, &mut app.art)
            })
            .collect();
        f.render_stateful_widget(list(&t, items, block), area, &mut app.search_state);
    } else {
        let albums = app.filtered_search_albums();
        if albums.is_empty() {
            return empty(f, block);
        }
        let offset = app.search_album_state.offset();
        let items = release_rows(app, area, &albums, offset);
        f.render_stateful_widget(list(&t, items, block), area, &mut app.search_album_state);
    }
}

/// Square art on the left, title/details/buttons on the right.
fn hero(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    art: Option<PathBuf>,
    title: String,
    details: Vec<(String, Color)>,
    buttons: Vec<(String, Style, Option<ButtonAction>)>,
) {
    let t = app.theme;
    f.render_widget(panel(&t, "", false), area);
    let body = inner(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Length(2), Constraint::Min(10)])
        .split(body);
    if let Some(path) = art {
        app.art.queue(path, cols[0]);
    }
    let w = cols[2].width as usize;
    let mut lines = vec![Line::from(Span::styled(fit(&title, w), bold(t.bright)))];
    lines.extend(
        details
            .into_iter()
            .map(|(s, c)| Line::from(Span::styled(fit(&s, w), fg(c)))),
    );
    f.render_widget(Paragraph::new(lines), cols[2]);
    let by = Rect::new(cols[2].x, cols[2].bottom().saturating_sub(1), cols[2].width, 1);
    button_row(f, &mut app.hits.buttons, by, buttons);
}

fn artist_view(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let Some(prof) = app.artist_profile.clone() else {
        let name = app.artist_loading_name.clone();
        let (head, hint) = match name {
            Some(n) => ("Loading artist…", n),
            None => ("No artist selected", "Click any artist name".to_string()),
        };
        empty_state(f, &t, area, panel(&t, "Artist", true), head, &hint);
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Length(1), Constraint::Min(6)])
        .split(area);

    let liked = app.is_artist_liked(&prof.name);
    let saved = app
        .lib
        .entries
        .values()
        .filter(|e| crate::names::same_artist(&e.artist, &prof.name))
        .count();
    let mut details = vec![(format!("{saved} songs downloaded"), t.text)];
    if let Some(fans) = prof.fans {
        details.push((fmt_fans(fans), t.dim));
    }
    details.push((prof.source.clone(), t.faint));
    let art = app.artist_avatar_path.clone();
    hero(
        f,
        app,
        rows[0],
        art,
        prof.name.clone(),
        details,
        vec![
            (
                if liked { " ♥ Following " } else { " ♥ Follow " }.into(),
                pill(&t, liked, t.gold),
                Some(ButtonAction::ToggleLikeArtist),
            ),
            (" ↓ Top tracks ".into(), pill(&t, false, t.accent), Some(ButtonAction::ArtistImportAll)),
            (" ← Back ".into(), pill(&t, false, t.accent), Some(ButtonAction::Home)),
        ],
    );

    let count = |kind: &[&str]| {
        prof.albums
            .iter()
            .filter(|a| kind.contains(&a.record_type.as_str()))
            .count()
    };
    let tabs = [
        ("Top", ArtistTab::TopTracks, prof.top_tracks.len()),
        ("All", ArtistTab::All, prof.albums.len()),
        ("Albums", ArtistTab::Albums, count(&["album", "compile"])),
        ("EPs", ArtistTab::Eps, count(&["ep"])),
        ("Singles", ArtistTab::Singles, count(&["single"])),
    ];
    let wide = rows[1].width >= 62;
    let defs = tabs
        .iter()
        .map(|&(name, tab, n)| {
            let label = if wide { format!(" {name} {n} ") } else { format!(" {name} ") };
            (
                label,
                pill(&t, app.artist_tab == tab, t.accent),
                Some(ButtonAction::ArtistTab(tab)),
            )
        })
        .collect();
    button_row(f, &mut app.hits.buttons, rows[1], defs);

    let area = rows[2];
    app.hits.track_list = area;
    let focused = app.focus == Focus::Main;
    if app.artist_tab == ArtistTab::TopTracks {
        let offset = app.artist_track_state.offset();
        let items = spec_rows(app, area, &prof.top_tracks, offset, None, true);
        let widget = list(&t, items, panel(&t, "Top tracks", focused));
        f.render_stateful_widget(widget, area, &mut app.artist_track_state);
    } else {
        let albums = app.filtered_artist_albums();
        let block = panel(&t, "Discography", focused);
        if albums.is_empty() {
            empty_state(f, &t, area, block, "No releases", "Nothing in this category");
            return;
        }
        let offset = app.artist_album_state.offset();
        let items = release_rows(app, area, &albums, offset);
        f.render_stateful_widget(list(&t, items, block), area, &mut app.artist_album_state);
    }
}

fn album_view(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let Some(det) = app.album_detail.clone() else {
        let name = app.album_loading_name.clone();
        let (head, hint) = match name {
            Some(n) => ("Loading album…", n),
            None => ("Album unavailable", "Press Esc to go back".to_string()),
        };
        empty_state(f, &t, area, panel(&t, "Album", true), head, &hint);
        app.hits
            .buttons
            .push((Rect::new(area.x + 2, area.y, 7, 1), ButtonAction::AlbumBack));
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(6)])
        .split(area);

    let saved = det
        .tracks
        .iter()
        .filter(|tr| app.is_saved(&tr.title, &tr.artist, tr.duration_sec))
        .count();
    let art = app.album_cover_path.clone();
    hero(
        f,
        app,
        rows[0],
        art,
        det.title.clone(),
        vec![
            (det.artist.clone(), t.warm),
            (
                format!("{} · {} · {} tracks", det.type_badge(), det.release_date, det.tracks.len()),
                t.dim,
            ),
            (format!("{saved} / {} downloaded", det.tracks.len()), t.text),
        ],
        vec![
            (" ↓ Download all ".into(), pill(&t, false, t.accent), Some(ButtonAction::AlbumImportAll)),
            (" Artist ".into(), pill(&t, false, t.accent), Some(ButtonAction::OpenAlbumArtist)),
            (" ← Back ".into(), pill(&t, false, t.accent), Some(ButtonAction::AlbumBack)),
        ],
    );

    let area = rows[1];
    app.hits.track_list = area;
    let offset = app.album_track_state.offset();
    let items = spec_rows(app, area, &det.tracks, offset, det.cover_url.as_deref(), false);
    let widget = list(&t, items, panel(&t, "Tracklist", app.focus == Focus::Main));
    f.render_stateful_widget(widget, area, &mut app.album_track_state);
}

fn settings_view(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let (left, keys) = if area.width >= 88 {
        let c = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(44), Constraint::Length(44)])
            .split(area);
        (c[0], Some(c[1]))
    } else {
        (area, None)
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(6),
        ])
        .split(left);

    // Storage
    f.render_widget(panel(&t, "Storage", false), rows[0]);
    let body = inner(rows[0]);
    let used = total_cache_bytes(&app.lib);
    let ratio = (used as f64 / app.max_bytes.max(1) as f64).clamp(0.0, 1.0);
    let bar_w = body.width.saturating_sub(2) as usize;
    let filled = (bar_w as f64 * ratio).round() as usize;
    let liked = app.lib.entries.values().filter(|e| e.liked).count();
    let sync = if crate::sync::available() {
        "Word-level lyric sync: on"
    } else {
        "Word-level lyric sync: off (run `itmp setup-sync`)"
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(
                " {} of {}  ·  {} songs  ·  {liked} liked",
                fmt_mb(used),
                fmt_mb(app.max_bytes),
                app.lib.entries.len()
            ),
            fg(t.text),
        )),
        Line::from(vec![
            Span::raw(" "),
            Span::styled("━".repeat(filled), fg(t.accent)),
            Span::styled("━".repeat(bar_w.saturating_sub(filled)), fg(t.panel)),
        ]),
        Line::from(Span::styled(format!(" {}", app.dir.display()), fg(t.dim))),
        Line::from(Span::styled(format!(" {sync}"), fg(t.dim))),
    ];
    f.render_widget(Paragraph::new(lines), body);
    let by = Rect::new(body.x + 1, body.y + 4, body.width.saturating_sub(2), 1);
    button_row(
        f,
        &mut app.hits.buttons,
        by,
        vec![
            (" −100 MB ".into(), pill(&t, false, t.accent), Some(ButtonAction::CacheDec)),
            (" +100 MB ".into(), pill(&t, false, t.accent), Some(ButtonAction::CacheInc)),
            (" Prune ".into(), pill(&t, false, t.accent), Some(ButtonAction::PruneCache)),
            (" Import ".into(), pill(&t, false, t.accent), Some(ButtonAction::Import)),
        ],
    );

    // Content filter and picture quality
    f.render_widget(panel(&t, "Filter & display", false), rows[1]);
    let body = inner(rows[1]);
    let on = app.lib.censor;
    button_row(
        f,
        &mut app.hits.buttons,
        Rect::new(body.x + 1, body.y, body.width.saturating_sub(2), 1),
        vec![
            (
                if on { " Censor filter: ON " } else { " Censor filter: OFF " }.into(),
                pill(&t, on, t.accent),
                Some(ButtonAction::ToggleCensor),
            ),
            (
                format!(" Sound: {} ", app.lib.censor_style.label()),
                pill(&t, false, t.accent),
                Some(ButtonAction::CensorStyle),
            ),
        ],
    );
    let images = format!(" Images: {}", app.art.describe());
    f.render_widget(
        Paragraph::new(Span::styled(fit(&images, body.width as usize), fg(t.dim))),
        Rect::new(body.x, body.y + 1, body.width, 1),
    );

    // Spotify
    f.render_widget(panel(&t, "Spotify", false), rows[2]);
    let mut defs = vec![(
        " + Link URL / CSV ".into(),
        pill(&t, true, t.accent),
        Some(ButtonAction::SpotifyAddUrl),
    )];
    if app.spotify.is_logged_in() {
        defs.push((format!(" {} ", app.spotify.display_name), fg(t.dim), None));
        defs.push((" Refresh ".into(), pill(&t, false, t.accent), Some(ButtonAction::SpotifyRefresh)));
        defs.push((" Logout ".into(), pill(&t, false, t.accent), Some(ButtonAction::SpotifyLogout)));
    } else {
        defs.push((" Log in ".into(), pill(&t, false, t.accent), Some(ButtonAction::SpotifyLoginPrompt)));
        defs.push((" Dashboard ".into(), pill(&t, false, t.accent), Some(ButtonAction::SpotifyDashboard)));
    }
    button_row(f, &mut app.hits.buttons, inner(rows[2]), defs);

    let list_area = rows[3];
    app.hits.settings_list = list_area;
    let focused = app.focus == Focus::Main;
    if let Some(col) = app.spotify_open_col.clone() {
        let w = list_area.width.saturating_sub(5) as usize;
        let tracks = app.spotify_tracks.clone();
        let items: Vec<ListItem> = tracks
            .iter()
            .map(|tr| {
                let mark = if app.is_saved(&tr.title, &tr.artist, tr.duration_sec) {
                    "✓"
                } else {
                    " "
                };
                let title = fit(&tr.title, w.saturating_sub(10));
                let artist = fit(&tr.artist, w.saturating_sub(10 + width(&title)));
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark} "), fg(t.accent)),
                    Span::styled(format!("{:>5}  ", duration(tr.duration_sec)), fg(t.dim)),
                    Span::styled(title, fg(t.bright)),
                    Span::styled(format!("  {artist}"), fg(t.warm)),
                ]))
            })
            .collect();
        let widget = list(&t, items, panel(&t, format!("{} · {}", col.name, tracks.len()), focused));
        f.render_stateful_widget(widget, list_area, &mut app.spotify_track_state);
    } else if app.spotify_cols.is_empty() {
        empty_state(
            f,
            &t,
            list_area,
            panel(&t, "Linked playlists", focused),
            "No playlists linked",
            &format!("Link a public playlist, album or export file. OAuth redirect URI: {SPOTIFY_REDIRECT_URI}"),
        );
    } else {
        let items: Vec<ListItem> = app
            .spotify_cols
            .iter()
            .map(|c| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{}  ", c.name), bold(t.bright)),
                    Span::styled(format!("{} · {}", c.track_count, c.subtitle), fg(t.dim)),
                ]))
            })
            .collect();
        let widget = list(&t, items, panel(&t, "Linked playlists", focused));
        f.render_stateful_widget(widget, list_area, &mut app.spotify_col_state);
    }

    let Some(keys) = keys else { return };
    let groups: [(&str, &[(&str, &str)]); 3] = [
        (
            "Playback",
            &[
                ("Space", "play / pause"),
                ("Enter", "play or download"),
                ("n  b", "next / previous"),
                ("← →", "seek 5 seconds"),
                ("+ -", "volume"),
                ("< >", "speed down / up"),
                ("{ }", "pitch down / up"),
                ("t  0", "link / reset speed+pitch"),
                ("z  r", "shuffle / repeat"),
            ],
        ),
        (
            "Browse",
            &[
                ("/  s", "search"),
                ("A", "artist page"),
                ("1-5", "switch tabs"),
                ("D", "downloads"),
                (",", "settings"),
                ("Tab Esc", "switch pane / back"),
                ("q", "quit"),
            ],
        ),
        (
            "Library & lyrics",
            &[
                ("l  f", "like song / follow artist"),
                ("L", "show lyrics"),
                ("S", "re-sync this song's lyrics"),
                ("[ ] \\", "nudge lyrics ∓50ms / reset"),
                ("?", "Genius explanation"),
                ("d", "download / remove"),
                ("Q N P", "queue / new list / add"),
                ("a i c", "get all / import / prune"),
                ("x  v", "censor on/off, sound (settings)"),
            ],
        ),
    ];
    let mut lines = Vec::new();
    for (name, binds) in groups {
        lines.push(Line::from(Span::styled(format!(" {name}"), bold(t.accent))));
        for (k, d) in binds {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k:<10}"), bold(t.gold)),
                Span::styled(d.to_string(), fg(t.text)),
            ]));
        }
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines).block(panel(&t, "Keys", false)), keys);
}

// ---------------------------------------------------------------------------
// Lyrics
// ---------------------------------------------------------------------------

/// Greedy word wrap that returns word indices per row.
fn wrap_words(words: &[&str], max: usize) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0;
    for (i, w) in words.iter().enumerate() {
        let n = width(w);
        match rows.last_mut() {
            Some(row) if used + 1 + n <= max => {
                row.push(i);
                used += 1 + n;
            }
            _ => {
                rows.push(vec![i]);
                used = n;
            }
        }
    }
    rows
}

/// How many characters ahead of the one being sung get a soft glow.
const GLOW: usize = 8;

/// Where the singing currently is, as character positions within the whole line.
/// Also returns where each word starts.
fn reading_heads(words: &[crate::lyrics::LrcWord], pos: f64) -> (Vec<usize>, Vec<usize>) {
    let mut starts = Vec::with_capacity(words.len());
    let mut at = 0;
    let mut singing = Vec::new();
    let mut finished = 0;
    for word in words {
        let n = word.text.chars().count();
        starts.push(at);
        if pos >= word.end {
            finished = finished.max(at + n);
        } else if pos >= word.start {
            singing.push(at + lit_chars(word, pos));
        }
        at += n + 1;
    }
    // Between words (and before the first) the glow waits on what comes next.
    if singing.is_empty() {
        singing.push(finished);
    }
    (starts, singing)
}

/// Characters of the current word that have been reached.
fn lit_chars(word: &crate::lyrics::LrcWord, pos: f64) -> usize {
    let frac = (pos - word.start) / (word.end - word.start).max(0.06);
    let n = word.text.chars().count();
    ((n as f64 * frac).ceil() as usize).clamp(1, n.max(1))
}

/// Brightness (0..1) of an unsung character: strongest right next to a reading head.
fn glow(at: usize, heads: &[usize]) -> f64 {
    heads
        .iter()
        .filter(|&&head| at >= head && at - head < GLOW)
        .map(|&head| 1.0 - (at - head) as f64 / GLOW as f64)
        .fold(0.0, f64::max)
}

fn lyric_rows(app: &App, idx: usize, active: Option<usize>, pos: f64, w: usize) -> Vec<Line<'static>> {
    let t = &app.theme;
    let line = &app.lrc_lines[idx];
    let words: Vec<&str> = line.words.iter().map(|x| x.text.as_str()).collect();
    let is_active = active == Some(idx);
    let plain = {
        let base = match active {
            Some(a) if idx < a => mix(t.bg, t.text, if a - idx == 1 { 0.55 } else { 0.40 }),
            Some(a) => mix(t.bg, t.text, (0.76 - 0.08 * (idx - a).saturating_sub(1) as f64).max(0.44)),
            None => mix(t.bg, t.text, 0.5),
        };
        // While scrolling by hand, keep everything readable.
        if app.lyrics_scroll_offset != 0 {
            mix(t.bg, t.text, 0.65)
        } else {
            base
        }
    };
    let (starts, heads) = reading_heads(&line.words, pos);
    wrap_words(&words, w)
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (n, i) in row.into_iter().enumerate() {
                if n > 0 {
                    spans.push(Span::raw(" "));
                }
                let word = &line.words[i];
                if !is_active {
                    spans.push(Span::styled(word.text.clone(), fg(plain)));
                    continue;
                }
                let (sung, lit) = if word.is_adlib {
                    (t.gold, t.gold)
                } else {
                    (t.bright, t.accent)
                };
                let waiting = mix(t.bg, sung, 0.45);
                let style = |c: Color| {
                    let s = bold(c);
                    if word.is_adlib {
                        s.add_modifier(Modifier::ITALIC)
                    } else {
                        s
                    }
                };
                if pos >= word.end {
                    spans.push(Span::styled(word.text.clone(), style(sung)));
                    continue;
                }
                let current = pos >= word.start;
                let cut = if current { lit_chars(word, pos) } else { 0 };
                let mark = |s: Style| {
                    if current {
                        s.add_modifier(Modifier::UNDERLINED)
                    } else {
                        s
                    }
                };
                if current {
                    let head: String = word.text.chars().take(cut).collect();
                    spans.push(Span::styled(head, mark(style(lit))));
                }
                // The rest lights up softly just ahead of the voice, to read into.
                for (k, c) in word.text.chars().enumerate().skip(cut) {
                    let color = mix(waiting, lit, glow(starts[i] + k, &heads) * 0.75);
                    spans.push(Span::styled(c.to_string(), mark(style(color))));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn lyrics_view(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    app.hits.lyrics_area = area;
    let current = app.player.current_file.clone();
    let mut block = panel(&t, "Lyrics", false);
    if let (Some(file), false) = (&current, app.lrc_lines.is_empty()) {
        let aligned = app.lrc_lines.iter().any(|l| l.aligned);
        let (mut state, color) = if aligned {
            ("● synced".to_string(), t.accent)
        } else if app.aligner.is_pending(file) {
            ("◌ syncing to audio…".to_string(), t.gold)
        } else {
            ("○ estimated".to_string(), t.dim)
        };
        let offset = app.current_track_entry().map(|e| e.lrc_offset_ms).unwrap_or(0);
        if offset != 0 {
            state.push_str(&format!("  {:+.2}s", offset as f64 / 1000.0));
        }
        block = block.title(Line::from(Span::styled(format!(" {state} "), fg(color))).right_aligned());
    }

    if app.lrc_lines.is_empty() {
        let (head, hint) = if current.is_some() {
            ("No synced lyrics", "Press S to search again")
        } else {
            ("Lyrics", "Play a song to see them here")
        };
        empty_state(f, &t, area, block, head, hint);
        return;
    }
    f.render_widget(block, area);

    let body = Rect::new(
        area.x + 3,
        area.y + 1,
        area.width.saturating_sub(6),
        area.height.saturating_sub(2),
    );
    if body.width < 8 || body.height < 2 {
        return;
    }
    let w = body.width.saturating_sub(4) as usize;
    let pos = app.lyric_position();
    let active = app.lrc_lines.iter().rposition(|l| l.time_sec <= pos);
    let last = app.lrc_lines.len() - 1;
    let focus = (active.unwrap_or(0) as isize + app.lyrics_scroll_offset).clamp(0, last as isize) as usize;

    // Long instrumental gaps get progress dots so the screen doesn't look stuck.
    let dots = |idx: usize| -> Option<Line<'static>> {
        let next = app.lrc_lines.get(idx + 1)?.time_sec;
        let sung_until = app.lrc_lines[idx].words.iter().map(|x| x.end).fold(0.0, f64::max);
        if active != Some(idx) || next - sung_until < 5.0 || pos < sung_until {
            return None;
        }
        let done = (((pos - sung_until) / (next - sung_until)) * 4.0) as usize;
        Some(Line::from(vec![
            Span::styled("● ".repeat(done.min(4)), fg(t.accent)),
            Span::styled("· ".repeat(4 - done.min(4)), fg(t.dim)),
        ]))
    };

    let block_of = |idx: usize| {
        let mut rows = lyric_rows(app, idx, active, pos, w);
        rows.extend(dots(idx));
        rows
    };

    // The focused line sits about a third of the way down.
    let focus_rows = block_of(focus);
    let anchor = ((body.height as usize) / 3)
        .min((body.height as usize).saturating_sub(focus_rows.len()));
    let mut rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    for idx in (0..focus).rev() {
        if rows.len() >= anchor {
            break;
        }
        rows.push((None, Line::from("")));
        for line in block_of(idx).into_iter().rev() {
            rows.push((Some(idx), line));
        }
    }
    rows.truncate(anchor);
    while rows.len() < anchor {
        rows.push((None, Line::from("")));
    }
    rows.reverse();
    for (n, idx) in (focus..=last).enumerate() {
        if rows.len() >= body.height as usize {
            break;
        }
        let block = if n == 0 { focus_rows.clone() } else { block_of(idx) };
        rows.extend(block.into_iter().map(|l| (Some(idx), l)));
        rows.push((None, Line::from("")));
    }
    rows.truncate(body.height as usize);

    let (mx, my) = app.mouse_pos;
    let over = rect_contains(area, mx, my);
    for (n, (idx, mut line)) in rows.into_iter().enumerate() {
        let y = body.y + n as u16;
        if let Some(idx) = idx {
            app.hits
                .lyrics_rows
                .push((y, idx, app.lrc_lines[idx].time_sec));
            if over && my == y {
                if active != Some(idx) {
                    for span in &mut line.spans {
                        span.style = span.style.fg(t.bright).add_modifier(Modifier::UNDERLINED);
                    }
                }
                let x = body.x + line.width() as u16 + 1;
                let r = Rect::new(x, y, 3, 1);
                if r.right() <= body.right() {
                    app.hits.lyric_help_btns.push((r, idx));
                    line.spans.push(Span::raw(" "));
                    line.spans.push(Span::styled(
                        "[?]",
                        pill(&t, rect_contains(r, mx, my), t.gold).fg(if rect_contains(r, mx, my) {
                            t.bg
                        } else {
                            t.gold
                        }),
                    ));
                }
            }
        }
        f.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
    }

    if app.lyrics_scroll_offset != 0 && area.width >= 20 && area.height >= 5 {
        let label = " ↩ Back to now ";
        let x = area.x + area.width.saturating_sub(width(label) as u16) / 2;
        let y = area.bottom().saturating_sub(2);
        f.render_widget(Clear, Rect::new(x, y, width(label) as u16, 1));
        button(f, &mut app.hits.buttons, x, y, label, pill(&t, true, t.accent), ButtonAction::LyricsReturn);
    }
}

// ---------------------------------------------------------------------------
// Player bar
// ---------------------------------------------------------------------------

fn player_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    app.hits.controls_bar = area;
    let mut block = panel(&t, "", false);
    let show_status = app.busy
        || app.active_job.is_some()
        || (!app.status.is_empty() && app.status_at.elapsed() < Duration::from_secs(5));
    if show_status {
        let max = area.width.saturating_sub(6) as usize;
        block = block.title(Span::styled(
            format!(" {} ", fit(&app.status, max)),
            fg(t.gold),
        ));
    }
    f.render_widget(block, area);
    let body = inner(area);
    if body.height < 2 || body.width < 40 {
        return;
    }
    let row = Rect::new(body.x + 1, body.y, body.width.saturating_sub(2), 1);

    let playing = app.pb.active && !app.pb.paused;
    let liked = app.current_track_entry().map(|e| e.liked).unwrap_or(false);

    // Transport, centred.
    let repeat = match app.repeat {
        RepeatMode::One => " repeat 1 ",
        _ => " repeat ",
    };
    let toggle = |on: bool| if on { bold(t.accent) } else { fg(t.faint) };
    let transport = [
        (" shuffle ", toggle(app.shuffle), ButtonAction::Shuffle),
        (" ◀◀ ", fg(t.text), ButtonAction::Prev),
        (
            if playing { "  ▐▐  " } else { "  ▶   " },
            pill(&t, true, t.accent),
            ButtonAction::PlayPause,
        ),
        (" ▶▶ ", fg(t.text), ButtonAction::Next),
        (repeat, toggle(app.repeat != RepeatMode::Off), ButtonAction::Repeat),
    ];
    let t_w: u16 = transport.iter().map(|(l, _, _)| width(l) as u16 + 1).sum();

    // Extras, right aligned. Drop the wordy ones on narrow terminals.
    let volume = (app.player.volume as usize + 6) / 12;
    let wide = row.width >= 124;
    let mut extras: Vec<(String, Style, Option<ButtonAction>)> = vec![(
        " ♥ ".into(),
        if liked { bold(t.gold) } else { fg(t.faint) },
        Some(ButtonAction::Like),
    )];
    if wide {
        extras.push((" Artist ".into(), fg(t.dim), Some(ButtonAction::OpenCurrentArtist)));
        extras.push((
            " Queue ".into(),
            toggle(matches!(app.view, View::Queue)),
            Some(ButtonAction::OpenQueue),
        ));
        extras.push((" Lyrics ".into(), toggle(app.show_lyrics_pane), Some(ButtonAction::ToggleLyrics)));
    }
    extras.push((" − ".into(), fg(t.text), Some(ButtonAction::VolDown)));
    if wide {
        extras.push(("━".repeat(volume.min(8)), fg(t.accent), None));
        extras.push(("─".repeat(8 - volume.min(8)), fg(t.faint), None));
    }
    extras.push((format!("{:>3}%", app.player.volume), fg(t.dim), None));
    extras.push((" + ".into(), fg(t.text), Some(ButtonAction::VolUp)));
    let e_w: u16 = extras.iter().map(|(l, _, _)| width(l) as u16).sum();

    let centre = row.x + row.width.saturating_sub(t_w) / 2;
    let right = row.right().saturating_sub(e_w);
    let tx = centre.min(right.saturating_sub(t_w + 1));
    let mut x = tx;
    for (label, style, action) in transport {
        x += button(f, &mut app.hits.buttons, x, row.y, label, style, action) + 1;
    }
    let mut x = right;
    for (label, style, action) in extras {
        let w = width(&label) as u16;
        match action {
            Some(a) => {
                button(f, &mut app.hits.buttons, x, row.y, &label, style, a);
            }
            None => f.render_widget(Paragraph::new(Span::styled(label, style)), Rect::new(x, row.y, w, 1)),
        }
        x += w;
    }

    // Now playing, left.
    let space = tx.saturating_sub(row.x + 2) as usize;
    // Keep room for the censor tag.
    let room = space.saturating_sub(if app.lib.censor { 12 } else { 0 });
    let line = match app.current_track_entry() {
        Some(e) => {
            let title = fit(&e.title, room);
            let left = room.saturating_sub(width(&title) + 2);
            let artist = if left > 3 {
                fit(&clean_artist_name(&e.artist), left)
            } else {
                String::new()
            };
            let tag = match (app.lib.censor, app.lrc_lines.is_empty()) {
                (false, _) => Span::raw(""),
                (true, false) => Span::styled("  clean", fg(t.accent)),
                (true, true) => Span::styled("  uncensored", bold(t.red)),
            };
            Line::from(vec![
                Span::styled(title, bold(t.bright)),
                Span::raw("  "),
                Span::styled(artist, fg(t.warm)),
                tag,
            ])
        }
        None => Line::from(Span::styled(fit("Nothing playing", room), fg(t.dim))),
    };
    f.render_widget(Paragraph::new(line), Rect::new(row.x, row.y, space as u16, 1));

    // Progress line.
    let has = app.pb.dur > 0.1 && app.current_track_entry().is_some();
    let now = format!("{:>5} ", if has { fmt_time(app.smooth_pos) } else { "0:00".into() });
    let end = format!(" {:<5}", if has { fmt_time(app.pb.dur) } else { "–:––".into() });
    let bar_x = row.x + width(&now) as u16;
    let bar_w = row.width.saturating_sub((width(&now) + width(&end)) as u16);
    let ratio = if has {
        (app.smooth_pos / app.pb.dur).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((bar_w.saturating_sub(1)) as f64 * ratio).round() as usize;
    let y = body.y + 1;
    let bar = Line::from(vec![
        Span::styled(now, fg(t.dim)),
        Span::styled("━".repeat(filled), fg(t.accent)),
        Span::styled("●", bold(if has { t.bright } else { t.faint })),
        Span::styled(
            "─".repeat((bar_w as usize).saturating_sub(filled + 1)),
            fg(t.faint),
        ),
        Span::styled(end, fg(t.dim)),
    ]);
    f.render_widget(Paragraph::new(bar), Rect::new(row.x, y, row.width, 1));
    app.hits.progress_bar = Rect::new(bar_x.saturating_sub(1), y, bar_w + 2, 1);

    tune_row(f, app, area);
}

/// Speed and pitch controls, set into the bottom border of the player.
fn tune_row(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let tune = app.lib.tune;
    let value = |changed: bool| if changed { bold(t.accent) } else { fg(t.dim) };
    let defs = [
        (" speed ".to_string(), fg(t.dim), None),
        (" − ".into(), fg(t.text), Some(ButtonAction::Speed(false))),
        (tune.speed_label(), value(tune.speed != 1.0), None),
        (" + ".into(), fg(t.text), Some(ButtonAction::Speed(true))),
        ("  pitch ".into(), fg(t.dim), None),
        (" − ".into(), fg(t.text), Some(ButtonAction::Pitch(false))),
        (format!("{:^8}", tune.pitch_label()), value(tune.semitones != 0.0), None),
        (" + ".into(), fg(t.text), Some(ButtonAction::Pitch(true))),
        (" ".into(), fg(t.dim), None),
        (
            if tune.linked { " linked " } else { " unlinked " }.into(),
            pill(&t, tune.linked, t.accent),
            Some(ButtonAction::ToggleTuneLink),
        ),
        (" reset ".into(), fg(t.text), Some(ButtonAction::ResetTune)),
    ];
    let total: u16 = defs.iter().map(|(l, _, _)| width(l) as u16).sum();
    if area.height < 4 || total + 2 > area.width {
        return;
    }
    let y = area.bottom() - 1;
    let mut x = area.x + (area.width - total) / 2;
    for (label, style, action) in defs {
        let w = width(&label) as u16;
        match action {
            Some(a) => {
                button(f, &mut app.hits.buttons, x, y, &label, style, a);
            }
            None => f.render_widget(Paragraph::new(Span::styled(label, style)), Rect::new(x, y, w, 1)),
        }
        x += w;
    }
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn overlay<'a>(t: &Theme, title: String, color: Color) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(color))
        .style(Style::default().bg(t.bg).fg(t.text))
        .title(Span::styled(format!(" {title} "), bold(color)))
}

fn downloads_menu(f: &mut Frame, app: &mut App, header: Rect) {
    let t = app.theme;
    let screen = f.area();
    let recent = app.recent_downloaded_tracks(10);
    let mut pending: Vec<(String, bool)> = Vec::new();
    if let Some(job) = &app.active_job {
        pending.push((job.label(), true));
    }
    pending.extend(app.jobs.iter().take(4).map(|j| (j.label(), false)));
    let more = app.jobs.len().saturating_sub(4);

    let w = 56u16.min(screen.width.saturating_sub(2));
    let extra = if pending.is_empty() {
        0
    } else {
        pending.len() + 1 + (more > 0) as usize
    };
    let h = ((recent.len().max(1) + extra + 2) as u16).min(screen.height.saturating_sub(4));
    let x = app.hits.dl_btn.right().saturating_sub(w).min(screen.width.saturating_sub(w));
    let r = Rect::new(x, header.bottom(), w, h);
    app.hits.dl_dropdown = r;
    f.render_widget(Clear, r);
    f.render_widget(overlay(&t, "Downloads".into(), t.gold), r);
    let body = inner(r);
    let wide = body.width as usize;

    let mut y = body.y;
    for (label, active) in &pending {
        if y >= body.bottom() {
            return;
        }
        let mark = if *active { "↓" } else { "·" };
        let line = Span::styled(
            fit(&format!(" {mark} {label}"), wide),
            if *active { bold(t.gold) } else { fg(t.dim) },
        );
        f.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
        y += 1;
    }
    if more > 0 {
        let line = Span::styled(format!("   and {more} more"), fg(t.dim));
        f.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
        y += 1;
    }
    if !pending.is_empty() {
        let rule = Span::styled("─".repeat(wide), fg(t.faint));
        f.render_widget(Paragraph::new(rule), Rect::new(body.x, y, body.width, 1));
        y += 1;
    }
    if recent.is_empty() {
        let line = Span::styled(" No downloaded songs yet", fg(t.dim));
        f.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
        return;
    }
    let (mx, my) = app.mouse_pos;
    for (file, title, artist, size) in recent {
        if y >= body.bottom() {
            break;
        }
        let size = fmt_mb(size);
        let left = wide.saturating_sub(width(&size) + 6);
        let label = if artist.is_empty() {
            title
        } else {
            format!("{title} — {artist}")
        };
        let label = fit(&label, left);
        let pad = left.saturating_sub(width(&label));
        let play = Rect::new(body.x, y, body.width.saturating_sub(4), 1);
        let del = Rect::new(body.right().saturating_sub(4), y, 3, 1);
        let line = Line::from(vec![
            Span::styled(
                format!(" {label}{}", " ".repeat(pad)),
                if rect_contains(play, mx, my) {
                    bold(t.accent)
                } else {
                    fg(t.text)
                },
            ),
            Span::styled(format!(" {size}"), fg(t.dim)),
        ]);
        f.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
        let del_style = if rect_contains(del, mx, my) {
            pill(&t, true, t.red)
        } else {
            fg(t.red)
        };
        f.render_widget(Paragraph::new(Span::styled(" ✕ ", del_style)), del);
        app.hits.dl_play_rows.push((play, file.clone()));
        app.hits.dl_del_btns.push((del, file));
        y += 1;
    }
}

fn input_modal(f: &mut Frame, app: &mut App) {
    let t = app.theme;
    let (title, prompt) = match &app.input_mode {
        InputMode::Import => ("Import music", "Link, export .csv, or a file/folder path".to_string()),
        InputMode::SpotifyClientId => (
            "Spotify login",
            format!("Client ID (redirect URI {SPOTIFY_REDIRECT_URI})"),
        ),
        InputMode::SpotifyAddUrl => ("Link Spotify", "Playlist/album URL or .csv/.json path".to_string()),
        InputMode::NewPlaylist => ("New playlist", "Name".to_string()),
        _ => return,
    };
    let screen = f.area();
    let w = 70u16.min(screen.width.saturating_sub(4));
    let r = Rect::new(
        (screen.width.saturating_sub(w)) / 2,
        screen.height.saturating_sub(6) / 2,
        w,
        6u16.min(screen.height),
    );
    app.hits.modal = r;
    f.render_widget(Clear, r);
    let room = w.saturating_sub(7) as usize;
    let n = app.input_buf.chars().count();
    let shown: String = app.input_buf.chars().skip(n.saturating_sub(room)).collect();
    let lines = vec![
        Line::from(Span::styled(format!(" {prompt}"), fg(t.dim))),
        Line::from(""),
        Line::from(Span::styled(format!(" › {shown}▏"), bold(t.bright))),
    ];
    f.render_widget(
        Paragraph::new(lines).block(overlay(&t, title.into(), t.accent)),
        r,
    );
}

fn genius_popup(f: &mut Frame, app: &mut App) {
    let t = app.theme;
    let Some(pop) = app.genius_popup.clone() else {
        return;
    };
    let screen = f.area();
    let w = 72u16.min(screen.width.saturating_sub(4));
    let h = 20u16.min(screen.height.saturating_sub(4));
    let r = Rect::new(
        (screen.width.saturating_sub(w)) / 2,
        (screen.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    app.hits.popup_rect = r;
    f.render_widget(Clear, r);

    let mut lines = vec![
        Line::from(Span::styled(format!(" “{}”", pop.lyric_line), bold(t.accent))),
        Line::from(""),
    ];
    if !pop.fragment.is_empty() && pop.fragment != pop.lyric_line {
        lines.push(Line::from(Span::styled(
            format!(" {}", pop.fragment.replace('\n', " / ")),
            fg(t.gold),
        )));
        lines.push(Line::from(""));
    }
    for para in pop.explanation.lines() {
        lines.push(Line::from(Span::styled(format!(" {para}"), fg(t.text))));
    }
    if !pop.url.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!(" {}", pop.url), fg(t.dim))));
    }
    let title = fit(&format!("Genius · {}", pop.song_title), w.saturating_sub(16) as usize);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay(&t, title, t.gold)),
        r,
    );
    button(
        f,
        &mut app.hits.buttons,
        r.right().saturating_sub(11),
        r.y,
        " Esc ✕ ",
        pill(&t, true, t.gold),
        ButtonAction::ClosePopup,
    );
}
