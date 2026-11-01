//! midifon: GUI на wxdragon поверх midifon-core.
//!
//! Модель как у редактора: есть один текущий документ (запись или открытый файл).
//! Запись идёт во временный файл, Save / Save As копируют его туда, куда скажет пользователь.
//! Главный экран минимальный (только журнал), всё управление и все настройки живут в меню.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use midifon_core::devices;
use midifon_core::engine::{Engine, Notice, Notifier, Phase, Playback, StartOptions, Started};
use midifon_core::settings::{fmt_secs, parse_secs, Settings};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use wxdragon::prelude::*;

// File
const ID_NEW: Id = ID_HIGHEST + 1;
const ID_OPEN: Id = ID_HIGHEST + 2;
const ID_SAVE: Id = ID_HIGHEST + 3;
const ID_SAVEAS: Id = ID_HIGHEST + 4;
const ID_CLOSE: Id = ID_HIGHEST + 5;
const ID_NEXT_TAB: Id = ID_HIGHEST + 6;
const ID_PREV_TAB: Id = ID_HIGHEST + 7;
// Record and play
const ID_REC: Id = ID_HIGHEST + 10;
const ID_PLAY: Id = ID_HIGHEST + 11;
const ID_PLAY_BEGIN: Id = ID_HIGHEST + 12;
const ID_STOP: Id = ID_HIGHEST + 13;
// Options
const ID_OPT_AUTO: Id = ID_HIGHEST + 20;
const ID_OPT_PAUSE: Id = ID_HIGHEST + 21;
const ID_OPT_PAUSE_LEN: Id = ID_HIGHEST + 22;
const ID_OPT_TIMER: Id = ID_HIGHEST + 23;
const ID_OPT_KEEP: Id = ID_HIGHEST + 24;
const ID_OPT_PREROLL: Id = ID_HIGHEST + 25;
// Devices
const ID_DEV_IN: Id = ID_HIGHEST + 30;
const ID_DEV_OUT: Id = ID_HIGHEST + 31;
const ID_DEV_REFRESH: Id = ID_HIGHEST + 32;
// Help
const ID_KEYS: Id = ID_HIGHEST + 40;

struct Doc {
    id: u64,
    /// Файл, который играем и сохраняем (временный дубль или открытый файл).
    file: Option<PathBuf>,
    /// Куда документ сохранён пользователем.
    saved_as: Option<PathBuf>,
    /// Есть несохранённая запись.
    dirty: bool,
}

impl Doc {
    fn new(id: u64) -> Doc {
        Doc { id, file: None, saved_as: None, dirty: false }
    }

    fn name(&self) -> String {
        match &self.saved_as {
            Some(p) => p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| fname(p)),
            None => "untitled".into(),
        }
    }

    /// Имя с звёздочкой, если есть несохранённые изменения.
    fn label(&self) -> String {
        format!("{}{}", self.name(), if self.dirty { "*" } else { "" })
    }
}

struct State {
    frame: Frame,
    status: StatusBar,
    notebook: Notebook,
    engine: RefCell<Engine>,
    notices: Arc<Mutex<VecDeque<Notice>>>,
    /// Документы в порядке вкладок.
    docs: RefCell<Vec<Doc>>,
    next_id: Cell<u64>,
    /// Вкладка, которой принадлежит идущая запись или воспроизведение.
    owner: Cell<Option<u64>>,
    takes_dir: PathBuf,
    last_sig: RefCell<String>,
}

type S = Rc<State>;

fn fname(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Единственная точка вывода сообщений: строка состояния.
/// Сюда же подключится озвучивание через NVDA.
fn say(s: &State, msg: &str) {
    s.status.set_status_text(msg, 0);
}

fn describe(n: &Notice) -> String {
    match n {
        Notice::TakeStarted { .. } => "Note detected. Recording.".into(),
        Notice::PauseSplit { saved } => format!("Detected pause, saved {}. Waiting for the next notes.", fname(saved)),
        Notice::PauseStopped { .. } => "Detected pause. Recording stopped.".into(),
        Notice::TimerElapsed { saved: Some(_) } => "Time limit reached. Recording stopped.".into(),
        Notice::TimerElapsed { saved: None } => "Time limit reached. Recording stopped, nothing was recorded.".into(),
        Notice::PlaybackFinished => "Playback finished.".into(),
        Notice::Error(e) => format!("Error: {e}"),
    }
}

// ───────────── вкладки ─────────────

fn cur(s: &State) -> usize {
    let n = s.docs.borrow().len();
    (s.notebook.selection().max(0) as usize).min(n.saturating_sub(1))
}

fn cur_id(s: &State) -> u64 {
    s.docs.borrow()[cur(s)].id
}

fn idx_of(s: &State, id: u64) -> Option<usize> {
    s.docs.borrow().iter().position(|d| d.id == id)
}

/// Новая пустая вкладка (рабочее поле пустое). Возвращает её индекс.
fn add_tab(s: &State) -> usize {
    let id = s.next_id.get();
    s.next_id.set(id + 1);
    let panel = Panel::builder(&s.notebook).build();
    s.docs.borrow_mut().push(Doc::new(id));
    s.notebook.add_page(&panel, "untitled", true, None);
    s.docs.borrow().len() - 1
}

fn doc_is_empty(d: &Doc) -> bool {
    d.file.is_none()
}

// ───────────── меню ─────────────

fn item_label(s: &State, id: Id, label: &str) {
    if let Some(bar) = s.frame.get_menu_bar() {
        if let Some(item) = bar.find_item(id) {
            item.set_label(label);
        }
    }
}

fn checked(s: &State, id: Id) -> bool {
    s.frame.get_menu_bar().map(|b| b.is_item_checked(id)).unwrap_or(false)
}

/// Подписи настроек с текущими значениями и флажки.
fn update_options_menu(s: &State) {
    let st = s.engine.borrow().settings();
    if let Some(bar) = s.frame.get_menu_bar() {
        bar.check_item(ID_OPT_AUTO, st.auto_start);
        bar.check_item(ID_OPT_PAUSE, st.pause_split);
        bar.check_item(ID_OPT_KEEP, st.keep_controllers);
    }
    item_label(s, ID_OPT_PAUSE_LEN, &format!("Pause &length: {} s...", st.pause_secs));
    let timer = st.timer_secs.map(fmt_secs).unwrap_or_else(|| "off".into());
    item_label(s, ID_OPT_TIMER, &format!("&Time limit: {timer}..."));
    item_label(s, ID_OPT_PREROLL, &format!("&Pre-roll: {} ms...", st.preroll_ms));
    item_label(s, ID_DEV_IN, &format!("&Input device: {}...", st.input.as_deref().unwrap_or("none")));
    item_label(s, ID_DEV_OUT, &format!("&Output device: {}...", st.output.as_deref().unwrap_or("none")));
}

/// Состояние меню, заголовка окна и подписей вкладок. Вызывается только при изменении состояния.
fn update_state_ui(s: &State) {
    let ci = cur(s);
    let (status, pb_raw) = {
        let e = s.engine.borrow();
        (e.status(), e.playback())
    };
    let (cid, labels) = {
        let d = s.docs.borrow();
        (d[ci].id, d.iter().map(|x| x.label()).collect::<Vec<_>>())
    };
    let pb = if s.owner.get() == Some(cid) { pb_raw } else { Playback::Stopped };
    let rec_active = status.phase != Phase::Idle;

    item_label(s, ID_REC, if rec_active { "Stop &recording\tCtrl+R" } else { "&Record\tCtrl+R" });
    let play_label = if rec_active {
        "&Stop recording\tSpace"
    } else {
        match pb {
            Playback::Playing => "&Pause\tSpace",
            Playback::Paused => "&Resume\tSpace",
            Playback::Stopped => "&Play\tSpace",
        }
    };
    item_label(s, ID_PLAY, play_label);

    for (i, l) in labels.iter().enumerate() {
        if s.notebook.get_page_text(i).as_deref() != Some(l.as_str()) {
            s.notebook.set_page_text(i, l);
        }
    }
    s.frame.set_title(&format!("{} - midifon", labels[ci]));
}

// ───────────── диалоги ─────────────

fn ask_text(s: &State, title: &str, msg: &str, default: &str) -> Option<String> {
    let dlg = TextEntryDialog::builder(&s.frame, msg, title).with_default_value(default).build();
    if dlg.show_modal() == ID_OK {
        dlg.get_value()
    } else {
        None
    }
}

/// Выбор из списка. Возвращает индекс.
fn ask_choice(s: &State, title: &str, msg: &str, names: &[String], current: Option<&str>) -> Option<usize> {
    let refs: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
    let dlg = SingleChoiceDialog::builder(&s.frame, msg, title, &refs).build();
    if let Some(i) = current.and_then(|c| names.iter().position(|n| n == c)) {
        dlg.set_selection(i as i32);
    }
    if dlg.show_modal() == ID_OK {
        let i = dlg.get_selection();
        if i >= 0 {
            return Some(i as usize);
        }
    }
    None
}

/// Если во вкладке есть несохранённая запись, спрашивает, что делать. true — можно продолжать.
fn confirm_save(s: &State, idx: usize) -> bool {
    let (dirty, label) = {
        let d = s.docs.borrow();
        (d[idx].dirty, d[idx].label())
    };
    if !dirty {
        return true;
    }
    if s.notebook.selection() != idx as i32 {
        s.notebook.set_selection(idx);
    }
    let dlg = MessageDialog::builder(&s.frame, &format!("{label} is not saved. Save it first?"), "midifon")
        .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::Cancel | MessageDialogStyle::IconQuestion)
        .build();
    match dlg.show_modal() {
        ID_YES => save(s, idx),
        ID_NO => true,
        _ => false,
    }
}

// ───────────── документ ─────────────

fn set_new_take(s: &State, id: u64, file: PathBuf) {
    if let Some(i) = idx_of(s, id) {
        let mut d = s.docs.borrow_mut();
        d[i].file = Some(file);
        d[i].saved_as = None;
        d[i].dirty = true;
    }
}

fn do_new(s: &State) {
    add_tab(s);
    say(s, "New tab. Control R records.");
    refresh_state(s, true);
}

fn do_close_tab(s: &State) {
    let idx = cur(s);
    let id = s.docs.borrow()[idx].id;
    if s.owner.get() == Some(id) {
        stop_all(s);
    }
    if !confirm_save(s, idx) {
        return;
    }
    s.notebook.remove_page(idx);
    s.docs.borrow_mut().remove(idx);
    if s.docs.borrow().is_empty() {
        add_tab(s);
    }
    say(s, "Tab closed.");
    refresh_state(s, true);
}

fn do_open(s: &State) {
    let dir = s.engine.borrow().settings().dir.to_string_lossy().into_owned();
    let dlg = FileDialog::builder(&s.frame)
        .with_message("Open MIDI file")
        .with_default_dir(&dir)
        .with_wildcard("MIDI files (*.mid;*.midi)|*.mid;*.midi")
        .build();
    if dlg.show_modal() != ID_OK {
        return;
    }
    let Some(p) = dlg.get_path() else { return };
    let p = PathBuf::from(p);
    // пустую вкладку используем, иначе открываем новую
    let idx = if doc_is_empty(&s.docs.borrow()[cur(s)]) { cur(s) } else { add_tab(s) };
    {
        let mut d = s.docs.borrow_mut();
        d[idx].file = Some(p.clone());
        d[idx].saved_as = Some(p.clone());
        d[idx].dirty = false;
    }
    s.notebook.set_selection(idx);
    say(s, &format!("Opened {}. Space plays.", fname(&p)));
    refresh_state(s, true);
}

/// Save: true, если вкладка теперь сохранена.
fn save(s: &State, idx: usize) -> bool {
    let (file, saved_as) = {
        let d = s.docs.borrow();
        (d[idx].file.clone(), d[idx].saved_as.clone())
    };
    let Some(file) = file else {
        say(s, "Nothing to save.");
        return false;
    };
    match saved_as {
        Some(target) => {
            if target != file {
                if let Err(e) = std::fs::copy(&file, &target) {
                    say(s, &format!("Error: cannot save {}: {e}", fname(&target)));
                    return false;
                }
            }
            s.docs.borrow_mut()[idx].dirty = false;
            say(s, &format!("Saved {}.", fname(&target)));
            refresh_state(s, true);
            true
        }
        None => save_as(s, idx),
    }
}

fn save_as(s: &State, idx: usize) -> bool {
    let (file, default_name) = {
        let d = s.docs.borrow();
        (d[idx].file.clone(), d[idx].saved_as.as_deref().map(fname).unwrap_or_else(|| "recording.mid".into()))
    };
    let Some(file) = file else {
        say(s, "Nothing to save.");
        return false;
    };
    let dir = s.engine.borrow().settings().dir;
    let _ = std::fs::create_dir_all(&dir);
    let dlg = FileDialog::builder(&s.frame)
        .with_message("Save MIDI file")
        .with_default_dir(&dir.to_string_lossy())
        .with_default_file(&default_name)
        .with_wildcard("MIDI files (*.mid)|*.mid")
        .with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
        .build();
    if dlg.show_modal() != ID_OK {
        return false;
    }
    let Some(p) = dlg.get_path() else { return false };
    let mut target = PathBuf::from(p);
    if target.extension().is_none() {
        target.set_extension("mid");
    }
    if target != file {
        if let Err(e) = std::fs::copy(&file, &target) {
            say(s, &format!("Error: cannot save {}: {e}", fname(&target)));
            return false;
        }
    }
    if let Some(parent) = target.parent() {
        let parent = parent.to_path_buf();
        let _ = s.engine.borrow().update(|st| st.dir = parent);
    }
    {
        let mut d = s.docs.borrow_mut();
        d[idx].saved_as = Some(target.clone());
        d[idx].dirty = false;
    }
    say(s, &format!("Saved {}.", fname(&target)));
    refresh_state(s, true);
    true
}

// ───────────── запись и воспроизведение ─────────────

fn start_recording(s: &State) {
    if s.engine.borrow().playback() != Playback::Stopped {
        s.engine.borrow_mut().stop();
    }
    if !s.engine.borrow().input_connected() {
        say(s, "No input device. Choose one in the Devices menu, Input device.");
        return;
    }
    // записываем в пустую вкладку, иначе в новую: ничего не затирается
    if !doc_is_empty(&s.docs.borrow()[cur(s)]) {
        add_tab(s);
    }
    let id = cur_id(s);
    let opts = StartOptions { name: Some("take".into()), dir: Some(s.takes_dir.clone()), pause_stops: true };
    let res = s.engine.borrow().start_with(opts);
    match res {
        Ok(Started::Armed) => {
            s.owner.set(Some(id));
            say(s, "Armed. Waiting for the first note. Control R or Space stops.");
        }
        Ok(Started::Recording(_)) => {
            s.owner.set(Some(id));
            say(s, "Recording. Control R or Space stops.");
        }
        Err(e) => say(s, &format!("Error: {e:#}")),
    }
    refresh_state(s, true);
}

/// Останавливает запись и/или воспроизведение.
fn stop_all(s: &State) {
    let o = s.engine.borrow_mut().stop();
    let owner = s.owner.take();
    match (o.was_recording, o.saved) {
        (true, Some(p)) => {
            if let Some(id) = owner {
                set_new_take(s, id, p);
            }
            say(s, "Recording stopped. Space plays it, Control S saves it.");
        }
        (true, None) => say(s, "Recording stopped. Nothing was recorded."),
        (false, _) if o.was_playing => say(s, "Playback stopped."),
        _ => say(s, "Nothing is running."),
    }
    refresh_state(s, true);
}

fn do_record(s: &State) {
    if s.engine.borrow().status().phase == Phase::Idle {
        start_recording(s);
    } else {
        stop_all(s);
    }
}

fn play_from_start(s: &State) {
    let (file, id) = {
        let d = s.docs.borrow();
        let doc = &d[cur(s)];
        (doc.file.clone(), doc.id)
    };
    let Some(file) = file else {
        say(s, "Nothing to play. Record with Control R or open a file with Control O.");
        return;
    };
    if s.engine.borrow().settings().output.is_none() {
        say(s, "No output device. Choose one in the Devices menu, Output device.");
        return;
    }
    let res = s.engine.borrow_mut().play(&file);
    match res {
        Ok(()) => {
            s.owner.set(Some(id));
            say(s, "Playing from the beginning.");
        }
        Err(e) => say(s, &format!("Error: {e:#}")),
    }
    refresh_state(s, true);
}

fn do_play_pause(s: &State) {
    if s.engine.borrow().status().phase != Phase::Idle {
        stop_all(s);
        return;
    }
    let pb = s.engine.borrow().playback();
    if pb != Playback::Stopped && s.owner.get() == Some(cur_id(s)) {
        let now = s.engine.borrow().toggle_pause();
        say(s, if now == Playback::Paused { "Paused." } else { "Resumed." });
        refresh_state(s, true);
    } else {
        play_from_start(s);
    }
}

fn do_play_begin(s: &State) {
    if s.engine.borrow().status().phase != Phase::Idle {
        return;
    }
    play_from_start(s);
}

// ───────────── настройки ─────────────

fn save_err(s: &State, r: anyhow::Result<()>) {
    if let Err(e) = r {
        say(s, &format!("Error: cannot save settings: {e:#}"));
    }
}

fn opt_toggle(s: &State, id: Id) {
    let on = checked(s, id);
    let r = s.engine.borrow().update(|st| match id {
        ID_OPT_AUTO => st.auto_start = on,
        ID_OPT_PAUSE => st.pause_split = on,
        _ => st.keep_controllers = on,
    });
    save_err(s, r);
    let (what, state) = match id {
        ID_OPT_AUTO => ("Start on first note", on),
        ID_OPT_PAUSE => ("Stop after pause", on),
        _ => ("Keep controllers", on),
    };
    say(s, &format!("{what}: {}.", if state { "on" } else { "off" }));
}

fn opt_pause_len(s: &State) {
    let cur = s.engine.borrow().settings().pause_secs;
    let Some(v) = ask_text(s, "Pause length", "Pause length in seconds (recording stops after this much silence):", &cur.to_string())
    else {
        return;
    };
    match v.trim().replace(',', ".").trim_end_matches('s').trim().parse::<f64>() {
        Ok(sec) if sec > 0.0 => {
            let r = s.engine.borrow().update(|st| st.pause_secs = sec);
            save_err(s, r);
            say(s, &format!("Pause length: {sec} s."));
        }
        _ => say(s, "Enter a number of seconds greater than 0."),
    }
}

fn opt_timer(s: &State) {
    let cur = s.engine.borrow().settings().timer_secs.map(|t| t.to_string()).unwrap_or_default();
    let Some(v) = ask_text(
        s,
        "Time limit",
        "Time limit, for example 90, 2m or 1:30. Leave empty or 0 for no limit:",
        &cur,
    ) else {
        return;
    };
    let v = v.trim();
    let secs = if v.is_empty() { Some(0) } else { parse_secs(v) };
    match secs {
        Some(0) => {
            let r = s.engine.borrow().update(|st| st.timer_secs = None);
            save_err(s, r);
            say(s, "Time limit: off.");
        }
        Some(t) => {
            let r = s.engine.borrow().update(|st| st.timer_secs = Some(t));
            save_err(s, r);
            say(s, &format!("Time limit: {}.", fmt_secs(t)));
        }
        None => say(s, "Could not read the time. Examples: 90, 2m, 1:30."),
    }
}

fn opt_preroll(s: &State) {
    let cur = s.engine.borrow().settings().preroll_ms;
    let Some(v) = ask_text(
        s,
        "Pre-roll",
        "Pre-roll in milliseconds: how much controller activity before the first note is kept:",
        &cur.to_string(),
    ) else {
        return;
    };
    match v.trim().parse::<u64>() {
        Ok(ms) if ms <= 5000 => {
            let r = s.engine.borrow().update(|st| st.preroll_ms = ms);
            save_err(s, r);
            say(s, &format!("Pre-roll: {ms} ms."));
        }
        _ => say(s, "Enter a number from 0 to 5000."),
    }
}

// ───────────── устройства ─────────────

fn refresh_devices(s: &State) {
    let st = s.engine.borrow().settings();
    let ins = devices::input_names().unwrap_or_default();
    let outs = devices::output_names().unwrap_or_default();

    let mut input = st.input.clone();
    if input.is_none() && ins.len() == 1 {
        input = Some(ins[0].clone());
    }
    if let Some(name) = &input {
        if !ins.contains(name) {
            say(s, &format!("Saved input \"{name}\" is not available."));
        } else if s.engine.borrow().status().phase == Phase::Idle {
            let already = st.input.as_deref() == Some(name.as_str()) && s.engine.borrow().input_connected();
            if !already {
                match s.engine.borrow_mut().set_input(name) {
                    Ok(()) => say(s, &format!("Input: {name}")),
                    Err(e) => say(s, &format!("Error: {e:#}")),
                }
            }
        }
    }
    if st.output.is_none() && outs.len() == 1 {
        let _ = s.engine.borrow().set_output(&outs[0]);
        say(s, &format!("Output: {}", outs[0]));
    }
    update_options_menu(s);
}

fn pick_input(s: &State) {
    let names = devices::input_names().unwrap_or_default();
    if names.is_empty() {
        say(s, "No MIDI input devices found.");
        return;
    }
    let cur = s.engine.borrow().settings().input;
    if let Some(i) = ask_choice(s, "Input device", "Choose the MIDI input device:", &names, cur.as_deref()) {
        let res = s.engine.borrow_mut().set_input(&names[i]);
        match res {
            Ok(()) => say(s, &format!("Input: {}", names[i])),
            Err(e) => say(s, &format!("Error: {e:#}")),
        }
        update_options_menu(s);
    }
}

fn pick_output(s: &State) {
    let names = devices::output_names().unwrap_or_default();
    if names.is_empty() {
        say(s, "No MIDI output devices found.");
        return;
    }
    let cur = s.engine.borrow().settings().output;
    if let Some(i) = ask_choice(s, "Output device", "Choose the MIDI output device:", &names, cur.as_deref()) {
        let res = s.engine.borrow().set_output(&names[i]);
        match res {
            Ok(()) => say(s, &format!("Output: {}", names[i])),
            Err(e) => say(s, &format!("Error: {e:#}")),
        }
        update_options_menu(s);
    }
}

// ───────────── справка ─────────────

fn show_keys(s: &State) {
    let dlg = MessageDialog::builder(
        &s.frame,
        "Control N: new tab\nControl Tab: next tab\nControl Shift Tab: previous tab\nControl W: close tab\nControl O: open\nControl S: save\nControl Shift S: save as\n\nControl R: record or stop recording\nSpace: play, pause, resume; stops recording\nControl Space: play from the beginning\nEscape: stop\n\nF5: refresh devices",
        "Keyboard shortcuts",
    )
    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
    .build();
    dlg.show_modal();
}

fn show_about(s: &State) {
    let dlg = MessageDialog::builder(&s.frame, "midifon: MIDI recorder and player.\nTerminal version: mfcli.", "About midifon")
        .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
        .build();
    dlg.show_modal();
}

// ───────────── состояние (по таймеру) ─────────────

fn refresh_state(s: &State, force: bool) {
    // уведомления из рабочих потоков
    let pending: Vec<Notice> = s.notices.lock().unwrap().drain(..).collect();
    for n in &pending {
        match n {
            Notice::PauseStopped { saved } | Notice::TimerElapsed { saved: Some(saved) } => {
                if let Some(id) = s.owner.take() {
                    set_new_take(s, id, saved.clone());
                }
            }
            Notice::TimerElapsed { saved: None } => {
                s.owner.set(None);
            }
            _ => {}
        }
        say(s, &describe(n));
    }

    let sig = {
        let e = s.engine.borrow();
        let d = s.docs.borrow();
        let docs: Vec<String> = d.iter().map(|x| format!("{}{}{}", x.label(), x.file.is_some(), x.id)).collect();
        format!("{:?}|{:?}|{}|{}|{:?}", e.status().phase, e.playback(), cur(s), docs.join(","), s.owner.get())
    };
    if force || *s.last_sig.borrow() != sig || !pending.is_empty() {
        *s.last_sig.borrow_mut() = sig;
        update_state_ui(s);
    }
}

// ───────────── построение окна ─────────────

fn main() {
    let _ = wxdragon::main(|_| build());
}

fn build() {
    let notices: Arc<Mutex<VecDeque<Notice>>> = Arc::new(Mutex::new(VecDeque::new()));
    let n2 = notices.clone();
    let notify: Notifier = Arc::new(move |n| n2.lock().unwrap().push_back(n));
    let cfg_path = Settings::default_path();
    let (settings, cfg_err) = match &cfg_path {
        Some(p) => match Settings::load(p) {
            Ok(s) => (s, None),
            Err(e) => (Settings::default(), Some(format!("{e:#}"))),
        },
        None => (Settings::default(), None),
    };
    let engine = Engine::new(settings, cfg_path, notify);

    let takes_dir = std::env::temp_dir().join("midifon-takes");
    let _ = std::fs::remove_dir_all(&takes_dir);

    let file = Menu::builder()
        .append_item(ID_NEW, "&New tab\tCtrl+N", "Open a new empty tab")
        .append_item(ID_OPEN, "&Open...\tCtrl+O", "Open a MIDI file")
        .append_item(ID_SAVE, "&Save\tCtrl+S", "Save the recording")
        .append_item(ID_SAVEAS, "Save &As...\tCtrl+Shift+S", "Save the recording under a new name")
        .append_item(ID_CLOSE, "&Close tab\tCtrl+W", "Close the current tab")
        .append_item(ID_NEXT_TAB, "Nex&t tab\tCtrl+Tab", "Switch to the next tab")
        .append_item(ID_PREV_TAB, "&Previous tab\tCtrl+Shift+Tab", "Switch to the previous tab")
        .append_separator()
        .append_item(ID_EXIT, "E&xit\tAlt+F4", "Quit")
        .build();
    let rec = Menu::builder()
        .append_item(ID_REC, "&Record\tCtrl+R", "Start or stop recording")
        .append_item(ID_PLAY, "&Play\tSpace", "Play, pause or resume")
        .append_item(ID_PLAY_BEGIN, "Play from the &beginning\tCtrl+Space", "Play from the beginning")
        .append_item(ID_STOP, "&Stop\tEsc", "Stop recording or playback")
        .build();
    let opts = Menu::builder()
        .append_check_item(ID_OPT_AUTO, "Start on &first note", "Wait for the first note before recording")
        .append_check_item(ID_OPT_PAUSE, "&Stop after pause", "Stop recording after a long silence")
        .append_item(ID_OPT_PAUSE_LEN, "Pause &length...", "Set the pause length")
        .append_item(ID_OPT_TIMER, "&Time limit...", "Set a recording time limit")
        .append_check_item(ID_OPT_KEEP, "&Keep controllers between recordings", "Write last controller values at the start of each recording")
        .append_item(ID_OPT_PREROLL, "&Pre-roll...", "Set the pre-roll")
        .build();
    let dev = Menu::builder()
        .append_item(ID_DEV_IN, "&Input device...", "Choose the MIDI input")
        .append_item(ID_DEV_OUT, "&Output device...", "Choose the MIDI output")
        .append_item(ID_DEV_REFRESH, "&Refresh devices\tF5", "Rescan MIDI devices")
        .build();
    let help = Menu::builder()
        .append_item(ID_KEYS, "&Keyboard shortcuts", "List of keyboard shortcuts")
        .append_item(ID_ABOUT, "&About", "About midifon")
        .build();
    let bar = MenuBar::builder()
        .append(file, "&File")
        .append(rec, "&Record and play")
        .append(opts, "&Options")
        .append(dev, "&Devices")
        .append(help, "&Help")
        .build();

    let frame = Frame::builder().with_title("untitled - midifon").with_size(Size::new(560, 360)).build();
    frame.set_menu_bar(bar);
    let status = frame.create_status_bar(1, 0, ID_ANY as i32, "status");
    // Рабочее поле: только вкладки, содержимое вкладок пустое
    let notebook = Notebook::builder(&frame).build();

    let s: S = Rc::new(State {
        frame,
        status,
        notebook,
        engine: RefCell::new(engine),
        notices,
        docs: RefCell::new(Vec::new()),
        next_id: Cell::new(1),
        owner: Cell::new(None),
        takes_dir,
        last_sig: RefCell::new(String::new()),
    });
    add_tab(&s);

    if let Some(e) = cfg_err {
        say(&s, &format!("Config error: {e}. Using defaults."));
    }
    refresh_devices(&s);
    say(&s, "Ready. Control R records, Space plays.");
    refresh_state(&s, true);

    let c = s.clone();
    s.frame.on_menu(move |ev| match ev.get_id() {
        ID_NEW => do_new(&c),
        ID_OPEN => do_open(&c),
        ID_SAVE => {
            let i = cur(&c);
            save(&c, i);
        }
        ID_SAVEAS => {
            let i = cur(&c);
            save_as(&c, i);
        }
        ID_CLOSE => do_close_tab(&c),
        ID_NEXT_TAB | ID_PREV_TAB => {
            c.notebook.advance_selection(ev.get_id() == ID_NEXT_TAB);
            refresh_state(&c, true);
        }
        ID_REC => do_record(&c),
        ID_PLAY => do_play_pause(&c),
        ID_PLAY_BEGIN => do_play_begin(&c),
        ID_STOP => stop_all(&c),
        ID_OPT_AUTO | ID_OPT_PAUSE | ID_OPT_KEEP => {
            opt_toggle(&c, ev.get_id());
            refresh_state(&c, true);
        }
        ID_OPT_PAUSE_LEN => {
            opt_pause_len(&c);
            update_options_menu(&c);
        }
        ID_OPT_TIMER => {
            opt_timer(&c);
            update_options_menu(&c);
        }
        ID_OPT_PREROLL => {
            opt_preroll(&c);
            update_options_menu(&c);
        }
        ID_DEV_IN => pick_input(&c),
        ID_DEV_OUT => pick_output(&c),
        ID_DEV_REFRESH => {
            refresh_devices(&c);
            say(&c, "Devices refreshed.");
        }
        ID_KEYS => show_keys(&c),
        ID_ABOUT => show_about(&c),
        ID_EXIT => c.frame.close(false),
        _ => {}
    });

    // смена вкладки: обновить заголовок и меню
    let c = s.clone();
    s.notebook.on_page_changed(move |_| refresh_state(&c, true));

    // закрытие: остановить запись и спросить про несохранённое в каждой вкладке;
    // без skip окно не закроется
    let c = s.clone();
    s.frame.on_close(move |ev| {
        if c.engine.borrow().status().phase != Phase::Idle {
            stop_all(&c);
        }
        let n = c.docs.borrow().len();
        for i in 0..n {
            if !confirm_save(&c, i) {
                return;
            }
        }
        c.engine.borrow_mut().stop();
        ev.skip(true);
    });

    // Space на вкладках (на GTK элемент забирает пробел раньше акселератора меню)
    let c = s.clone();
    s.notebook.on_key_down(move |ev| {
        if let WindowEventData::Keyboard(k) = ev {
            if k.get_key_code() == Some(32) {
                if k.control_down() {
                    do_play_begin(&c);
                } else {
                    do_play_pause(&c);
                }
                k.event.skip(false);
            } else {
                k.event.skip(true);
            }
        }
    });

    // опрос состояния и уведомлений
    let timer = Timer::new(&s.frame);
    let c = s.clone();
    timer.on_tick(move |_| refresh_state(&c, false));
    timer.start(150, false);
    std::mem::forget(timer);

    s.frame.centre();
    s.frame.show(true);
    s.notebook.set_focus();
}
