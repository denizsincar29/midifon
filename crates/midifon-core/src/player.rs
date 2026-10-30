use crate::engine::{Notice, Notifier};
use anyhow::{anyhow, bail, Result};
use midir::{MidiOutput, MidiOutputConnection};
use midly::live::LiveEvent;
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Разворачивает SMF в плоский список (время в мкс, байты сообщения).
pub fn schedule(data: &[u8]) -> Result<Vec<(u64, Vec<u8>)>> {
    let smf = Smf::parse(data)?;
    let ppq = match smf.header.timing {
        Timing::Metrical(t) => t.as_int() as f64,
        Timing::Timecode(..) => bail!("SMPTE timing is not supported"),
    };

    enum Kind {
        Tempo(u32),
        Midi(Vec<u8>),
    }
    // (тик, приоритет, вид); приоритет: темп, note off, прочее, note on
    let mut items: Vec<(u64, u8, Kind)> = Vec::new();
    for track in &smf.tracks {
        let mut tick = 0u64;
        for ev in track {
            tick += ev.delta.as_int() as u64;
            match ev.kind {
                TrackEventKind::Meta(MetaMessage::Tempo(t)) => items.push((tick, 0, Kind::Tempo(t.as_int()))),
                TrackEventKind::Midi { channel, message } => {
                    let prio = match message {
                        MidiMessage::NoteOff { .. } => 1,
                        MidiMessage::NoteOn { vel, .. } if vel.as_int() == 0 => 1,
                        MidiMessage::NoteOn { .. } => 3,
                        _ => 2,
                    };
                    let mut buf = Vec::with_capacity(3);
                    LiveEvent::Midi { channel, message }.write_std(&mut buf)?;
                    items.push((tick, prio, Kind::Midi(buf)));
                }
                _ => {}
            }
        }
    }
    items.sort_by_key(|(t, p, _)| (*t, *p));

    let mut out = Vec::new();
    let mut tempo = 500_000f64;
    let mut last_tick = 0u64;
    let mut us = 0f64;
    for (tick, _, kind) in items {
        us += (tick - last_tick) as f64 * tempo / ppq;
        last_tick = tick;
        match kind {
            Kind::Tempo(t) => tempo = t as f64,
            Kind::Midi(b) => out.push((us.round() as u64, b)),
        }
    }
    Ok(out)
}

pub struct Player {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Player {
    pub fn start(path: &Path, out_name: &str, notify: Notifier) -> Result<Player> {
        let data = std::fs::read(path).map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
        let events = schedule(&data)?;
        if events.is_empty() {
            bail!("the file has no MIDI events");
        }
        let out = MidiOutput::new("midifon")?;
        let port = out
            .ports()
            .into_iter()
            .find(|p| out.port_name(p).map(|n| n == out_name).unwrap_or(false))
            .ok_or_else(|| anyhow!("output device not found: {out_name}"))?;
        let conn = out
            .connect(&port, "midifon-out")
            .map_err(|e| anyhow!("cannot open output device: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (stop2, done2) = (stop.clone(), done.clone());
        let handle = std::thread::spawn(move || {
            let mut conn = conn;
            let start = Instant::now() + Duration::from_millis(100);
            let mut natural = true;
            for (us, bytes) in &events {
                if !wait_until(start + Duration::from_micros(*us), &stop2) {
                    natural = false;
                    break;
                }
                let _ = conn.send(bytes);
            }
            silence(&mut conn);
            done2.store(true, Ordering::SeqCst);
            if natural {
                notify(Notice::PlaybackFinished);
            }
        });
        Ok(Player { stop, done, handle: Some(handle) })
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Ждёт до момента `target`; false, если попросили остановиться.
fn wait_until(target: Instant, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= target {
            return true;
        }
        let left = target - now;
        if left > Duration::from_millis(2) {
            std::thread::sleep((left - Duration::from_millis(1)).min(Duration::from_millis(20)));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn silence(conn: &mut MidiOutputConnection) {
    for ch in 0..16u8 {
        for cc in [64u8, 120, 123] {
            let _ = conn.send(&[0xB0 | ch, cc, 0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smf::{self, TimedEvent};
    use midly::num::{u4, u7};

    #[test]
    fn roundtrip_timing() {
        let dir = std::env::temp_dir().join("midifon-test-rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.mid");
        let on = MidiMessage::NoteOn { key: u7::from(60), vel: u7::from(90) };
        let off = MidiMessage::NoteOff { key: u7::from(60), vel: u7::from(0) };
        let evs = [
            TimedEvent { rel_us: 0, channel: u4::from(0), message: on },
            TimedEvent { rel_us: 500_000, channel: u4::from(0), message: off },
        ];
        smf::write(&path, 120.0, &[], &evs, 600_000).unwrap();
        let sched = schedule(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(sched.len(), 2);
        assert_eq!(sched[0].0, 0);
        assert!((sched[1].0 as i64 - 500_000).abs() < 2_000, "got {}", sched[1].0);
        assert_eq!(sched[0].1, vec![0x90, 60, 90]);
    }
}
