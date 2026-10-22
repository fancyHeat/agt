use super::*;

/// A fresh, empty folder for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agt-history-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn line(e: &Entry) -> serde_json::Value {
    let (line, _) = record(e, &LocalTime::at(e.started_at));
    assert!(
        line.ends_with('\n') && line.matches('\n').count() == 1,
        "{line}"
    );
    serde_json::from_str(&line).unwrap()
}

#[test]
fn dates_round_trip() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    for days in [-1, 59, 11_016, 20_742, 20_743, 47_482] {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
    }
    assert_eq!(parse_day("2026-10-16"), Some(days_from_civil(2026, 10, 16)));
    for bad in [
        "2026-1-16",
        "2026-13-01",
        "26-10-16",
        "2026-10-16x",
        "notes.txt",
        "2026_10_16",
    ] {
        assert_eq!(parse_day(bad), None, "{bad}");
    }
}

#[test]
fn local_time_formats() {
    // 2026-10-16 12:23:05.123 UTC, two hours east.
    let ms = days_from_civil(2026, 10, 16) * 86_400_000 + (12 * 3600 + 23 * 60 + 5) * 1000 + 123;
    assert_eq!(ms, 1_792_153_385_123);
    let t = LocalTime::with_offset(ms, 7200);
    assert_eq!(t.day(), "2026-10-16");
    assert_eq!(t.file_stem(), "142305-123");
    assert_eq!(t.iso(), "2026-10-16T14:23:05.123+02:00");
    // West of UTC, across midnight.
    let t = LocalTime::with_offset(ms - 12 * 3600 * 1000, -(5 * 3600 + 30 * 60));
    assert_eq!(t.iso(), "2026-10-15T18:53:05.123-05:30");
    // Whatever this machine's zone, it's within a day of UTC and whole minutes.
    let off = utc_offset(ms / 1000);
    assert!(off.abs() <= 14 * 3600 && off % 60 == 0, "{off}");
}

#[test]
fn records_hold_only_what_is_saved() {
    let started_at = 1_792_153_385_123;
    // Text, polished.
    let e = Entry {
        started_at,
        audio: vec![0.1; 16],
        audio_secs: 3.521_9,
        text: Some(Text {
            raw: "um so lets meet on tuesday".into(),
            polished: Some("So, let's meet on Tuesday.".into()),
            polish_outcome: Some("polished"),
            pasted: Some("So, let's meet on Tuesday. ".into()),
        }),
    };
    let v = line(&e);
    let stem = LocalTime::at(started_at).file_stem();
    assert_eq!(v["started_at"], started_at);
    assert_eq!(v["audio"], format!("{stem}.wav"));
    assert_eq!(v["audio_secs"], 3.522);
    assert_eq!(v["raw"], "um so lets meet on tuesday");
    assert_eq!(v["polished"], "So, let's meet on Tuesday.");
    assert_eq!(v["polish_outcome"], "polished");
    assert_eq!(v["pasted"], "So, let's meet on Tuesday. ");
    assert!(
        v["time"]
            .as_str()
            .unwrap()
            .starts_with(&LocalTime::at(started_at).day())
    );
    // Nothing else (settings and timings are in agt.db, joined on started_at), in this order.
    let (text, _) = record(&e, &LocalTime::at(e.started_at));
    let keys = [
        "started_at",
        "time",
        "audio",
        "audio_secs",
        "raw",
        "polished",
        "polish_outcome",
        "pasted",
    ];
    assert_eq!(v.as_object().unwrap().len(), keys.len());
    let at = |k: &str| text.find(&format!("\"{k}\":")).unwrap();
    assert!(keys.windows(2).all(|w| at(w[0]) < at(w[1])), "{text}");

    // Polish off, and no audio: polished/polish_outcome are null, no audio key.
    let e = Entry {
        started_at,
        audio_secs: 1.0,
        text: Some(Text {
            raw: "hello there".into(),
            pasted: Some("hello there ".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let v = line(&e);
    assert!(v.get("audio").is_none());
    assert!(v["polished"].is_null() && v["polish_outcome"].is_null());
    assert_eq!(v["pasted"], "hello there ");

    // No speech: an empty transcript, nothing pasted.
    let e = Entry {
        started_at,
        text: Some(Text::default()),
        ..Default::default()
    };
    let v = line(&e);
    assert_eq!(v["raw"], "");
    assert!(v["pasted"].is_null());

    // Audio only: no text keys at all.
    let e = Entry {
        started_at,
        audio: vec![0.0; 16],
        audio_secs: 0.001,
        text: None,
    };
    let v = line(&e);
    assert_eq!(v.as_object().unwrap().len(), 4, "{v}");
    assert!(v.get("raw").is_none() && v.get("pasted").is_none());
}

#[test]
fn write_appends_lines_and_wavs() {
    let dir = temp_dir("write");
    let samples: Vec<f32> = (0..3200).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
    let at = now_ms();
    for (i, text) in ["first", "second \"quoted\"\nline"].into_iter().enumerate() {
        let e = Entry {
            started_at: at + i as i64 * 1000,
            audio: samples.clone(),
            audio_secs: 0.2,
            text: Some(Text {
                raw: text.into(),
                ..Default::default()
            }),
        };
        write(&dir, &e).unwrap();
    }
    let day = dir.join(LocalTime::at(at).day());
    let lines = fs::read_to_string(day.join(RECORDS_FILE)).unwrap();
    let records: Vec<serde_json::Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // (A run straddling midnight would split them; the second is then in the next folder.)
    assert!(!records.is_empty());
    assert_eq!(records[0]["raw"], "first");
    if let Some(r) = records.get(1) {
        assert_eq!(r["raw"], "second \"quoted\"\nline");
    }
    let wav = day.join(records[0]["audio"].as_str().unwrap());
    let back = agt_audio::wav::read_16k_mono(&wav).unwrap();
    assert_eq!(back.len(), samples.len());
    assert!(samples.iter().zip(&back).all(|(a, b)| (a - b).abs() < 1e-3));
    // Written under a temporary name, then renamed: none left behind.
    assert!(
        fs::read_dir(&day).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn read_returns_what_was_written_in_order() {
    let dir = temp_dir("read");
    // Two days (fixed dates, written by hand: `write` would use this machine's zone).
    let one = dir.join("2026-10-15");
    let two = dir.join("2026-10-16");
    for d in [&one, &two] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(
        two.join(RECORDS_FILE),
        "{\"started_at\":3,\"time\":\"x\",\"audio\":\"090000-003.wav\",\"audio_secs\":1.0,\"raw\":\"into Maine\",\"polished\":null,\"polish_outcome\":null,\"pasted\":\"into main \"}\n\
         {\"started_at\":4,\"time\":\"x\",\"audio_secs\":1.0}\n",
    )
    .unwrap();
    // A line cut short by a crash, then an intact one; text not saved.
    fs::write(
        one.join(RECORDS_FILE),
        "{\"started_at\":1,\"ti\n{\"started_at\":2,\"time\":\"x\",\"audio\":\"235959-999.wav\",\"audio_secs\":2.0}\n",
    )
    .unwrap();
    fs::write(dir.join("notes.txt"), "not a day").unwrap();
    let saved = read(&dir);
    assert_eq!(
        saved,
        [
            Saved {
                started_at: 2,
                day: "2026-10-15".into(),
                audio: Some(one.join("235959-999.wav")),
                raw: None,
            },
            Saved {
                started_at: 3,
                day: "2026-10-16".into(),
                audio: Some(two.join("090000-003.wav")),
                raw: Some("into Maine".into()),
            },
            Saved {
                started_at: 4,
                day: "2026-10-16".into(),
                audio: None,
                raw: None,
            },
        ]
    );
    assert!(read(&dir.join("missing")).is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_line_cut_short_by_a_crash_does_not_swallow_the_next() {
    let dir = temp_dir("torn");
    let path = dir.join(RECORDS_FILE);
    append_line(&path, "{\"a\":1}\n").unwrap();
    fs::write(&path, "{\"a\":1}\n{\"b\":").unwrap(); // the crash
    append_line(&path, "{\"c\":3}\n").unwrap();
    append_line(&path, "{\"d\":4}\n").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\"a\":1}\n{\"b\":\n{\"c\":3}\n{\"d\":4}\n"
    );
    // An empty file gets no leading newline.
    let empty = dir.join("empty.jsonl");
    fs::write(&empty, "").unwrap();
    append_line(&empty, "{}\n").unwrap();
    assert_eq!(fs::read_to_string(&empty).unwrap(), "{}\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn prune_deletes_old_day_folders_only() {
    let dir = temp_dir("prune");
    let today = days_from_civil(2026, 10, 16);
    for name in [
        "2026-10-16",
        "2026-09-16",
        "2026-09-15",
        "2025-01-01",
        "notes",
    ] {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join(RECORDS_FILE), "{}\n").unwrap();
    }
    fs::write(dir.join("2020-01-01"), "a file, not a day folder").unwrap();
    let left = || {
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    // 0 would mean forever: the writer never prunes then. 30: today and the 30 days before.
    prune(&dir, today, 30);
    assert_eq!(left(), ["2020-01-01", "2026-09-16", "2026-10-16", "notes"]);
    prune(&dir, today, 1);
    assert_eq!(left(), ["2020-01-01", "2026-10-16", "notes"]);
    // A missing folder is fine.
    prune(&dir.join("missing"), today, 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn keep_days_zero_keeps_everything() {
    let dir = temp_dir("forever");
    fs::create_dir_all(dir.join("2001-01-01")).unwrap();
    let config = HistoryConfig {
        save_text: true,
        save_audio: false,
        keep_days: 0,
    };
    let h = start(&config, dir.clone()).unwrap();
    assert!(h.save_text && !h.save_audio);
    let at = now_ms();
    h.save(Box::new(Entry {
        started_at: at,
        text: Some(Text::default()),
        ..Default::default()
    }));
    // On disk once flush returns (as at exit).
    assert!(flush(Duration::from_secs(5)));
    assert!(
        dir.join(LocalTime::at(at).day())
            .join(RECORDS_FILE)
            .exists()
    );
    assert!(dir.join("2001-01-01").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn off_saves_nothing() {
    let config = HistoryConfig {
        keep_days: 0,
        ..Default::default()
    };
    assert!(start(&config, temp_dir("off")).is_none());
}
