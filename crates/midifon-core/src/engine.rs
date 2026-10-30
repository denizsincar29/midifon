use crate::player::Player;
use crate::settings::Settings;
use crate::smf::{self, TimedEvent};
use anyhow::{anyhow, bail, Result};
use midir::{Ignore, MidiInput, MidiInputConnection};
use midly::live::LiveEvent;
use midly::num::{u4, u7};
use midly::{MidiMessage, PitchBend};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Асинхронные уведомления (их печатает CLI / показывает GUI).
#[derive(Debug, Clone)]
pub enum Notice {
    /// Автозапись: пришла первая нота, файл начат.
    TakeStarted { path: PathBuf },
    /// Обнаружена пауза: файл сохранён, ждём следующие ноты.
    PauseSplit { saved: PathBuf },
    /// Сработал таймер: запись остановлена.
    TimerElapsed { saved: Option<PathBuf> },
    PlaybackFinished,
    Error(String),
}

pub type Notifier = Arc<dyn Fn(Notice) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// Ждём первую ноту.
    Armed,
    Recording,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub phase: Phase,
    /// Файл текущей записи.
    pub path: Option<PathBuf>,
    pub last_saved: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Started {
    Armed,
    Recording(PathBuf),
}

#[derive(Debug, Clone)]
pub struct StopOutcome {
    pub was_recording: bool,
    pub saved: Option<PathBuf>,
    pub was_playing: bool,
}

enum Msg {
    Midi { ts_us: u64, at: Instant, bytes: Vec<u8> },
    Start { name: Option<String>, reply: Sender<Result<Started, String>> },
    Stop { reply: Sender<(bool, Option<PathBuf>)> },
    Shutdown,
}

// ───────────────────────── контроллеры ─────────────────────────

#[derive(Clone)]
struct Chan {
    program: Option<u7>,
    bend: Option<PitchBend>,
    cc: [Option<u7>; 128],
}

impl Default for Chan {
    fn default() -> Self {
        Chan { program: None, bend: None, cc: [None; 128] }
    }
}

struct Controllers {
    ch: Vec<Chan>,
}

impl Controllers {
    fn new() -> Self {
        Controllers { ch: vec![Chan::default(); 16] }
    }

    fn update(&mut self, ch: u4, msg: &MidiMessage) {
        let c = &mut self.ch[ch.as_int() as usize];
        match msg {
            // 120..127 — канальные режимы (all notes off и т.п.), их не восстанавливаем
            MidiMessage::Controller { controller, value } if controller.as_int() < 120 => {
                c.cc[controller.as_int() as usize] = Some(*value)
            }
            MidiMessage::ProgramChange { program } => c.program = Some(*program),
            MidiMessage::PitchBend { bend } => c.bend = Some(*bend),
            _ => {}
        }
    }

    /// Порядок на канале: CC (включая bank select) → program change → pitch bend.
    fn snapshot(&self) -> Vec<(u4, MidiMessage)> {
        let mut out = Vec::new();
        for (i, c) in self.ch.iter().enumerate() {
            let ch = u4::from(i as u8);
            for (n, v) in c.cc.iter().enumerate() {
                if let Some(v) = v {
                    out.push((ch, MidiMessage::Controller { controller: u7::from(n as u8), value: *v }));
                }
            }
            if let Some(p) = c.program {
                out.push((ch, MidiMessage::ProgramChange { program: p }));
            }
            if let Some(b) = c.bend {
                out.push((ch, MidiMessage::PitchBend { bend: b }));
            }
        }
        out
    }
}

// ───────────────────────── дубль (один файл) ─────────────────────────

struct Take {
    path: PathBuf,
    n: u32,
    start: Instant,
    ts0: Option<u64>,
    lead_us: u64,
    events: Vec<TimedEvent>,
    held: HashSet<(u8, u8)>,
    last_note_at: Option<Instant>,
    last_note_rel: u64,
    init: Vec<(u4, MidiMessage)>,
    bpm: f64,
}

impl Take {
    fn last_rel(&self) -> u64 {
        self.events.last().map(|e| e.rel_us).unwrap_or(0)
    }

    fn push(&mut self, ts: u64, at: Instant, ch: u4, msg: MidiMessage) {
        let rel = match self.ts0 {
            None => {
                self.ts0 = Some(ts);
                self.lead_us = at.saturating_duration_since(self.start).as_micros() as u64;
                self.lead_us
            }
            Some(t0) => self.lead_us + ts.saturating_sub(t0),
        }
        .max(self.last_rel());

        match msg {
            MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                self.held.insert((ch.as_int(), key.as_int()));
                self.last_note_at = Some(at);
                self.last_note_rel = rel;
            }
            MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
                self.held.remove(&(ch.as_int(), key.as_int()));
                self.last_note_at = Some(at);
                self.last_note_rel = rel;
            }
            _ => {}
        }
        self.events.push(TimedEvent { rel_us: rel, channel: ch, message: msg });
    }

    fn write(mut self, end_rel: u64) -> Result<(PathBuf, u32)> {
        // зависшие ноты закрываем в конце
        let end = end_rel.max(self.last_rel());
        let mut held: Vec<_> = self.held.iter().copied().collect();
        held.sort();
        for (ch, key) in held {
            self.events.push(TimedEvent {
                rel_us: end,
                channel: u4::from(ch),
                message: MidiMessage::NoteOff { key: u7::from(key), vel: u7::from(0) },
            });
        }
        smf::write(&self.path, self.bpm, &self.init, &self.events, end)?;
        Ok((self.path, self.n))
    }
}

struct Session {
    name: String,
    dir: PathBuf,
    next: u32,
    timer: Option<Duration>,
    deadline: Option<Instant>,
}

enum End {
    At(Instant),
    LastNote,
}

// ───────────────────────── рабочий поток ─────────────────────────

struct Worker {
    settings: Arc<Mutex<Settings>>,
    notify: Notifier,
    status: Arc<Mutex<Status>>,
    phase: Phase,
    ctrl: Controllers,
    preroll: VecDeque<(u64, Instant, u4, MidiMessage)>,
    take: Option<Take>,
    session: Option<Session>,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Msg::Midi { ts_us, at, bytes }) => self.on_midi(ts_us, at, &bytes),
                Ok(Msg::Start { name, reply }) => {
                    let _ = reply.send(self.start(name));
                }
                Ok(Msg::Stop { reply }) => {
                    let was = self.phase != Phase::Idle;
                    let saved = self.finish(End::At(Instant::now()));
                    self.reset_idle();
                    let _ = reply.send((was, saved));
                }
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick(Instant::now());
        }
    }

    fn publish(&self) {
        let mut st = self.status.lock().unwrap();
        st.phase = self.phase;
        st.path = self.take.as_ref().map(|t| t.path.clone());
    }

    fn reset_idle(&mut self) {
        self.phase = Phase::Idle;
        self.take = None;
        self.session = None;
        self.preroll.clear();
        self.publish();
    }

    fn start(&mut self, name: Option<String>) -> Result<Started, String> {
        if self.phase != Phase::Idle {
            return Err("already recording".into());
        }
        let s = self.settings.lock().unwrap().clone();
        std::fs::create_dir_all(&s.dir)
            .map_err(|e| format!("cannot create folder {}: {e}", s.dir.display()))?;
        let name = name
            .map(|n| sanitize(&n))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| sanitize(&s.name));
        self.session = Some(Session {
            name: if name.is_empty() { "rec".into() } else { name },
            dir: s.dir.clone(),
            next: 1,
            timer: s.timer_secs.map(Duration::from_secs),
            deadline: None,
        });
        self.preroll.clear();
        if s.auto_start {
            self.phase = Phase::Armed;
            self.publish();
            Ok(Started::Armed)
        } else {
            let path = self.begin_take(Instant::now());
            Ok(Started::Recording(path))
        }
    }

    fn begin_take(&mut self, start: Instant) -> PathBuf {
        let (keep, bpm) = {
            let s = self.settings.lock().unwrap();
            (s.keep_controllers, s.bpm)
        };
        let session = self.session.as_mut().expect("session");
        let mut n = session.next;
        let path = loop {
            let p = session.dir.join(format!("{}{}.mid", session.name, n));
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        session.next = n;
        if let (Some(t), None) = (session.timer, session.deadline) {
            session.deadline = Some(start + t);
        }
        let init = if keep { self.ctrl.snapshot() } else { Vec::new() };
        self.take = Some(Take {
            path: path.clone(),
            n,
            start,
            ts0: None,
            lead_us: 0,
            events: Vec::new(),
            held: HashSet::new(),
            last_note_at: None,
            last_note_rel: 0,
            init,
            bpm,
        });
        self.phase = Phase::Recording;
        self.publish();
        path
    }

    fn on_midi(&mut self, ts: u64, at: Instant, bytes: &[u8]) {
        let Ok(LiveEvent::Midi { channel, message }) = LiveEvent::parse(bytes) else {
            return;
        };
        match self.phase {
            Phase::Idle => {}
            Phase::Armed => {
                let is_note_on = matches!(message, MidiMessage::NoteOn { vel, .. } if vel.as_int() > 0);
                if is_note_on {
                    let start = self.preroll.front().map(|e| e.1).unwrap_or(at);
                    let path = self.begin_take(start);
                    let pre: Vec<_> = self.preroll.drain(..).collect();
                    if let Some(take) = self.take.as_mut() {
                        for (t, a, c, m) in pre {
                            take.push(t, a, c, m);
                        }
                        take.push(ts, at, channel, message);
                    }
                    (self.notify)(Notice::TakeStarted { path });
                } else if matches!(
                    message,
                    MidiMessage::Controller { .. } | MidiMessage::PitchBend { .. } | MidiMessage::ProgramChange { .. }
                ) {
                    let window = Duration::from_millis(self.settings.lock().unwrap().preroll_ms);
                    self.preroll.push_back((ts, at, channel, message));
                    while let Some(front) = self.preroll.front() {
                        if at.saturating_duration_since(front.1) > window {
                            self.preroll.pop_front();
                        } else {
                            break;
                        }
                    }
                }
            }
            Phase::Recording => {
                if let Some(take) = self.take.as_mut() {
                    take.push(ts, at, channel, message);
                }
            }
        }
        self.ctrl.update(channel, &message);
    }

    fn tick(&mut self, now: Instant) {
        if self.phase == Phase::Idle {
            return;
        }
        // таймер сессии
        if let Some(dl) = self.session.as_ref().and_then(|s| s.deadline) {
            if now >= dl {
                let saved = self.finish(End::At(dl));
                self.reset_idle();
                (self.notify)(Notice::TimerElapsed { saved });
                return;
            }
        }
        // пауза
        if self.phase == Phase::Recording {
            let (split, secs) = {
                let s = self.settings.lock().unwrap();
                (s.pause_split, s.pause_secs)
            };
            if !split {
                return;
            }
            let quiet = self.take.as_ref().map_or(false, |t| {
                t.held.is_empty()
                    && t.last_note_at
                        .map_or(false, |la| now.saturating_duration_since(la).as_secs_f64() >= secs)
            });
            if quiet {
                if let Some(saved) = self.finish(End::LastNote) {
                    self.phase = Phase::Armed;
                    self.preroll.clear();
                    self.publish();
                    (self.notify)(Notice::PauseSplit { saved });
                }
            }
        }
    }

    /// Закрывает текущий файл. None — если писать нечего или произошла ошибка.
    fn finish(&mut self, end: End) -> Option<PathBuf> {
        let take = self.take.take()?;
        if take.events.is_empty() {
            return None;
        }
        let end_rel = match end {
            End::LastNote => take.last_note_rel,
            End::At(t) => (t.saturating_duration_since(take.start).as_micros() as u64).max(take.last_rel()),
        };
        match take.write(end_rel) {
            Ok((path, n)) => {
                if let Some(s) = self.session.as_mut() {
                    s.next = n + 1;
                }
                self.status.lock().unwrap().last_saved = Some(path.clone());
                Some(path)
            }
            Err(e) => {
                (self.notify)(Notice::Error(format!("cannot save recording: {e}")));
                None
            }
        }
    }
}

fn sanitize(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c })
        .collect()
}

// ───────────────────────── публичный Engine ─────────────────────────

pub struct Engine {
    settings: Arc<Mutex<Settings>>,
    cfg_path: Option<PathBuf>,
    tx: Sender<Msg>,
    status: Arc<Mutex<Status>>,
    conn: Option<MidiInputConnection<()>>,
    player: Option<Player>,
    notify: Notifier,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn new(settings: Settings, cfg_path: Option<PathBuf>, notify: Notifier) -> Engine {
        let settings = Arc::new(Mutex::new(settings));
        let status = Arc::new(Mutex::new(Status { phase: Phase::Idle, path: None, last_saved: None }));
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            settings: settings.clone(),
            notify: notify.clone(),
            status: status.clone(),
            phase: Phase::Idle,
            ctrl: Controllers::new(),
            preroll: VecDeque::new(),
            take: None,
            session: None,
        };
        let thread = std::thread::Builder::new()
            .name("midifon-worker".into())
            .spawn(move || worker.run(rx))
            .expect("spawn worker");
        Engine { settings, cfg_path, tx, status, conn: None, player: None, notify, thread: Some(thread) }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Меняет настройки (действуют сразу, в т.ч. во время записи) и сохраняет их в файл.
    pub fn update<F: FnOnce(&mut Settings)>(&self, f: F) -> Result<()> {
        let snapshot = {
            let mut s = self.settings.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        if let Some(p) = &self.cfg_path {
            snapshot.save(p)?;
        }
        Ok(())
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn input_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Подключает входное устройство и запоминает его в настройках.
    pub fn set_input(&mut self, name: &str) -> Result<()> {
        if self.status().phase != Phase::Idle {
            bail!("stop recording first");
        }
        let mut input = MidiInput::new("midifon")?;
        input.ignore(Ignore::All);
        let port = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| anyhow!("input device not found: {name}"))?;
        let tx = self.tx.clone();
        // сначала закрываем старое соединение (на ALSA/WinMM два входа одного устройства могут мешать)
        self.conn = None;
        let conn = input
            .connect(
                &port,
                "midifon-in",
                move |ts, bytes, _| {
                    let _ = tx.send(Msg::Midi { ts_us: ts, at: Instant::now(), bytes: bytes.to_vec() });
                },
                (),
            )
            .map_err(|e| anyhow!("cannot open input device: {e}"))?;
        self.conn = Some(conn);
        let name = name.to_string();
        self.update(|s| s.input = Some(name))
    }

    pub fn set_output(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.update(|s| s.output = Some(name))
    }

    /// Для тестов и виртуальных источников: подаёт MIDI-сообщение, как будто оно пришло с устройства.
    pub fn feed(&self, ts_us: u64, bytes: &[u8]) {
        let _ = self.tx.send(Msg::Midi { ts_us, at: Instant::now(), bytes: bytes.to_vec() });
    }

    pub fn start(&self, name: Option<String>) -> Result<Started> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Msg::Start { name, reply }).map_err(|_| anyhow!("engine stopped"))?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("engine did not answer"))?
            .map_err(|e| anyhow!(e))
    }

    /// Останавливает запись (с сохранением) и/или воспроизведение.
    pub fn stop(&mut self) -> StopOutcome {
        let (reply, rx) = mpsc::channel();
        let (was_recording, saved) = if self.tx.send(Msg::Stop { reply }).is_ok() {
            rx.recv_timeout(Duration::from_secs(5)).unwrap_or((false, None))
        } else {
            (false, None)
        };
        let was_playing = self.is_playing();
        if let Some(mut p) = self.player.take() {
            p.stop();
        }
        StopOutcome { was_recording, saved, was_playing }
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().map_or(false, |p| !p.finished())
    }

    pub fn play(&mut self, path: &Path) -> Result<()> {
        let out = self
            .settings()
            .output
            .ok_or_else(|| anyhow!("no output device; use 'outputs' then 'out <number>'"))?;
        if let Some(mut p) = self.player.take() {
            p.stop();
        }
        self.player = Some(Player::start(path, &out, self.notify.clone())?);
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.conn = None;
        if let Some(mut p) = self.player.take() {
            p.stop();
        }
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
