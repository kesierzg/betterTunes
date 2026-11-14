//! `bt` without a command: a full-screen view for choosing what goes on each
//! device, how it's converted, and whether it fits.

use crate::sync::{self as cli, Seen};
use bettertunes_core::profile::{self, Profile, Storage};
use bettertunes_core::sync::{self, AlbumRef, Conversion, PlaylistRef, Remap, Skip, AAC_RATES};
use bettertunes_core::transcode::{self, Encoder};
use bettertunes_core::Library;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, TableState, Tabs};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread::Scope;
use std::time::Duration;

const TAB_NAMES: [&str; 4] = ["Playlists", "Artists", "Albums", "Genres"];

/// Conversion settings in the order `c` steps through them.
fn conversions() -> Vec<Conversion> {
    let mut all = vec![Conversion::Off];
    all.extend(AAC_RATES.iter().map(|&kbps| Conversion::Aac { kbps }));
    all
}

/// What a row stands for in the selection.
#[derive(Clone)]
enum Key {
    Everything,
    /// Index into the library's playlists.
    Playlist(usize),
    Artist(String),
    Album(AlbumRef),
    Genre(String),
}

struct Item {
    name: String,
    /// "smart" for smart playlists, the artist for albums.
    detail: String,
    key: Key,
    tracks: Vec<u64>,
    /// Bytes on the device at the current conversion.
    size: u64,
}

struct Device {
    profile: Profile,
    connected: bool,
    checking_storage: bool,
    dirty: bool,
}

enum Msg {
    Storage(usize, Option<Storage>),
    Progress {
        done: usize,
        total: usize,
        failed: usize,
    },
    /// Progress of a sync.
    Status(String),
    Finished(String),
}

struct App<'a> {
    lib: &'a Library,
    /// Where the library is, for the header.
    library: String,
    remap: &'a Remap,
    cancel: &'a AtomicBool,
    devices: Vec<Device>,
    current: usize,
    tab: usize,
    items: [Vec<Item>; 4],
    /// Rows shown in the current tab, as indices into its items.
    visible: Vec<usize>,
    table: TableState,
    filter: String,
    searching: bool,
    /// Size of each planned track at the current conversion.
    sizes: HashMap<u64, u64>,
    planned: HashSet<u64>,
    plan_size: u64,
    plan_songs: usize,
    plan_skipped: usize,
    message: String,
    busy: bool,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    quit: bool,
}

/// Runs the TUI until the user quits, saving changed profiles.
pub fn run(lib: &Library, library: &std::path::Path, remap: &Remap) -> io::Result<()> {
    eprintln!("Looking for devices...");
    let (tx, rx) = mpsc::channel();
    let devices = devices(cli::connected(), &tx);
    if devices.is_empty() {
        eprintln!("bt: no device connected or set up yet; connect one and run `bt` again");
        return Ok(());
    }
    let cancel = AtomicBool::new(false);
    let mut app = App::new(lib, remap, &cancel, devices, tx, rx);
    app.library = library.display().to_string();

    let mut terminal = ratatui::init();
    let result = std::thread::scope(|scope| {
        let result = app.event_loop(&mut terminal, scope);
        ratatui::restore();
        if app.busy {
            cancel.store(true, Ordering::Relaxed);
            eprintln!("Waiting for the conversions in progress to finish...");
        }
        result
    });
    app.save_all();
    result
}

/// Profiles for connected devices (storage read in the background) and for
/// the ones set up before.
fn devices(seen: Vec<Seen>, tx: &mpsc::Sender<Msg>) -> Vec<Device> {
    let saved = Profile::all().unwrap_or_default();
    let mut devices = Vec::new();
    for s in seen {
        let mut profile = saved
            .iter()
            .find(|p| p.id == s.id)
            .cloned()
            .unwrap_or_default();
        profile.id = s.id;
        profile.name = s.name;
        profile.model = s.model;
        let index = devices.len();
        let tx = tx.clone();
        let storage = s.storage;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Storage(index, storage()));
        });
        devices.push(Device {
            profile,
            connected: true,
            checking_storage: true,
            dirty: true,
        });
    }
    for p in saved {
        if !devices.iter().any(|d| d.profile.id == p.id) {
            devices.push(Device {
                profile: p,
                connected: false,
                checking_storage: false,
                dirty: false,
            });
        }
    }
    devices
}

fn build_items(lib: &Library) -> [Vec<Item>; 4] {
    let item = |name: String, detail: String, key, tracks| Item {
        name,
        detail,
        key,
        tracks,
        size: 0,
    };
    let mut playlists = vec![item(
        "Entire library".into(),
        String::new(),
        Key::Everything,
        lib.tracks.iter().map(|t| t.persistent_id).collect(),
    )];
    for (i, p) in lib
        .playlists
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_user())
    {
        let detail = if p.is_smart { "smart" } else { "" };
        playlists.push(item(
            p.name.clone(),
            detail.into(),
            Key::Playlist(i),
            p.track_ids.clone(),
        ));
    }

    let sorted = |map: BTreeMap<(String, String, String), Vec<u64>>| map.into_iter();
    let mut artists = BTreeMap::new();
    let mut albums = BTreeMap::new();
    let mut genres = BTreeMap::new();
    for t in &lib.tracks {
        let id = t.persistent_id;
        let mut names: Vec<&String> = t.artist.iter().chain(&t.album_artist).collect();
        names.dedup();
        for a in names {
            artists
                .entry((a.to_lowercase(), a.clone(), String::new()))
                .or_insert_with(Vec::new)
                .push(id);
        }
        if let Some(album) = &t.album {
            let artist = t
                .album_artist
                .as_ref()
                .or(t.artist.as_ref())
                .cloned()
                .unwrap_or_default();
            albums
                .entry((
                    format!("{}\0{}", artist.to_lowercase(), album.to_lowercase()),
                    album.clone(),
                    artist,
                ))
                .or_insert_with(Vec::new)
                .push(id);
        }
        if let Some(g) = &t.genre {
            genres
                .entry((g.to_lowercase(), g.clone(), String::new()))
                .or_insert_with(Vec::new)
                .push(id);
        }
    }
    let artists = sorted(artists)
        .map(|((_, name, _), ids)| item(name.clone(), String::new(), Key::Artist(name), ids))
        .collect();
    let albums = sorted(albums)
        .map(|((_, album, artist), ids)| {
            let key = Key::Album(AlbumRef {
                album: album.clone(),
                artist: (!artist.is_empty()).then(|| artist.clone()),
            });
            item(album, artist, key, ids)
        })
        .collect();
    let genres = sorted(genres)
        .map(|((_, name, _), ids)| item(name.clone(), String::new(), Key::Genre(name), ids))
        .collect();
    [playlists, artists, albums, genres]
}

impl<'a> App<'a> {
    fn new(
        lib: &'a Library,
        remap: &'a Remap,
        cancel: &'a AtomicBool,
        devices: Vec<Device>,
        tx: mpsc::Sender<Msg>,
        rx: mpsc::Receiver<Msg>,
    ) -> App<'a> {
        let mut app = App {
            lib,
            library: String::new(),
            remap,
            cancel,
            devices,
            current: 0,
            tab: 0,
            items: build_items(lib),
            visible: Vec::new(),
            table: TableState::default().with_selected(0),
            filter: String::new(),
            searching: false,
            sizes: HashMap::new(),
            planned: HashSet::new(),
            plan_size: 0,
            plan_songs: 0,
            plan_skipped: 0,
            message: String::new(),
            busy: false,
            tx,
            rx,
            quit: false,
        };
        app.conversion_changed();
        app.refilter();
        app
    }

    fn profile(&self) -> &Profile {
        &self.devices[self.current].profile
    }

    fn event_loop<'s>(
        &mut self,
        terminal: &mut DefaultTerminal,
        scope: &'s Scope<'s, 'a>,
    ) -> io::Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            while let Ok(msg) = self.rx.try_recv() {
                self.handle(msg);
            }
            if event::poll(Duration::from_millis(150))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.key(key, scope);
                    }
                }
            }
        }
        Ok(())
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Storage(i, storage) => {
                let d = &mut self.devices[i];
                d.checking_storage = false;
                if storage.is_some() {
                    d.profile.storage = storage;
                    d.dirty = true;
                }
            }
            Msg::Progress {
                done,
                total,
                failed,
            } => {
                self.message = format!("Converting: {done} of {total} songs");
                if failed > 0 {
                    self.message += &format!(", {failed} failed");
                }
            }
            Msg::Status(text) => self.message = text,
            Msg::Finished(text) => {
                self.busy = false;
                self.message = text;
            }
        }
    }

    fn key<'s>(&mut self, key: KeyEvent, scope: &'s Scope<'s, 'a>) {
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.filter.clear();
                    self.refilter();
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.searching = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return;
        }
        let rows = self.visible.len();
        let at = self.table.selected().unwrap_or(0);
        let move_to =
            |table: &mut TableState, i: usize| table.select(Some(i.min(rows.saturating_sub(1))));
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => move_to(&mut self.table, at + 1),
            KeyCode::Up | KeyCode::Char('k') => move_to(&mut self.table, at.saturating_sub(1)),
            KeyCode::PageDown => move_to(&mut self.table, at + 15),
            KeyCode::PageUp => move_to(&mut self.table, at.saturating_sub(15)),
            KeyCode::Home | KeyCode::Char('g') => move_to(&mut self.table, 0),
            KeyCode::End | KeyCode::Char('G') => move_to(&mut self.table, rows),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Enter | KeyCode::Char('S') => self.start_sync(scope),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.switch_tab(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.switch_tab(3),
            KeyCode::Char('/') => {
                self.searching = true;
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('c') => self.step_conversion(1),
            KeyCode::Char('C') => self.step_conversion(conversions().len() - 1),
            KeyCode::Char('d') => self.switch_device(),
            KeyCode::Char('s') => {
                self.save_all();
                self.message = "Saved.".into();
            }
            KeyCode::Char('p') => self.start_conversion(scope),
            KeyCode::Char('P') => self.start_pull(scope),
            _ => {}
        }
    }

    fn switch_tab(&mut self, by: usize) {
        self.tab = (self.tab + by) % TAB_NAMES.len();
        self.filter.clear();
        self.table.select(Some(0));
        self.refilter();
    }

    fn switch_device(&mut self) {
        if self.devices.len() < 2 {
            self.message = "Only one device is set up.".into();
            return;
        }
        if self.busy {
            self.message = "Wait for the conversion to finish first.".into();
            return;
        }
        self.save_all();
        self.current = (self.current + 1) % self.devices.len();
        self.conversion_changed();
        self.message = format!("Now choosing for {}.", self.profile().name);
    }

    fn step_conversion(&mut self, by: usize) {
        let all = conversions();
        let now = all
            .iter()
            .position(|c| *c == self.profile().conversion)
            .unwrap_or(0);
        let d = &mut self.devices[self.current];
        d.profile.conversion = all[(now + by) % all.len()];
        d.dirty = true;
        self.conversion_changed();
    }

    /// Recomputes every size after the conversion (or device) changed.
    fn conversion_changed(&mut self) {
        let conversion = self.profile().conversion;
        self.sizes = self
            .lib
            .tracks
            .iter()
            .filter_map(|t| Some((t.persistent_id, sync::track_outcome(t, conversion).ok()?.1)))
            .collect();
        for items in &mut self.items {
            for item in items {
                item.size = item.tracks.iter().filter_map(|id| self.sizes.get(id)).sum();
            }
        }
        self.replan();
    }

    fn replan(&mut self) {
        let p = &self.devices[self.current].profile;
        let plan = sync::plan(self.lib, &p.selection, p.conversion, self.remap, false);
        self.planned = plan.items.iter().map(|i| i.track.persistent_id).collect();
        self.plan_size = plan.total_size();
        self.plan_songs = plan.items.len();
        self.plan_skipped = plan.skipped.len();
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self.items[self.tab]
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                needle.is_empty()
                    || it.name.to_lowercase().contains(&needle)
                    || it.detail.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        let at = self.table.selected().unwrap_or(0);
        self.table
            .select(Some(at.min(self.visible.len().saturating_sub(1))));
    }

    fn is_selected(&self, key: &Key) -> bool {
        let sel = &self.profile().selection;
        match key {
            Key::Everything => sel.everything,
            Key::Playlist(i) => sel
                .playlists
                .iter()
                .any(|r| r.matches(&self.lib.playlists[*i])),
            Key::Artist(a) => sel.artists.contains(a),
            Key::Album(a) => sel.albums.contains(a),
            Key::Genre(g) => sel.genres.contains(g),
        }
    }

    fn toggle(&mut self) {
        let Some(&index) = self.table.selected().and_then(|i| self.visible.get(i)) else {
            return;
        };
        let key = self.items[self.tab][index].key.clone();
        let on = !self.is_selected(&key);
        let lib = self.lib;
        let d = &mut self.devices[self.current];
        let sel = &mut d.profile.selection;
        fn set<T: PartialEq>(list: &mut Vec<T>, item: T, on: bool) {
            list.retain(|x| *x != item);
            if on {
                list.push(item);
            }
        }
        match key {
            Key::Everything => sel.everything = on,
            Key::Playlist(i) => {
                let p = &lib.playlists[i];
                sel.playlists.retain(|r| !r.matches(p));
                if on {
                    sel.playlists.push(PlaylistRef::new(p));
                }
            }
            Key::Artist(a) => set(&mut sel.artists, a, on),
            Key::Album(a) => set(&mut sel.albums, a, on),
            Key::Genre(g) => set(&mut sel.genres, g, on),
        }
        d.dirty = true;
        self.replan();
    }

    fn start_conversion<'s>(&mut self, scope: &'s Scope<'s, 'a>) {
        if self.busy {
            self.message = "Wait for the current conversion or sync to finish.".into();
            return;
        }
        let p = self.profile().clone();
        if p.selection.is_empty() {
            self.message = "Choose something to sync first.".into();
            return;
        }
        self.busy = true;
        self.message = "Checking song files...".into();
        let (lib, remap, cancel, tx) = (self.lib, self.remap, self.cancel, self.tx.clone());
        scope.spawn(move || {
            let text = convert(lib, remap, &p, cancel, &tx);
            let _ = tx.send(Msg::Finished(text));
        });
    }

    fn start_sync<'s>(&mut self, scope: &'s Scope<'s, 'a>) {
        if self.busy {
            self.message = "Wait for the current conversion or sync to finish.".into();
            return;
        }
        let d = &self.devices[self.current];
        if !d.connected {
            self.message = format!("{} isn't connected.", d.profile.name);
            return;
        }
        if d.profile.selection.is_empty() {
            self.message = "Choose something to sync first.".into();
            return;
        }
        self.save_all();
        let p = self.profile().clone();
        self.busy = true;
        self.message = format!("Syncing {}...", p.name);
        let (lib, remap, tx) = (self.lib, self.remap, self.tx.clone());
        scope.spawn(move || {
            let Some(device) = crate::push::find(&p.id) else {
                let _ = tx.send(Msg::Finished(format!(
                    "{} isn't connected over USB.",
                    p.name
                )));
                return;
            };
            let status = Mutex::new(tx.clone());
            let progress = |e: crate::push::Event| {
                let text = match e {
                    crate::push::Event::Step(s) => format!("{s}..."),
                    crate::push::Event::Converting { done, total } => {
                        format!("Converting: {done} of {total} songs")
                    }
                    crate::push::Event::Copying { done, total, track } => format!(
                        "Copying {} of {total}: {}",
                        done + 1,
                        cli::track_name(track)
                    ),
                };
                let _ = status.lock().unwrap().send(Msg::Status(text));
            };
            let text = match crate::push::run(&device, lib, &p, remap, &progress) {
                Ok(r) => {
                    let mut text = format!(
                        "Synced: {} songs, {} playlists (copied {}, kept {}, removed {}).",
                        r.songs, r.playlists, r.copied, r.kept, r.removed
                    );
                    if !r.failed.is_empty() {
                        text += &format!(
                            " {} couldn't be synced (see `bt sync run`).",
                            r.failed.len()
                        );
                    }
                    text
                }
                Err(e) => format!("Sync failed: {e}"),
            };
            let _ = tx.send(Msg::Finished(text));
        });
    }

    /// Brings the device's changes back into iTunes (or saves the script).
    fn start_pull<'s>(&mut self, scope: &'s Scope<'s, 'a>) {
        if self.busy {
            self.message = "Wait for the current conversion or sync to finish.".into();
            return;
        }
        let d = &self.devices[self.current];
        if !d.connected {
            self.message = format!("{} isn't connected.", d.profile.name);
            return;
        }
        let p = self.profile().clone();
        self.busy = true;
        self.message = format!("Reading changes from {}...", p.name);
        let (lib, tx) = (self.lib, self.tx.clone());
        scope.spawn(move || {
            let text = (|| {
                let device = crate::push::find(&p.id)
                    .ok_or(format!("{} isn't connected over USB.", p.name))?;
                let pulled = crate::pull::gather(&device, &p, lib)?;
                if pulled.changes.is_empty() {
                    return Ok(format!("Nothing to bring back from {}.", p.name));
                }
                let c = &pulled.changes;
                let summary = format!(
                    "{} playlists to add to, {} new, {} copies to remove, {} songs' play counts. ",
                    c.additions.len(),
                    c.new_playlists.len(),
                    c.copies.len(),
                    c.stats.len()
                );
                crate::pull::apply(c, &p, None).map(|m| summary + &m.replace('\n', " "))
            })()
            .unwrap_or_else(|e: String| format!("Pull failed: {e}"));
            let _ = tx.send(Msg::Finished(text));
        });
    }

    fn save_all(&mut self) {
        for d in &mut self.devices {
            if d.dirty {
                match d.profile.save() {
                    Ok(()) => d.dirty = false,
                    Err(e) => self.message = format!("Couldn't save {}: {e}", d.profile.name),
                }
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [header, tabs, list, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        let titles = TAB_NAMES
            .iter()
            .zip(&self.items)
            .map(|(name, items)| format!(" {name} ({}) ", items.len()));
        f.render_widget(
            Tabs::new(titles)
                .select(self.tab)
                .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan))
                .divider(""),
            tabs,
        );
        self.draw_list(f, list);
        self.draw_footer(f, footer);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let d = &self.devices[self.current];
        let p = &d.profile;
        let mut first = vec![
            Span::styled(
                " betterTunes ",
                Style::new().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled(p.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {}", p.model)),
        ];
        if self.devices.len() > 1 {
            first.push(Span::styled(
                format!(
                    "   (device {} of {}, d to switch)",
                    self.current + 1,
                    self.devices.len()
                ),
                Style::new().fg(Color::DarkGray),
            ));
        }
        let second = if d.checking_storage {
            "Checking how much room it has...".to_owned()
        } else {
            let state = if d.connected {
                "connected"
            } else {
                "not connected"
            };
            match p.storage {
                Some(s) => format!(
                    "{state}, {} total, {} room for music",
                    cli::size(s.total),
                    cli::size(s.room_for_music())
                ),
                None => format!("{state}, storage unknown"),
            }
        };
        let dim = Style::new().fg(Color::DarkGray);
        let text = vec![
            Line::from(first),
            Line::styled(format!(" {second}"), dim),
            Line::styled(format!(" Library: {}", self.library), dim),
        ];
        f.render_widget(Paragraph::new(text), area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let items = &self.items[self.tab];
        let rows: Vec<Row> = self
            .visible
            .iter()
            .map(|&i| {
                let it = &items[i];
                let mark = if self.is_selected(&it.key) {
                    Span::styled(
                        "[x]",
                        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                    )
                } else if it.tracks.iter().any(|id| self.planned.contains(id)) {
                    Span::styled("[~]", Style::new().fg(Color::Yellow))
                } else {
                    Span::raw("[ ]")
                };
                Row::new(vec![
                    Cell::from(mark),
                    Cell::from(it.name.clone()),
                    Cell::from(Span::styled(
                        it.detail.clone(),
                        Style::new().fg(Color::DarkGray),
                    )),
                    Cell::from(Line::from(it.tracks.len().to_string()).right_aligned()),
                    Cell::from(Line::from(cli::size(it.size)).right_aligned()),
                ])
            })
            .collect();
        let detail_width = if self.tab == 2 { 24 } else { 6 };
        let title = if self.searching || !self.filter.is_empty() {
            let cursor = if self.searching { "_" } else { "" };
            format!(
                " Search: {}{cursor}  ({} found) ",
                self.filter,
                self.visible.len()
            )
        } else {
            format!(" {} ", TAB_NAMES[self.tab])
        };
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Fill(1),
                Constraint::Length(detail_width),
                Constraint::Length(6),
                Constraint::Length(9),
            ],
        )
        .header(
            Row::new([
                Cell::from(""),
                Cell::from("Name"),
                Cell::from(""),
                Cell::from(Line::from("Songs").right_aligned()),
                Cell::from(Line::from("Size").right_aligned()),
            ])
            .style(Style::new().fg(Color::DarkGray)),
        )
        .block(
            Block::new()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(title),
        )
        .row_highlight_style(
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .column_spacing(1);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let [gauge, info, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(area);
        let p = self.profile();
        let room = p.storage.map(|s| s.room_for_music());
        let (ratio, color, verdict) = match room {
            Some(room) if room > 0 => {
                let r = self.plan_size as f64 / room as f64;
                let verdict = if self.plan_size <= room {
                    format!("fits, {} free", cli::size(room - self.plan_size))
                } else {
                    format!("{} too much", cli::size(self.plan_size - room))
                };
                (
                    r.min(1.0),
                    if r <= 1.0 { Color::Green } else { Color::Red },
                    verdict,
                )
            }
            _ => (0.0, Color::Gray, "room unknown".into()),
        };
        let label = format!(
            "{} songs, {} of {} ({verdict})",
            self.plan_songs,
            cli::size(self.plan_size),
            room.map_or_else(|| "?".into(), cli::size)
        );
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(label)
                .gauge_style(Style::new().fg(color).bg(Color::Black)),
            gauge,
        );
        let mut lines = vec![Line::from(vec![
            Span::styled(" Conversion: ", Style::new().fg(Color::DarkGray)),
            Span::raw(cli::describe_conversion(p.conversion)),
        ])];
        let status = if !self.message.is_empty() {
            self.message.clone()
        } else if self.plan_skipped > 0 {
            format!(
                "{} chosen songs can't be synced (not downloaded, video or DRM)",
                self.plan_skipped
            )
        } else {
            String::new()
        };
        lines.push(Line::styled(
            format!(" {status}"),
            Style::new().fg(Color::Yellow),
        ));
        f.render_widget(Paragraph::new(lines), info);
        let help = if self.searching {
            " type to search   enter done   esc clear"
        } else {
            " space choose  enter sync  P pull  ←/→ lists  / search  c/C conversion  d device  p convert  q quit"
        };
        f.render_widget(
            Paragraph::new(help).style(Style::new().fg(Color::Black).bg(Color::Gray)),
            keys,
        );
    }
}

/// Converts what the profile needs into the cache, reporting progress.
fn convert(
    lib: &Library,
    remap: &Remap,
    p: &Profile,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<Msg>,
) -> String {
    let plan = sync::plan(lib, &p.selection, p.conversion, remap, true);
    let missing = plan
        .skipped
        .iter()
        .filter(|(_, s)| *s == Skip::Missing)
        .count();
    let missing = if missing > 0 {
        format!(" {missing} song files weren't found.")
    } else {
        String::new()
    };
    let cache = match profile::cache_dir() {
        Ok(c) => c,
        Err(e) => return e.to_string(),
    };
    let (todo, cached) = transcode::jobs(&plan, &cache, remap);
    if todo.is_empty() {
        return format!("Nothing to convert ({cached} already done).{missing}");
    }
    let encoders = Encoder::available();
    if encoders.is_empty() {
        return "No AAC encoder found; install ffmpeg.".into();
    }
    let failed = Mutex::new(0);
    let total = todo.len();
    transcode::run(
        &todo,
        &encoders,
        transcode::default_threads(),
        |done, _, result| {
            let mut failed = failed.lock().unwrap();
            *failed += usize::from(result.is_err());
            let _ = tx.send(Msg::Progress {
                done,
                total,
                failed: *failed,
            });
            !cancel.load(Ordering::Relaxed)
        },
    );
    let failed = failed.into_inner().unwrap();
    if cancel.load(Ordering::Relaxed) {
        return "Conversion stopped.".into();
    }
    let failed = if failed > 0 {
        format!(", {failed} failed (see `bt sync prepare`)")
    } else {
        String::new()
    };
    format!("Converted {} songs{failed}.{missing}", total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bettertunes_core::{Playlist, Track};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn device(storage: Option<Storage>) -> Device {
        Device {
            profile: Profile {
                id: "abc".into(),
                name: "Test iPod".into(),
                model: "iPod touch".into(),
                storage,
                ..Profile::default()
            },
            connected: true,
            checking_storage: false,
            dirty: false,
        }
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let line: String = (0..width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect();
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App<'_>, codes: &[KeyCode]) {
        std::thread::scope(|s| {
            for &code in codes {
                app.key(KeyEvent::new(code, KeyModifiers::NONE), s);
            }
        });
    }

    fn library() -> Library {
        let track = |id, artist: &str, mb: u64| Track {
            persistent_id: id,
            title: Some(format!("song {id}")),
            artist: Some(artist.into()),
            album: Some("Album".into()),
            genre: Some("Pop".into()),
            duration_ms: Some(200_000),
            size: Some(mb << 20),
            bit_rate: Some(1000),
            location: Some(PathBuf::from(format!("/m/{id}.m4a"))),
            ..Track::default()
        };
        Library {
            tracks: vec![
                track(1, "Alpha Band", 30),
                track(2, "Yellow Lake", 40),
                track(3, "Alpha Band", 50),
            ],
            playlists: vec![
                Playlist {
                    name: "####!####".into(),
                    is_master: true,
                    track_ids: vec![1, 2, 3],
                    ..Playlist::default()
                },
                Playlist {
                    name: "Music".into(),
                    distinguished_kind: 4,
                    track_ids: vec![1, 2, 3],
                    ..Playlist::default()
                },
                Playlist {
                    persistent_id: Some(7),
                    name: "★ night ☾".into(),
                    track_ids: vec![2, 3],
                    ..Playlist::default()
                },
            ],
        }
    }

    #[test]
    fn chooses_and_shows_the_fit() {
        let lib = library();
        let remap = Remap::default();
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        // 200 MB is kept free, leaving about 60 MB for music.
        let storage = Storage {
            total: 2 << 30,
            free: 260 << 20,
            music: 0,
        };
        let mut app = App::new(&lib, &remap, &cancel, vec![device(Some(storage))], tx, rx);
        let s = screen(&mut app, 90, 16);
        assert!(s.contains("Test iPod"), "{s}");
        assert!(s.contains("Playlists (2)"), "{s}");
        // Built-in playlists are hidden.
        assert!(!s.contains("Music "), "{s}");
        assert!(s.contains("0 songs, 0 MB"), "{s}");

        // Choose the playlist: two lossless songs, too big for the room.
        press(&mut app, &[KeyCode::Down, KeyCode::Char(' ')]);
        let s = screen(&mut app, 90, 16);
        assert!(s.contains("[x] ★"), "{s}");
        assert!(s.contains("2 songs, 94 MB"), "{s}");
        assert!(s.contains("too much"), "{s}");

        // Converting to 128 kbps makes it fit.
        press(&mut app, &[KeyCode::Char('c')]);
        let s = screen(&mut app, 90, 16);
        assert!(s.contains("128 kbps AAC"), "{s}");
        assert!(s.contains("fits"), "{s}");

        // Artists already covered by the playlist are marked as partly in.
        press(&mut app, &[KeyCode::Tab]);
        let s = screen(&mut app, 90, 16);
        assert!(s.contains("[~] Alpha Band"), "{s}");

        // Searching narrows the list.
        press(
            &mut app,
            &[
                KeyCode::Char('/'),
                KeyCode::Char('y'),
                KeyCode::Char('e'),
                KeyCode::Enter,
            ],
        );
        let s = screen(&mut app, 90, 16);
        assert!(
            s.contains("Yellow Lake") && !s.contains("Alpha Band"),
            "{s}"
        );
        assert!(app.devices[0].dirty);
    }

    /// Prints the screen for a real library: `BT_ITL=... cargo test -p
    /// bettertunes-cli shows_a_real_library -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs BT_ITL pointing at a real library"]
    fn shows_a_real_library() {
        let lib = Library::open(std::env::var("BT_ITL").unwrap().as_ref()).unwrap();
        let remap = Remap::default();
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let storage = Storage {
            total: 15_925_174_272,
            free: 15_895_154_688,
            music: 0,
        };
        let mut app = App::new(&lib, &remap, &cancel, vec![device(Some(storage))], tx, rx);
        press(
            &mut app,
            &[
                KeyCode::Char('c'),
                KeyCode::Char('c'),
                KeyCode::Char('c'),
                KeyCode::Char('c'),
                KeyCode::Char('c'),
            ],
        );
        press(
            &mut app,
            &[
                KeyCode::Down,
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Char(' '),
            ],
        );
        println!("{}", screen(&mut app, 100, 30));
        press(
            &mut app,
            &[
                KeyCode::Tab,
                KeyCode::Char('/'),
                KeyCode::Char('b'),
                KeyCode::Char('l'),
                KeyCode::Char('a'),
                KeyCode::Enter,
            ],
        );
        println!("{}", screen(&mut app, 100, 14));
    }

    /// Syncs the connected touch through the TUI's own code path:
    /// `BT_TUI_SYNC=1 cargo test -p bettertunes-cli tui_sync -- --ignored --nocapture`.
    #[test]
    #[ignore = "syncs a real device"]
    fn tui_sync() {
        assert!(std::env::var_os("BT_TUI_SYNC").is_some());
        let settings = profile::Settings::load().unwrap();
        let lib = Library::open(settings.library.as_ref().unwrap()).unwrap();
        let remap = Remap(settings.remap.into_iter().map(|m| (m.from, m.to)).collect());
        let cancel = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let devices = devices(cli::connected(), &tx);
        let mut app = App::new(&lib, &remap, &cancel, devices, tx, rx);
        let touch = app
            .devices
            .iter()
            .position(|d| d.profile.model.contains("iPod touch"))
            .unwrap();
        app.current = touch;
        app.conversion_changed();
        std::thread::scope(|s| {
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), s);
            println!("after enter: {}", app.message);
            while app.busy {
                if let Ok(msg) = app.rx.recv_timeout(Duration::from_secs(600)) {
                    let done = matches!(msg, Msg::Finished(_));
                    app.handle(msg);
                    if done {
                        println!("finished: {}", app.message);
                    }
                }
            }
        });
    }
}
