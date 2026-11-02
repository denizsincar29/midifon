use midifon_core::engine::{Engine, Notice, Notifier, Phase, Started};
use midifon_core::player::schedule;
use midifon_core::settings::Settings;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn setup(tag: &str, f: impl FnOnce(&mut Settings)) -> (Engine, Arc<Mutex<Vec<Notice>>>, std::path::PathBuf, Instant) {
    let dir = std::env::temp_dir().join(format!("midifon-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut s = Settings::default();
    s.dir = dir.clone();
    f(&mut s);
    let log = Arc::new(Mutex::new(Vec::new()));
    let l2 = log.clone();
    let notify: Notifier = Arc::new(move |n| l2.lock().unwrap().push(n));
    (Engine::new(s, None, notify), log, dir, Instant::now())
}

fn send(e: &Engine, t0: Instant, bytes: &[u8]) {
    e.feed(t0.elapsed().as_micros() as u64, bytes);
}

fn ms(n: u64) {
    std::thread::sleep(Duration::from_millis(n));
}

#[test]
fn pause_split_keeps_controllers() {
    let (mut e, log, dir, t0) = setup("split", |s| {
        s.pause_split = true;
        s.pause_secs = 0.3;
        s.auto_start = true;
    });
    send(&e, t0, &[0xB0, 7, 100]); // volume, пока ничего не пишем
    send(&e, t0, &[0xC0, 5]); // program change
    ms(50);
    assert!(matches!(e.start(None).unwrap(), Started::Armed));
    send(&e, t0, &[0xB0, 64, 127]); // педаль в pre-roll
    ms(50);
    send(&e, t0, &[0x90, 60, 90]);
    ms(100);
    send(&e, t0, &[0x80, 60, 0]);
    ms(700); // пауза > 0.3 с
    assert_eq!(e.status().phase, Phase::Armed);
    send(&e, t0, &[0x90, 62, 80]);
    ms(100);
    send(&e, t0, &[0x80, 62, 0]);
    ms(50);
    let out = e.stop();
    assert!(out.was_recording);

    let notes = log.lock().unwrap().clone();
    assert!(notes.iter().any(|n| matches!(n, Notice::PauseSplit { saved } if saved.ends_with("rec1.mid"))), "{notes:?}");
    assert!(notes.iter().any(|n| matches!(n, Notice::TakeStarted { .. })));
    assert!(dir.join("rec1.mid").exists() && dir.join("rec2.mid").exists());

    for f in ["rec1.mid", "rec2.mid"] {
        let sched = schedule(&std::fs::read(dir.join(f)).unwrap()).unwrap();
        // volume и program восстановлены в начале (время 0)
        assert!(sched.iter().any(|(t, b)| *t == 0 && b == &vec![0xB0, 7, 100]), "{f}: {sched:?}");
        assert!(sched.iter().any(|(t, b)| *t == 0 && b == &vec![0xC0, 5]), "{f}");
        // педаль нажата до первой ноты: тоже на тике 0
        assert!(sched.iter().any(|(t, b)| *t == 0 && b == &vec![0xB0, 64, 127]), "{f}: {sched:?}");
        // первая нота = время 0, а не после паузы ожидания
        let first_on = sched.iter().find(|(_, b)| b[0] == 0x90).unwrap().0;
        assert_eq!(first_on, 0, "{f}");
    }
    // первый файл заканчивается на последней ноте (≈100 мс после первой), а не на паузе
    let s1 = schedule(&std::fs::read(dir.join("rec1.mid")).unwrap()).unwrap();
    let on = s1.iter().find(|(_, b)| b[0] == 0x90).unwrap().0;
    let off = s1.iter().find(|(_, b)| b[0] == 0x80).unwrap().0;
    assert!((90_000..150_000).contains(&(off - on)), "note length {}", off - on);
}

#[test]
fn manual_start_and_timer() {
    let (mut e, log, dir, t0) = setup("timer", |s| {
        s.auto_start = false;
        s.timer_secs = Some(1);
        s.name = "jam".into();
    });
    let Started::Recording(p) = e.start(None).unwrap() else { panic!("expected recording") };
    assert!(p.ends_with("jam1.mid"));
    ms(200);
    send(&e, t0, &[0x90, 60, 90]);
    ms(100);
    send(&e, t0, &[0x80, 60, 0]);
    ms(1200); // таймер 1 с
    assert_eq!(e.status().phase, Phase::Idle);
    let notes = log.lock().unwrap().clone();
    assert!(notes.iter().any(|n| matches!(n, Notice::TimerElapsed { saved: Some(_) })), "{notes:?}");
    let sched = schedule(&std::fs::read(dir.join("jam1.mid")).unwrap()).unwrap();
    // ведущая тишина сохранена (~200 мс до первой ноты)
    assert!(sched[0].0 > 150_000, "lead {}", sched[0].0);
    // второй rec не затирает первый
    e.start(None).unwrap();
    ms(50);
    let o = e.stop();
    assert!(o.saved.is_none()); // пустую запись не пишем
}

#[test]
fn stop_closes_held_notes() {
    let (mut e, _log, dir, t0) = setup("held", |s| s.auto_start = true);
    e.start(Some("held".into())).unwrap();
    send(&e, t0, &[0x90, 60, 90]);
    ms(100);
    let o = e.stop();
    let p = o.saved.unwrap();
    assert!(p.ends_with("held1.mid"));
    let sched = schedule(&std::fs::read(dir.join("held1.mid")).unwrap()).unwrap();
    assert_eq!(sched.iter().filter(|(_, b)| b[0] == 0x80).count(), 1);
}

#[test]
fn pause_stops_recording_in_custom_dir() {
    use midifon_core::engine::StartOptions;
    let (mut e, log, dir, t0) = setup("pstop", |s| {
        s.pause_split = true;
        s.pause_secs = 0.3;
        s.auto_start = true;
    });
    let custom = dir.join("sub");
    e.start_with(StartOptions { name: Some("take".into()), dir: Some(custom.clone()), pause_stops: true }).unwrap();
    send(&e, t0, &[0x90, 60, 90]);
    ms(100);
    send(&e, t0, &[0x80, 60, 0]);
    ms(700);
    // запись остановилась сама, в Armed не вернулась
    assert_eq!(e.status().phase, Phase::Idle);
    assert!(custom.join("take1.mid").exists());
    let notes = log.lock().unwrap().clone();
    assert!(notes.iter().any(|n| matches!(n, Notice::PauseStopped { saved } if saved.ends_with("take1.mid"))), "{notes:?}");
    assert!(!notes.iter().any(|n| matches!(n, Notice::PauseSplit { .. })));
    assert_eq!(e.status().last_saved.unwrap(), custom.join("take1.mid"));
    let o = e.stop();
    assert!(!o.was_recording);
}

#[test]
fn keep_off_still_writes_controllers_changed_while_armed() {
    let (mut e, _log, dir, t0) = setup("keepoff", |s| {
        s.auto_start = true;
        s.keep_controllers = false;
    });
    send(&e, t0, &[0xB0, 7, 100]); // до rec: не должно попасть
    ms(30);
    e.start(Some("k".into())).unwrap();
    send(&e, t0, &[0xB0, 10, 20]); // pan после rec, до ноты: должно попасть на тик 0
    send(&e, t0, &[0xB0, 10, 30]); // последнее значение побеждает
    ms(50);
    send(&e, t0, &[0x90, 60, 90]);
    ms(80);
    send(&e, t0, &[0x80, 60, 0]);
    ms(30);
    e.stop();
    let sched = schedule(&std::fs::read(dir.join("k1.mid")).unwrap()).unwrap();
    assert!(sched.iter().any(|(t, b)| *t == 0 && b == &vec![0xB0, 10, 30]), "{sched:?}");
    assert!(!sched.iter().any(|(_, b)| b == &vec![0xB0, 10, 20]), "{sched:?}");
    assert!(!sched.iter().any(|(_, b)| b == &vec![0xB0, 7, 100]), "{sched:?}");
    // сначала контроллер, потом нота, оба на тике 0, в таком порядке
    assert_eq!(sched[0].1, vec![0xB0, 10, 30]);
    assert_eq!(sched[1].1, vec![0x90, 60, 90]);
    assert_eq!(sched[1].0, 0);
}
