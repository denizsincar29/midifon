use anyhow::Result;
use midly::num::{u24, u28, u4};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, Track, TrackEvent, TrackEventKind};
use std::path::Path;

pub const PPQ: u16 = 480;

#[derive(Debug, Clone, Copy)]
pub struct TimedEvent {
    pub rel_us: u64,
    pub channel: u4,
    pub message: MidiMessage,
}

/// Пишет формат-0 SMF. `init` — события на тике 0 (восстановление контроллеров).
pub fn write(
    path: &Path,
    bpm: f64,
    init: &[(u4, MidiMessage)],
    events: &[TimedEvent],
    end_us: u64,
) -> Result<()> {
    let bpm = if bpm > 1.0 { bpm } else { 120.0 };
    let tempo = (60_000_000.0 / bpm).round() as u32;
    let to_tick = |us: u64| -> u64 { ((us as f64) * (PPQ as f64) * bpm / 60_000_000.0).round() as u64 };
    let delta = |d: u64| u28::from(d.min(0x0FFF_FFFF) as u32);

    let mut track: Track = Vec::with_capacity(events.len() + init.len() + 2);
    track.push(TrackEvent {
        delta: 0.into(),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(tempo))),
    });
    for (ch, msg) in init {
        track.push(TrackEvent {
            delta: 0.into(),
            kind: TrackEventKind::Midi { channel: *ch, message: *msg },
        });
    }
    let mut prev = 0u64;
    for ev in events {
        let t = to_tick(ev.rel_us).max(prev);
        track.push(TrackEvent {
            delta: delta(t - prev),
            kind: TrackEventKind::Midi { channel: ev.channel, message: ev.message },
        });
        prev = t;
    }
    let end = to_tick(end_us).max(prev);
    track.push(TrackEvent {
        delta: delta(end - prev),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });

    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(PPQ.into())),
        tracks: vec![track],
    };
    smf.save(path)?;
    Ok(())
}
