//! midifon: GUI на wxdragon поверх midifon-core.
//! Доступность: нативные контролы, метка (StaticText) создаётся прямо перед своим полем,
//! у всех кнопок и флажков есть мнемоники, весь вывод событий дублируется в журнал (read-only текстовое поле).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use midifon_core::devices;
use midifon_core::engine::{Engine, Notice, Notifier, Phase, Started};
use midifon_core::settings::Settings;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use wxdragon::prelude::*;

const ID_START: Id = ID_HIGHEST + 1;
const ID_STOP: Id = ID_HIGHEST + 2;
const ID_PLAY_LAST: Id = ID_HIGHEST + 3;
const ID_OPEN: Id = ID_HIGHEST + 4;
const ID_REFRESH: Id = ID_HIGHEST + 5;
const ID_FOLDER: Id = ID_HIGHEST + 6;
const ID_OPT_AUTO: Id = ID_HIGHEST + 7;
const ID_OPT_PAUSE: Id = ID_HIGHEST + 8;
const ID_OPT_KEEP: Id = ID_HIGHEST + 9;

struct Ui {
    frame: Frame,
    start: Button,
    stop: Button,
    play: Button,
    status: StaticText,
    input: Choice,
    output: Choice,
    dir: TextCtrl,
    name: TextCtrl,
    auto: CheckBox,
    pause: CheckBox,
    pause_secs: SpinCtrlDouble,
    timer: CheckBox,
    timer_secs: SpinCtrl,
    keep: CheckBox,
    preroll: SpinCtrl,
    log: TextCtrl,
}

struct State {
    ui: Ui,
    engine: RefCell<Engine>,
    notices: Arc<Mutex<VecDeque<Notice>>>,
    inputs: RefCell<Vec<String>>,
    outputs: RefCell<Vec<String>>,
    last_status: RefCell<String>,
    last_enabled: RefCell<(bool, bool, bool)>,
}

type S = Rc<State>;

fn fname(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn log(s: &State, msg: &str) {
    s.ui.log.append_text(&format!("{msg}\n"));
}

fn describe(n: &Notice) -> String {
    match n {
        Notice::TakeStarted { path } => format!("Note detected. Recording to {}.", fname(path)),
        Notice::PauseSplit { saved } => format!("Detected pause, saved {}. Waiting for the next notes.", fname(saved)),
        Notice::TimerElapsed { saved: Some(p) } => format!("Timer elapsed, saved {}. Recording stopped.", fname(p)),
        Notice::TimerElapsed { saved: None } => "Timer elapsed. Recording stopped, nothing was recorded.".into(),
        Notice::PlaybackFinished => "Playback finished.".into(),
        Notice::Error(e) => format!("Error: {e}"),
    }
}

// ───────────── настройки ↔ виджеты ─────────────

/// Виджеты → настройки (вызывается при любом изменении).
fn apply_ui(s: &State) {
    let u = &s.ui;
    let dir = u.dir.get_value();
    let name = u.name.get_value();
    let pause_secs = u.pause_secs.get_value().max(0.1);
    let timer_secs = u.timer_secs.value().max(1) as u64;
    let preroll = u.preroll.value().max(0) as u64;
    let (auto, pause, timer, keep) = (u.auto.is_checked(), u.pause.is_checked(), u.timer.is_checked(), u.keep.is_checked());
    let res = s.engine.borrow().update(|st| {
        if !dir.trim().is_empty() {
            st.dir = PathBuf::from(dir.trim());
        }
        if !name.trim().is_empty() {
            st.name = name.trim().to_string();
        }
        st.auto_start = auto;
        st.pause_split = pause;
        st.pause_secs = pause_secs;
        st.timer_secs = if timer { Some(timer_secs) } else { None };
        st.keep_controllers = keep;
        st.preroll_ms = preroll;
    });
    if let Err(e) = res {
        log(s, &format!("Error: cannot save settings: {e:#}"));
    }
}

/// Включает/выключает поля секунд и синхронизирует пункты меню с флажками.
fn sync_enables(s: &State) {
    let u = &s.ui;
    u.pause_secs.enable(u.pause.is_checked());
    u.timer_secs.enable(u.timer.is_checked());
    if let Some(bar) = u.frame.get_menu_bar() {
        bar.check_item(ID_OPT_AUTO, u.auto.is_checked());
        bar.check_item(ID_OPT_PAUSE, u.pause.is_checked());
        bar.check_item(ID_OPT_KEEP, u.keep.is_checked());
    }
}

/// Настройки → виджеты (при запуске).
fn load_ui(s: &State) {
    let st = s.engine.borrow().settings();
    let u = &s.ui;
    u.dir.set_value(&st.dir.to_string_lossy());
    u.name.set_value(&st.name);
    u.auto.set_value(st.auto_start);
    u.pause.set_value(st.pause_split);
    u.pause_secs.set_value(st.pause_secs);
    u.timer.set_value(st.timer_secs.is_some());
    u.timer_secs.set_value(st.timer_secs.unwrap_or(60) as i32);
    u.keep.set_value(st.keep_controllers);
    u.preroll.set_value(st.preroll_ms as i32);
    sync_enables(s);
}

fn changed(s: &State) {
    apply_ui(s);
    sync_enables(s);
}

// ───────────── устройства ─────────────

fn fill_choice(choice: &Choice, names: &[String], selected: Option<&str>) {
    choice.clear();
    if names.is_empty() {
        choice.append("No devices found");
        choice.set_selection(0);
        return;
    }
    for n in names {
        choice.append(n);
    }
    if let Some(i) = selected.and_then(|sel| names.iter().position(|n| n == sel)) {
        choice.set_selection(i as u32);
    }
}

fn refresh_devices(s: &State) {
    let st = s.engine.borrow().settings();
    let ins = devices::input_names().unwrap_or_default();
    let outs = devices::output_names().unwrap_or_default();

    // как в CLI: единственное устройство выбирается само
    let mut input_sel = st.input.clone();
    if input_sel.is_none() && ins.len() == 1 {
        input_sel = Some(ins[0].clone());
    }
    let mut output_sel = st.output.clone();
    if output_sel.is_none() && outs.len() == 1 {
        output_sel = Some(outs[0].clone());
        let _ = s.engine.borrow().set_output(&outs[0]);
    }
    fill_choice(&s.ui.input, &ins, input_sel.as_deref());
    fill_choice(&s.ui.output, &outs, output_sel.as_deref());

    if let Some(name) = &input_sel {
        let idle = s.engine.borrow().status().phase == Phase::Idle;
        if idle && ins.contains(name) {
            if let Err(e) = s.engine.borrow_mut().set_input(name) {
                log(s, &format!("Error: {e:#}"));
            }
        } else if !ins.contains(name) {
            log(s, &format!("Saved input \"{name}\" is not available."));
        }
    }
    *s.inputs.borrow_mut() = ins;
    *s.outputs.borrow_mut() = outs;
}

fn input_selected(s: &State) {
    let Some(i) = s.ui.input.get_selection() else { return };
    let Some(name) = s.inputs.borrow().get(i as usize).cloned() else { return };
    let res = s.engine.borrow_mut().set_input(&name);
    match res {
        Ok(()) => log(s, &format!("Input: {name}")),
        Err(e) => log(s, &format!("Error: {e:#}")),
    }
}

fn output_selected(s: &State) {
    let Some(i) = s.ui.output.get_selection() else { return };
    let Some(name) = s.outputs.borrow().get(i as usize).cloned() else { return };
    let res = s.engine.borrow().set_output(&name);
    match res {
        Ok(()) => log(s, &format!("Output: {name}")),
        Err(e) => log(s, &format!("Error: {e:#}")),
    }
}

// ───────────── действия ─────────────

fn do_start(s: &State) {
    apply_ui(s);
    if !s.engine.borrow().input_connected() {
        log(s, "No input device. Choose one in the Input device list.");
        return;
    }
    let res = s.engine.borrow().start(None);
    match res {
        Ok(Started::Armed) => log(s, "Armed. Waiting for the first note."),
        Ok(Started::Recording(p)) => log(s, &format!("Recording to {}.", fname(&p))),
        Err(e) => log(s, &format!("Error: {e:#}")),
    }
    refresh_state(s);
    if s.engine.borrow().status().phase != Phase::Idle {
        s.ui.stop.set_focus();
    }
}

fn do_stop(s: &State) {
    let o = s.engine.borrow_mut().stop();
    match (o.was_recording, o.saved) {
        (true, Some(p)) => log(s, &format!("Stopped, saved {}.", fname(&p))),
        (true, None) => log(s, "Stopped. Nothing was recorded."),
        (false, _) if o.was_playing => log(s, "Playback stopped."),
        _ => log(s, "Nothing is running."),
    }
    refresh_state(s);
    s.ui.start.set_focus();
}

fn do_play(s: &State, path: &Path) {
    let res = s.engine.borrow_mut().play(path);
    match res {
        Ok(()) => log(s, &format!("Playing {}.", fname(path))),
        Err(e) => log(s, &format!("Error: {e:#}")),
    }
    refresh_state(s);
}

fn do_play_last(s: &State) {
    let last = s.engine.borrow().status().last_saved;
    match last {
        Some(p) => do_play(s, &p),
        None => log(s, "No recording in this session. Use File, Open MIDI file to play."),
    }
}

fn do_open_file(s: &State) {
    let dir = s.engine.borrow().settings().dir.to_string_lossy().into_owned();
    let dlg = FileDialog::builder(&s.ui.frame)
        .with_message("Choose a MIDI file to play")
        .with_default_dir(&dir)
        .with_wildcard("MIDI files (*.mid;*.midi)|*.mid;*.midi")
        .build();
    if dlg.show_modal() == ID_OK {
        if let Some(p) = dlg.get_path() {
            do_play(s, Path::new(&p));
        }
    }
}

fn do_browse(s: &State) {
    let cur = s.ui.dir.get_value();
    let dlg = DirDialog::builder(&s.ui.frame, "Choose the folder for recordings", &cur).build();
    if dlg.show_modal() == ID_OK {
        if let Some(p) = dlg.get_path() {
            s.ui.dir.set_value(&p);
            changed(s);
        }
    }
}

fn do_open_folder(s: &State) {
    let dir = s.engine.borrow().settings().dir;
    let _ = std::fs::create_dir_all(&dir);
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(&dir).spawn() {
        log(s, &format!("Error: cannot open folder: {e}"));
    }
}

fn do_about(s: &State) {
    let dlg = MessageDialog::builder(
        &s.ui.frame,
        "midifon: MIDI recorder and player.\nTerminal version: mfcli.",
        "About midifon",
    )
    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
    .build();
    dlg.show_modal();
}

// ───────────── состояние (по таймеру) ─────────────

fn refresh_state(s: &State) {
    // уведомления из рабочих потоков → журнал
    let pending: Vec<Notice> = s.notices.lock().unwrap().drain(..).collect();
    for n in &pending {
        log(s, &describe(n));
    }

    let (status, playing) = {
        let e = s.engine.borrow();
        (e.status(), e.is_playing())
    };
    let text = match status.phase {
        Phase::Idle if playing => "Playing".to_string(),
        Phase::Idle => "Idle".to_string(),
        Phase::Armed => "Armed: waiting for the first note".to_string(),
        Phase::Recording => format!("Recording to {}", status.path.as_deref().map(fname).unwrap_or_default()),
    };
    if *s.last_status.borrow() != text {
        s.ui.status.set_label(&text);
        *s.last_status.borrow_mut() = text;
    }

    let idle = status.phase == Phase::Idle;
    let enabled = (idle, !idle || playing, idle && !playing);
    if *s.last_enabled.borrow() != enabled {
        s.ui.start.enable(enabled.0);
        s.ui.stop.enable(enabled.1);
        s.ui.play.enable(enabled.2);
        s.ui.input.enable(idle);
        *s.last_enabled.borrow_mut() = enabled;
    }
}

// ───────────── построение окна ─────────────

fn label(parent: &Panel, text: &str) -> StaticText {
    StaticText::builder(parent).with_label(text).build()
}

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

    // меню
    let file = Menu::builder()
        .append_item(ID_OPEN, "&Open MIDI file to play...\tCtrl+O", "Play a MIDI file")
        .append_item(ID_PLAY_LAST, "Play &last recording\tCtrl+L", "Play the last saved recording")
        .append_item(ID_FOLDER, "Open recordings &folder", "Show the recordings folder")
        .append_separator()
        .append_item(ID_EXIT, "E&xit", "Quit")
        .build();
    let rec = Menu::builder()
        .append_item(ID_START, "&Start recording\tCtrl+R", "Start recording")
        .append_item(ID_STOP, "S&top\tCtrl+T", "Stop recording or playback")
        .append_separator()
        .append_check_item(ID_OPT_AUTO, "Start on &first note", "Wait for the first note before recording")
        .append_check_item(ID_OPT_PAUSE, "Split files on &pause", "Save the file and wait for new notes after a pause")
        .append_check_item(ID_OPT_KEEP, "&Keep controllers between recordings", "Write last controller values at the start of each file")
        .build();
    let dev = Menu::builder().append_item(ID_REFRESH, "&Refresh devices\tF5", "Rescan MIDI devices").build();
    let help = Menu::builder().append_item(ID_ABOUT, "&About", "About midifon").build();
    let bar = MenuBar::builder().append(file, "&File").append(rec, "&Record").append(dev, "&Devices").append(help, "&Help").build();

    let frame = Frame::builder().with_title("midifon").with_size(Size::new(660, 720)).build();
    frame.set_menu_bar(bar);
    let panel = Panel::builder(&frame).build();

    // кнопки и статус
    let start = Button::builder(&panel).with_label("&Start").build();
    let stop = Button::builder(&panel).with_label("S&top").build();
    let play = Button::builder(&panel).with_label("&Play last").build();
    let status_label = label(&panel, "Status:");
    let status = label(&panel, "Idle");

    // устройства и пути
    let grid = FlexGridSizer::builder(0, 2).with_vgap(6).with_hgap(8).build();
    let l_in = label(&panel, "&Input device:");
    let input = Choice::builder(&panel).build();
    let l_out = label(&panel, "&Output device:");
    let output = Choice::builder(&panel).build();
    let l_dir = label(&panel, "Recordings &folder:");
    let dir = TextCtrl::builder(&panel).build();
    let browse = Button::builder(&panel).with_label("&Browse...").build();
    let l_name = label(&panel, "File &name:");
    let name = TextCtrl::builder(&panel).build();
    let refresh = Button::builder(&panel).with_label("&Refresh devices").build();

    // параметры записи
    let auto = CheckBox::builder(&panel).with_label("Start on first &note").build();
    let keep = CheckBox::builder(&panel).with_label("&Keep controllers between recordings").build();
    let pause = CheckBox::builder(&panel).with_label("Split files on &pause").build();
    let l_pause = label(&panel, "Pause &length, seconds:");
    let pause_secs = SpinCtrlDouble::builder(&panel).with_range(0.5, 3600.0).build();
    pause_secs.set_increment(0.5);
    pause_secs.set_digits(1);
    let timer = CheckBox::builder(&panel).with_label("Stop after a &time limit").build();
    let l_timer = label(&panel, "Time limit, &seconds:");
    let timer_secs = SpinCtrl::builder(&panel).with_range(1, 86400).build();
    let l_pre = label(&panel, "Pre-roll before first note, &ms:");
    let preroll = SpinCtrl::builder(&panel).with_range(0, 5000).build();

    let l_log = label(&panel, "&Log:");
    let log_box = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .with_size(Size::new(-1, 140))
        .build();

    // раскладка
    let root = BoxSizer::builder(Orientation::Vertical).build();
    let row = BoxSizer::builder(Orientation::Horizontal).build();
    row.add(&start, 0, SizerFlag::All, 6);
    row.add(&stop, 0, SizerFlag::All, 6);
    row.add(&play, 0, SizerFlag::All, 6);
    row.add(&status_label, 0, SizerFlag::All | SizerFlag::AlignCenterVertical, 6);
    row.add(&status, 1, SizerFlag::All | SizerFlag::AlignCenterVertical, 6);
    root.add_sizer(&row, 0, SizerFlag::Expand, 0);

    grid.add(&l_in, 0, SizerFlag::AlignCenterVertical, 0);
    grid.add(&input, 1, SizerFlag::Expand, 0);
    grid.add(&l_out, 0, SizerFlag::AlignCenterVertical, 0);
    grid.add(&output, 1, SizerFlag::Expand, 0);
    grid.add(&l_dir, 0, SizerFlag::AlignCenterVertical, 0);
    let dir_row = BoxSizer::builder(Orientation::Horizontal).build();
    dir_row.add(&dir, 1, SizerFlag::Expand, 0);
    dir_row.add(&browse, 0, SizerFlag::Left, 6);
    grid.add_sizer(&dir_row, 1, SizerFlag::Expand, 0);
    grid.add(&l_name, 0, SizerFlag::AlignCenterVertical, 0);
    grid.add(&name, 1, SizerFlag::Expand, 0);
    grid.add_growable_col(1, 1);
    root.add_sizer(&grid, 0, SizerFlag::All | SizerFlag::Expand, 8);
    root.add(&refresh, 0, SizerFlag::Left | SizerFlag::Bottom, 8);

    root.add(&auto, 0, SizerFlag::Left | SizerFlag::Top, 8);
    root.add(&keep, 0, SizerFlag::Left | SizerFlag::Top, 8);
    let pause_row = BoxSizer::builder(Orientation::Horizontal).build();
    pause_row.add(&pause, 0, SizerFlag::AlignCenterVertical, 0);
    pause_row.add(&l_pause, 0, SizerFlag::Left | SizerFlag::AlignCenterVertical, 12);
    pause_row.add(&pause_secs, 0, SizerFlag::Left, 6);
    root.add_sizer(&pause_row, 0, SizerFlag::All, 8);
    let timer_row = BoxSizer::builder(Orientation::Horizontal).build();
    timer_row.add(&timer, 0, SizerFlag::AlignCenterVertical, 0);
    timer_row.add(&l_timer, 0, SizerFlag::Left | SizerFlag::AlignCenterVertical, 12);
    timer_row.add(&timer_secs, 0, SizerFlag::Left, 6);
    root.add_sizer(&timer_row, 0, SizerFlag::All, 8);
    let pre_row = BoxSizer::builder(Orientation::Horizontal).build();
    pre_row.add(&l_pre, 0, SizerFlag::AlignCenterVertical, 0);
    pre_row.add(&preroll, 0, SizerFlag::Left, 6);
    root.add_sizer(&pre_row, 0, SizerFlag::All, 8);

    root.add(&l_log, 0, SizerFlag::Left | SizerFlag::Top, 8);
    root.add(&log_box, 1, SizerFlag::All | SizerFlag::Expand, 8);
    panel.set_sizer(root, true);

    let ui = Ui {
        frame,
        start,
        stop,
        play,
        status,
        input,
        output,
        dir,
        name,
        auto,
        pause,
        pause_secs,
        timer,
        timer_secs,
        keep,
        preroll,
        log: log_box,
    };
    let s: S = Rc::new(State {
        ui,
        engine: RefCell::new(engine),
        notices,
        inputs: RefCell::new(Vec::new()),
        outputs: RefCell::new(Vec::new()),
        last_status: RefCell::new(String::new()),
        last_enabled: RefCell::new((false, false, false)),
    });

    load_ui(&s);
    if let Some(e) = cfg_err {
        log(&s, &format!("Config error: {e}. Using defaults."));
    }
    refresh_devices(&s);
    log(&s, "Ready.");
    refresh_state(&s);

    // события
    {
        let c = s.clone();
        s.ui.start.on_click(move |_| do_start(&c));
        let c = s.clone();
        s.ui.stop.on_click(move |_| do_stop(&c));
        let c = s.clone();
        s.ui.play.on_click(move |_| do_play_last(&c));
        let c = s.clone();
        browse.on_click(move |_| do_browse(&c));
        let c = s.clone();
        refresh.on_click(move |_| refresh_devices(&c));

        let c = s.clone();
        s.ui.input.on_selection_changed(move |_| input_selected(&c));
        let c = s.clone();
        s.ui.output.on_selection_changed(move |_| output_selected(&c));

        let c = s.clone();
        s.ui.dir.on_text_changed(move |_| apply_ui(&c));
        let c = s.clone();
        s.ui.name.on_text_changed(move |_| apply_ui(&c));
        let c = s.clone();
        s.ui.auto.on_toggled(move |_| changed(&c));
        let c = s.clone();
        s.ui.keep.on_toggled(move |_| changed(&c));
        let c = s.clone();
        s.ui.pause.on_toggled(move |_| changed(&c));
        let c = s.clone();
        s.ui.timer.on_toggled(move |_| changed(&c));
        let c = s.clone();
        s.ui.pause_secs.on_value_changed(move |_| changed(&c));
        let c = s.clone();
        s.ui.timer_secs.on_value_changed(move |_| changed(&c));
        let c = s.clone();
        s.ui.preroll.on_value_changed(move |_| changed(&c));

        // меню
        let c = s.clone();
        s.ui.frame.on_menu(move |ev| {
            let checked = |id: Id| c.ui.frame.get_menu_bar().map(|b| b.is_item_checked(id)).unwrap_or(false);
            match ev.get_id() {
                ID_START => do_start(&c),
                ID_STOP => do_stop(&c),
                ID_PLAY_LAST => do_play_last(&c),
                ID_OPEN => do_open_file(&c),
                ID_FOLDER => do_open_folder(&c),
                ID_REFRESH => refresh_devices(&c),
                ID_OPT_AUTO => {
                    c.ui.auto.set_value(checked(ID_OPT_AUTO));
                    changed(&c);
                }
                ID_OPT_PAUSE => {
                    c.ui.pause.set_value(checked(ID_OPT_PAUSE));
                    changed(&c);
                }
                ID_OPT_KEEP => {
                    c.ui.keep.set_value(checked(ID_OPT_KEEP));
                    changed(&c);
                }
                ID_ABOUT => do_about(&c),
                ID_EXIT => c.ui.frame.close(false),
                _ => {}
            }
        });

        // закрытие: сохранить идущую запись
        let c = s.clone();
        s.ui.frame.on_close(move |ev| {
            apply_ui(&c);
            let o = c.engine.borrow_mut().stop();
            if let Some(p) = o.saved {
                eprintln!("saved {}", p.display());
            }
            ev.skip(true);
        });
    }

    // опрос состояния и уведомлений
    let timer = Timer::new(&s.ui.frame);
    let c = s.clone();
    timer.on_tick(move |_| refresh_state(&c));
    timer.start(150, false);
    std::mem::forget(timer);

    s.ui.frame.centre();
    s.ui.frame.show(true);
    s.ui.start.set_focus();
}
