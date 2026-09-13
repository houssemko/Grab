use super::*;

static QUEUE_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Serializes every test that iterates the shared glib default
/// MainContext (MainLoops and drain pumps). glib futures are bound to
/// their spawning thread: a foreign iteration polling another test's
/// pending source aborts on the thread-affinity guard. Take AFTER
/// QUEUE_FILE_LOCK, always that order.
static MAIN_LOOP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Both test locks in the one safe order (queue, then loop). Take these
/// together and never the loop lock before the queue lock: a reversed
/// order across tests would deadlock the suite into a CI-timeout hang.
fn test_locks() -> (
    std::sync::MutexGuard<'static, ()>,
    std::sync::MutexGuard<'static, ()>,
) {
    let q = QUEUE_FILE_LOCK.lock().unwrap();
    let l = MAIN_LOOP_LOCK.lock().unwrap();
    (q, l)
}

fn test_queue_file(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "grab-q-{:?}-{tag}.json",
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    std::env::set_var("GRAB_QUEUE_FILE", &p);
    p
}

fn test_settings() -> gio::Settings {
    std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
    std::env::set_var("GSETTINGS_BACKEND", "memory");
    gio::Settings::new("io.github.houssemko.Grab")
}

/// Fresh loopback port per call. Parallel tests share one process (and
/// pid), so pid-derived ports collide; a counter never repeats.
/// Salted per process: crashed runs leak python servers that keep
/// listening, and without the salt the next run reuses their ports and
/// talks to stale fixtures.
fn test_port(offset: u16) -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    static SALT: OnceLock<u64> = OnceLock::new();
    let salt = *SALT.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        std::process::id() as u64 ^ nanos ^ ((nanos >> 17) | 1)
    });
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    21000
        + ((n as u64)
            .wrapping_mul(173)
            .wrapping_add(offset as u64)
            .wrapping_add(salt)
            % 40000) as u16
}

/// One throwaway HTTP fixture: temp dirs, payload file, and a running
/// throttled_server.py. On success the test kills the server and removes
/// `dir`; on failure everything stays behind (ranges.log replay).
struct Fixture {
    dir: std::path::PathBuf,
    dl: std::path::PathBuf,
    payload: Vec<u8>,
    port: u16,
    server: Rc<RefCell<std::process::Child>>,
}

fn spawn_fixture(
    tag: &str,
    served_name: &str,
    payload_len: u32,
    sleep_secs: &str,
    extra_args: &[&str],
    port_offset: u16,
) -> Fixture {
    let dir = std::env::temp_dir().join(format!("grab-{tag}-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
    std::fs::write(srv.join(served_name), &payload).unwrap();
    let port = test_port(port_offset);
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(format!(
        "{}/tests/throttled_server.py",
        env!("CARGO_MANIFEST_DIR")
    ))
    .arg(port.to_string())
    .arg(srv.join(served_name))
    .arg(dir.join("ranges.log"))
    .arg(sleep_secs);
    for a in extra_args {
        cmd.arg(a);
    }
    let server = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 range server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");
    Fixture {
        dir,
        dl,
        payload,
        port,
        server: Rc::new(RefCell::new(server)),
    }
}

/// Fail the current test, killing its server first (dirs stay for logs).
fn abort(server: &Rc<RefCell<std::process::Child>>, msg: &str) -> ! {
    let _ = server.borrow_mut().kill();
    panic!("TEST FAILURE: {msg}");
}

/// Success-path teardown shared by the live-server tests.
fn cleanup(server: &Rc<RefCell<std::process::Child>>, dir: &std::path::Path) {
    let _ = server.borrow_mut().kill();
    let _ = std::fs::remove_dir_all(dir);
}

/// Watchdog + run + quiescence drain shared by every main-loop test: the
/// drain pumps until the context goes quiet so no pending future is left
/// for another test's loop to trip over. Callers must hold
/// MAIN_LOOP_LOCK: only one test may iterate at a time.
fn run_loop(main_loop: &glib::MainLoop, watchdog_secs: u64) {
    let watchdog = main_loop.clone();
    let timed_out = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let (flag, done) = (Arc::clone(&timed_out), Arc::clone(&finished));
    std::thread::spawn(move || {
        // Poll so a finished test doesn't leave us sleeping for minutes.
        for _ in 0..watchdog_secs.max(1) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if done.load(Ordering::SeqCst) || !watchdog.is_running() {
                return;
            }
        }
        if !done.load(Ordering::SeqCst) && watchdog.is_running() {
            flag.store(true, Ordering::SeqCst);
            watchdog.quit();
        }
    });
    main_loop.run();
    finished.store(true, Ordering::SeqCst);
    assert!(!timed_out.load(Ordering::SeqCst), "TEST TIMEOUT");
    let ctx = glib::MainContext::default();
    let mut idle_rounds = 0;
    while idle_rounds < 50 {
        if ctx.iteration(false) {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
#[test]
fn splits_pieces() {
    // Too small: single-stream fallback.
    assert!(plan_pieces(0, 4).is_empty());
    assert!(plan_pieces(1024, 4).is_empty());
    assert!(plan_pieces(8 * 1024 * 1024 - 1, 4).is_empty());
    // Just under one segment per connection still splits if >= 2 fit.
    assert!(!plan_pieces(16 * 1024 * 1024 - 1, 4).is_empty());
    // One connection disables splitting entirely.
    assert!(plan_pieces(100 * 1024 * 1024, 1).is_empty());
    assert!(plan_pieces(100 * 1024 * 1024, 0).is_empty());
    // 100 MB at 4 connections: full 1 MB coverage, inclusive ends.
    let pieces = plan_pieces(100 * 1024 * 1024, 4);
    assert_eq!(pieces.len(), 100);
    assert_eq!(pieces[0], (0, 1024 * 1024 - 1));
    assert_eq!(pieces[99].1, 100 * 1024 * 1024 - 1);
    for w in pieces.windows(2) {
        assert_eq!(w[0].1 + 1, w[1].0);
    }
    // Uneven tail.
    let pieces = plan_pieces(9_000_000, 4);
    assert_eq!(pieces.len(), 9);
    assert_eq!(pieces.last().unwrap().1, 8_999_999);
    // Absurd server-claimed sizes never split (FIND-01): no giant
    // bitmap, no terabyte sparse file, just single-stream.
    assert!(plan_pieces(u64::MAX, 16).is_empty());
    assert!(plan_pieces(2 * (1 << 40), 16).is_empty());
    // Worker split count follows connections x 4 MB.
    assert_eq!(split_count(100 * 1024 * 1024, 4), 4);
    assert_eq!(split_count(100 * 1024 * 1024, 99), 16);
    assert_eq!(split_count(1024, 4), 0);
}

#[test]
fn piece_size_scales_with_total() {
    // Floor holds through ~4 GB: everything ordinary splits as before.
    assert_eq!(piece_len(1024), PIECE_MIN);
    assert_eq!(piece_len(4 * 1024 * 1024 * 1024), PIECE_MIN);
    // Beyond that pieces grow toward ~4k per download, capped at 16 MB.
    assert_eq!(piece_len(20 * 1024 * 1024 * 1024), 5 * 1024 * 1024);
    assert_eq!(piece_len(1 << 40), PIECE_MAX);
    // Bitmap stays aligned: the piece count covers the total exactly.
    let total = 20 * 1024 * 1024 * 1024;
    let st = SegmentState::new(total);
    assert_eq!(st.done.len() as u64, total.div_ceil(piece_len(total)));
    let pieces = plan_pieces(total, 4);
    assert_eq!(pieces.len() as u64, total.div_ceil(piece_len(total)));
    assert_eq!(pieces.last().unwrap().1, total - 1);
}

#[test]
fn parses_range_totals() {
    assert_eq!(parse_content_range("bytes 0-0/12345"), Some((0, 0, 12345)));
    assert_eq!(
        parse_content_range("bytes 1048576-2097151/8388608"),
        Some((1048576, 2097151, 8388608))
    );
    // Inverted range or garbage rejected.
    assert_eq!(parse_content_range("bytes 99-0/12345"), None);
    assert_eq!(parse_content_range("bytes */12345"), None);
    assert_eq!(parse_content_range("nonsense"), None);
    assert_eq!(parse_content_range("bytes 0-0/0"), None);
}

#[test]
fn resolves_response_totals() {
    assert_eq!(response_total(Some(100), None, false, 0), Some(100));
    assert_eq!(response_total(Some(60), None, true, 40), Some(100));
    // Chunked 206 without Content-Length: trust Content-Range.
    assert_eq!(
        response_total(None, Some("bytes 40-99/100"), true, 40),
        Some(100)
    );
    // Range starts elsewhere, or no headers at all: unknown.
    assert_eq!(response_total(None, Some("bytes 0-0/100"), true, 40), None);
    assert_eq!(response_total(None, None, true, 40), None);
    assert_eq!(response_total(None, None, false, 0), None);
}

#[test]
fn rejects_size_mismatched_restarts() {
    // Resumed range answered 200 with a different length: reject.
    assert!(rejects_unexpected_restart(false, 40, Some(100), Some(12)));
    // Same length, fresh start, or unknown lengths: proceed.
    assert!(!rejects_unexpected_restart(false, 40, Some(100), Some(100)));
    assert!(!rejects_unexpected_restart(false, 0, Some(100), Some(12)));
    assert!(!rejects_unexpected_restart(true, 40, Some(100), Some(60)));
    assert!(!rejects_unexpected_restart(false, 40, None, Some(12)));
    assert!(!rejects_unexpected_restart(false, 40, Some(100), None));
}

#[test]
fn prefix_len_caps_at_total() {
    // Tail piece is short: two done pieces of a 1.5 MB file cover
    // 1.5 MB, not 2 MB (an uncapped prefix could extend the file).
    let mut st = SegmentState::new(1_500_000);
    st.mark(0);
    st.mark(1);
    assert_eq!(st.prefix_len(), 1_500_000);
}

#[test]
fn content_disposition_star_case_insensitive() {
    // Servers emit FILENAME*= too; parameter names are case-insensitive.
    assert_eq!(
        filename_from_content_disposition("attachment; FILENAME*=UTF-8''%E2%82%ACrates.mp4"),
        Some("€rates.mp4".to_string())
    );
}

#[test]
fn dedupe_caps_iterations() {
    // Everything taken: must still return (not stat the disk forever).
    let name = dedupe_filename("f.iso", |_| true);
    assert!(name.starts_with("f ("));
}

#[test]
fn corrupt_queue_is_quarantined() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("quarantine");
    std::fs::write(&qf, b"{not json").unwrap();
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 0);
    let bak = qf.with_extension("json.bak");
    assert!(bak.exists());
    let _ = std::fs::remove_file(&bak);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn rename_noreplace_never_clobbers() {
    let dir = std::env::temp_dir().join(format!("grab-rename-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b, c) = (dir.join("a.bin"), dir.join("b.bin"), dir.join("c.bin"));
    std::fs::write(&a, b"aaa").unwrap();
    std::fs::write(&b, b"bbb").unwrap();
    // Occupied destination: error, victim untouched.
    assert!(rename_noreplace(&a, &b).is_err());
    assert_eq!(std::fs::read(&b).unwrap(), b"bbb");
    assert_eq!(std::fs::read(&a).unwrap(), b"aaa");
    // Free destination: moved, source gone.
    assert!(rename_noreplace(&a, &c).is_ok());
    assert!(!a.exists());
    assert_eq!(std::fs::read(&c).unwrap(), b"aaa");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overcap_queue_keeps_active_first() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("overcap");
    let settings = test_settings();
    let mut items = vec![
        StoredItem {
            url: "https://example.com/active.iso".to_string(),
            dest_dir: "/tmp/dl".to_string(),
            filename: "active.iso".to_string(),
            status: StoredStatus::Queued,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
        },
        StoredItem {
            url: "https://example.com/paused.iso".to_string(),
            dest_dir: "/tmp/dl".to_string(),
            filename: "paused.iso".to_string(),
            status: StoredStatus::Paused,
            progress: 0.5,
            segments: None,
            selected_files: None,
            output_dir: None,
        },
    ];
    for i in 0..1000 {
        items.push(StoredItem {
            url: format!("https://example.com/f{i}.iso"),
            dest_dir: "/tmp/dl".to_string(),
            filename: format!("f{i}.iso"),
            status: StoredStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
        });
    }
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items,
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    settings.set_int("max-concurrent", 1).unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    // Occupy the only slot so Queued restores can't spawn downloads.
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    m.running.borrow_mut().insert(99, holder);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 1000);
    let names: Vec<String> = (0..m.store().n_items())
        .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
        .map(|it| it.filename().to_string())
        .collect();
    // Resumable intent survives even though it was oldest in the file.
    assert!(names.contains(&"active.iso".to_string()));
    assert!(names.contains(&"paused.iso".to_string()));
    // Newest history kept, oldest history dropped.
    assert!(names.contains(&"f999.iso".to_string()));
    assert!(!names.contains(&"f0.iso".to_string()));
    assert!(!names.contains(&"f1.iso".to_string()));
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn forgets_bitmap_beyond_prefix() {
    let mut st = SegmentState::new(4 * PIECE_MIN);
    st.mark(0);
    st.mark(1);
    st.mark(3);
    st.forget_beyond_prefix();
    assert_eq!(
        st.missing(),
        vec![
            (2, 2 * PIECE_MIN, 3 * PIECE_MIN - 1),
            (3, 3 * PIECE_MIN, 4 * PIECE_MIN - 1),
        ]
    );
    assert_eq!(st.prefix_len(), 2 * PIECE_MIN);
}

#[test]
fn truncates_to_prefix() {
    let dir = std::env::temp_dir().join(format!("grab-trunc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("p.bin");
    std::fs::write(&file, vec![7u8; 3 * PIECE_MIN as usize]).unwrap();
    // Pieces 0,1 done, 2 missing: shrink to 2 MB.
    let mut st = SegmentState::new(3 * PIECE_MIN);
    st.mark(0);
    st.mark(1);
    truncate_to_prefix(&file, &st);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_MIN);
    // Already short: untouched.
    truncate_to_prefix(&file, &st);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_MIN);
    // Nothing done: emptied.
    let st = SegmentState::new(3 * PIECE_MIN);
    truncate_to_prefix(&file, &st);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_content_disposition() {
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"movie.mp4\""),
        Some("movie.mp4".to_string())
    );
    assert_eq!(
        filename_from_content_disposition("attachment; filename=movie.mp4"),
        Some("movie.mp4".to_string())
    );
    // RFC 5987 encoding wins over the plain fallback.
    assert_eq!(
        filename_from_content_disposition(
            "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E2%82%ACrates.mp4"
        ),
        Some("€rates.mp4".to_string())
    );
    // Directory components stripped (some servers send Windows paths).
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"C:\\\\dl\\\\movie.mp4\""),
        Some("movie.mp4".to_string())
    );
    assert_eq!(
        filename_from_content_disposition("attachment; FILENAME=\"upper.mp4\""),
        Some("upper.mp4".to_string())
    );
    // Garbage rejected: traversal, empty, missing.
    assert_eq!(filename_from_content_disposition("attachment"), None);
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"../evil.sh\""),
        Some("evil.sh".to_string())
    );
    assert_eq!(
        filename_from_content_disposition("attachment; filename=\"\""),
        None
    );
    assert_eq!(filename_from_content_disposition(""), None);
}

#[test]
fn detects_sparse_holes() {
    let dir = std::env::temp_dir().join(format!("grab-holes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sparse = dir.join("sparse.bin");
    let f = std::fs::File::create(&sparse).unwrap();
    f.set_len(20_000_000).unwrap();
    assert!(has_holes(&sparse));
    std::fs::write(&sparse, vec![9u8; 100]).unwrap();
    assert!(!has_holes(&sparse));
    assert!(!has_holes(&dir.join("missing.bin")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_rates() {
    assert_eq!(parse_rate(""), None);
    assert_eq!(parse_rate("0"), None);
    assert_eq!(parse_rate("500K"), Some(500 * 1024));
    assert_eq!(parse_rate("2M"), Some(2 * 1024 * 1024));
    assert_eq!(parse_rate("1.5M"), Some(1572864));
    assert_eq!(parse_rate("3G"), Some(3 * 1024 * 1024 * 1024));
    assert_eq!(parse_rate("1024"), Some(1024));
    assert_eq!(parse_rate("junk"), None);
    assert_eq!(parse_rate("-5K"), None);
}

#[test]
fn rate_limit_follows_settings_live() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("rate-live");
    let settings = test_settings();
    // The manager's watch publishes; the engine cap follows with no
    // re-queue. Junk reads as unlimited.
    let _manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    settings.set_string("speed-limit", "1M").unwrap();
    assert_eq!(live_rate_limit(), Some(1024 * 1024));
    settings.set_string("speed-limit", "").unwrap();
    assert_eq!(live_rate_limit(), None);
    settings.set_string("speed-limit", "junk").unwrap();
    assert_eq!(live_rate_limit(), None);
    // Restore: the memory backend is shared across tests; a leaked
    // value would throttle or fail other tests' spawns.
    settings.set_string("speed-limit", "").unwrap();
}

#[test]
fn notification_toggles() {
    // NOTE: no pristine-defaults assert here: the memory GSettings
    // backend is process-shared, so other tests' set_boolean(false)
    // calls are visible. This checks live key -> method wiring instead.
    let settings = test_settings();
    settings.set_boolean("show-notifications", true).unwrap();
    settings.set_boolean("notify-background", true).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    assert!(manager.notifications_enabled());
    assert!(manager.background_notifications_enabled());
    settings.set_boolean("notify-background", false).unwrap();
    assert!(!manager.background_notifications_enabled());
    assert!(manager.notifications_enabled());
}

#[test]
fn transferring_ignores_paused() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("transferring");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert!(!manager.has_transferring());
    let paused = DownloadItem::new(1, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    paused.set_status(DownloadStatus::Paused);
    manager.store().append(&paused);
    assert!(manager.has_active());
    assert!(!manager.has_transferring());
    let queued = DownloadItem::new(2, "https://example.com/b.bin", "b.bin", "/tmp/dl");
    queued.set_status(DownloadStatus::Queued);
    manager.store().append(&queued);
    assert!(manager.has_transferring());
}

#[test]
fn active_count_covers_cancel_all_scope() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("active-count");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert_eq!(manager.active_count(), 0);
    for (id, status) in [
        (1, DownloadStatus::Queued),
        (2, DownloadStatus::Downloading),
        (3, DownloadStatus::Paused),
        (4, DownloadStatus::Done),
        (5, DownloadStatus::Failed),
        (6, DownloadStatus::Cancelled),
    ] {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        it.set_status(status);
        manager.store().append(&it);
    }
    assert_eq!(manager.active_count(), 3);
    assert!(manager.has_active());
}

#[test]
fn error_banner_ignores_cancelled() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("error-banner");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert!(!manager.has_failed());
    assert!(!manager.has_errored());
    for id in 1..=2 {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        manager.store().append(&it);
    }
    manager.cancel_all();
    assert!(manager.has_failed());
    assert!(!manager.has_errored());
    let failed = DownloadItem::new(3, "https://example.com/g.bin", "g.bin", "/tmp/dl");
    failed.set_status(DownloadStatus::Failed);
    manager.store().append(&failed);
    assert!(manager.has_errored());
}

#[test]
fn formats_amounts() {
    assert_eq!(format_amounts(0, 1024), "0 B of 1.0 KB");
    assert_eq!(
        format_amounts(5 * 1024 * 1024, 2 * 1024 * 1024 * 1024),
        "5.0 MB of 2.0 GB"
    );
}

#[test]
fn formats_bytes() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(999), "999 B");
    assert_eq!(fmt_bytes(1024), "1.0 KB");
    assert_eq!(fmt_bytes(1536), "1.5 KB");
    assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
    assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
}

#[test]
fn formats_eta() {
    assert_eq!(fmt_eta(0), "0s");
    assert_eq!(fmt_eta(45), "45s");
    assert_eq!(fmt_eta(125), "2m5s");
    assert_eq!(fmt_eta(3723), "1h2m");
}

#[test]
fn filenames() {
    assert_eq!(filename_from_url("https://example.com/a/b.iso"), "b.iso");
    assert_eq!(filename_from_url("https://example.com/"), "index.html");
    assert_eq!(filename_from_url("not a url"), "index.html");
    assert_eq!(
        filename_from_url("https://example.com/a/my%20game.zip"),
        "my game.zip"
    );
}

#[test]
fn urls() {
    assert!(normalize_url("https://example.com/f.iso").is_ok());
    assert!(normalize_url("ftp://example.com/f").is_err());
    assert!(normalize_url("file:///etc/passwd").is_err());
    assert!(normalize_url("--post-file=x").is_err());
    assert!(normalize_url("localhost:8080/f.iso").is_ok());
    // Userinfo would persist plaintext creds in queue.json: reject.
    assert!(normalize_url("https://user:pass@example.com/f.iso").is_err());
    assert!(normalize_url("https://user@example.com/f.iso").is_err());
    assert!(normalize_url("user:pass@example.com/f.iso").is_err());
}

#[test]
fn magnet_links() {
    let good = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=test";
    assert_eq!(normalize_url(good).as_deref(), Ok(good));
    assert_eq!(
        normalize_url("  MAGNET:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3  ")
            .as_deref(),
        Ok("MAGNET:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    // Tracker-heavy magnets (Ubuntu's run ~2-4 KB) bypass the 2048
    // HTTP cap: parsed locally, never sent as a request line.
    let big = format!(
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3{}",
        "&tr=udp://tracker.example.com:1337/announce".repeat(100)
    );
    assert!(big.len() > MAX_URL_LEN);
    assert_eq!(normalize_url(&big).as_deref(), Ok(big.as_str()));
    // Absurd magnets still rejected.
    let huge = format!(
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&x={}",
        "a".repeat(16384)
    );
    assert!(normalize_url(&huge).is_err());
    // Tracker params contain `://`: must never reach the http scheme
    // branch (regression: "Unsupported scheme: magnet").
    let tracked = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&tr=http://tracker.example.com:80/announce&tr=udp://tracker.example.com:1337/announce";
    assert_eq!(normalize_url(tracked).as_deref(), Ok(tracked));
    // Pasted BOM must not defeat the magnet classifier.
    let bom = format!("\u{feff}{good}");
    assert_eq!(normalize_url(&bom).as_deref(), Ok(good));
    // Magnet-shaped but unparseable: rejected by the parser, never by
    // the scheme branch.
    let bad_scheme = normalize_url("magnet://xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3");
    assert!(bad_scheme.is_err());
    assert!(!bad_scheme.unwrap_err().contains("Unsupported scheme"));
    // No BTv1 info-hash: unresolvable, reject at intake.
    assert!(normalize_url("magnet:?dn=nameless").is_err());
    assert!(normalize_url("magnet:?xt=urn:btih:xyz").is_err());
    assert!(normalize_url("magnet:").is_err());
}

/// Minimal single-file .torrent: info{length: 1, name: "foo"}.
fn single_torrent_bytes() -> Vec<u8> {
    format!(
        "d8:announce12:http://t.co/4:infod6:lengthi1e4:name3:foo12:piece lengthi16384e6:pieces20:{}ee",
        "A".repeat(20)
    )
    .into_bytes()
}

/// Minimal multi-file .torrent: bar/{a.txt: 2, sub/b.txt: 3}.
/// Info-dict keys must be sorted (files < name < piece length <
/// pieces): the parser enforces canonical order.
fn multi_torrent_bytes() -> Vec<u8> {
    format!(
        "d8:announce12:http://t.co/4:infod5:filesld6:lengthi2e4:pathl5:a.txteed6:lengthi3e4:pathl3:sub5:b.txteee4:name3:bar12:piece lengthi16384e6:pieces20:{}ee",
        "A".repeat(20)
    )
    .into_bytes()
}

#[test]
fn torrent_urls() {
    // Nothing archived under that path: rejected at intake.
    assert!(normalize_url("torrent:/nope/missing.torrent").is_err());
    assert!(!crate::torrent::is_torrent_url(
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"
    ));
    // A real archived file round-trips through normalize.
    let stem = format!("grab-test-{}", std::process::id());
    let pseudo =
        crate::torrent::archive_torrent_file(&format!("{stem}.torrent"), &single_torrent_bytes())
            .unwrap();
    assert!(crate::torrent::is_torrent_url(&pseudo));
    assert!(crate::torrent::is_torrent(&pseudo));
    assert_eq!(normalize_url(&pseudo).as_deref(), Ok(pseudo.as_str()));
    crate::torrent::delete_archive_for_url(&pseudo);
    assert!(crate::torrent::archive_path_for_url(&pseudo).is_none());
}

#[test]
fn torrent_file_list_parses() {
    let (name, entries) = crate::torrent::torrent_file_list(&single_torrent_bytes()).unwrap();
    assert_eq!(name, "foo");
    assert!(entries.is_empty());
    let (name, entries) = crate::torrent::torrent_file_list(&multi_torrent_bytes()).unwrap();
    assert_eq!(name, "bar");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "a.txt");
    assert_eq!(entries[0].length, 2);
    assert_eq!(entries[1].path, "sub/b.txt");
    assert_eq!(entries[1].length, 3);
    assert!(crate::torrent::torrent_file_list(b"not a torrent").is_err());
}

#[test]
fn sweep_keeps_only_referenced_archives() {
    let dir = std::env::temp_dir().join(format!("grab-sweep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keep = dir.join("keep.torrent");
    let drop = dir.join("drop.torrent");
    let skip = dir.join("notes.txt");
    std::fs::write(&keep, b"x").unwrap();
    std::fs::write(&drop, b"x").unwrap();
    std::fs::write(&skip, b"x").unwrap();
    let mut referenced = std::collections::HashSet::new();
    referenced.insert(format!("torrent:{}", keep.to_string_lossy()));
    crate::torrent::sweep_archives_in(&dir, &referenced);
    assert!(keep.exists());
    assert!(!drop.exists());
    assert!(skip.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn staged_selection_survives_respawn() {
    // Regression: take-once lost the file filter on every re-spawn
    // (retry after cancel/fail), silently downloading everything.
    // Selections now peek until pruned with unreferenced archives.
    let url = "torrent:/tmp/grab-test-sel.torrent";
    crate::torrent::stage_selection(url, vec![2], 3);
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    assert_eq!(crate::torrent::selection_total(url), Some(3));
    let mut referenced = std::collections::HashSet::new();
    referenced.insert(url.to_string());
    crate::torrent::prune_selections(&referenced);
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    crate::torrent::prune_selections(&std::collections::HashSet::new());
    assert_eq!(crate::torrent::get_selection(url), None);
}

#[test]
fn torrent_file_enqueue_uses_stem_stub() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-enqueue");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let stem = format!("grab-enqueue-{}", std::process::id());
    let item = manager
        .enqueue_torrent_file(
            single_torrent_bytes(),
            &format!("{stem}.torrent"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(item.status(), DownloadStatus::Queued);
    assert!(crate::torrent::is_torrent_url(&item.url()));
    assert!(sane_filename(&item.filename()));
    assert!(!item.filename().is_empty());
    // Leave no Queued row behind (a later restore could spawn it) and
    // no archive behind; restore the shared memory-backend key.
    manager.cancel_all();
    crate::torrent::delete_archive_for_url(&item.url());
    assert!(crate::torrent::archive_path_for_url(&item.url()).is_none());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn staged_selection_reaches_spawn_take() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-take");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let stem = format!("grab-take-{}", std::process::id());
    let item = manager
        .enqueue_torrent_file(
            single_torrent_bytes(),
            &format!("{stem}.torrent"),
            None,
            Some(vec![0]),
        )
        .unwrap();
    // The exact key spawn_torrent takes with: a staged selection must
    // survive the archive → normalize → store round-trip identically.
    assert_eq!(
        crate::torrent::get_selection(&item.url().to_string()),
        Some(vec![0])
    );
    // Leave no Queued row behind (a later restore could spawn it) and
    // no archive behind; restore the shared memory-backend key.
    manager.cancel_all();
    crate::torrent::delete_archive_for_url(&item.url());
    assert!(crate::torrent::archive_path_for_url(&item.url()).is_none());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn delete_download_trashes_torrent_files() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-del");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-torrent-del-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("gone.bin");
    std::fs::write(&file, b"bye").unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let item = DownloadItem::new(
        7,
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=gone",
        "gone.bin",
        &dir.to_string_lossy(),
    );
    item.set_status(DownloadStatus::Done);
    store.append(&item);

    // Explicit delete trashes the real files (not kept silently) and
    // drops the row; the session entry was never created offline.
    assert!(manager.delete_download(7).is_ok());
    assert!(!file.exists());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_file(trash.join("files/gone.bin"));
    let _ = std::fs::remove_file(trash.join("info/gone.bin.trashinfo"));

    // Multi-file torrents trash the whole subfolder.
    let sub = dir.join("Some Torrent");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("a.mp4"), b"data").unwrap();
    let item2 = DownloadItem::new(
        8,
        "magnet:?xt=urn:btih:b94a8fe5ccb19ba61c4c0873d391e987982fbbd4&dn=Some+Torrent",
        "Some Torrent",
        &dir.to_string_lossy(),
    );
    item2.set_status(DownloadStatus::Done);
    store.append(&item2);
    assert!(manager.delete_download(8).is_ok());
    assert!(!sub.exists());
    assert_eq!(store.n_items(), 0);
    let _ = std::fs::remove_dir_all(trash.join("files/Some Torrent"));
    let _ = std::fs::remove_file(trash.join("info/Some Torrent.trashinfo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_keeps_torrent_archive_for_undo() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("torrent-remove");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let stem = format!("grab-undo-{}", std::process::id());
    let item = manager
        .enqueue_torrent_file(
            single_torrent_bytes(),
            &format!("{stem}.torrent"),
            None,
            None,
        )
        .unwrap();
    let id = item.id();
    let url = item.url().to_string();
    // Remove drops the row but keeps the archive, so Undo can re-add
    // the same pseudo-URL and the engine resumes from kept partials.
    manager.remove(id);
    assert_eq!(manager.store().n_items(), 0);
    assert!(
        crate::torrent::archive_path_for_url(&url).is_some_and(|p| p.exists()),
        "remove must keep the archive for Undo"
    );
    // Teardown BEFORE restoring keys (see rejects_relative_download_dir).
    manager.cancel_all();
    crate::torrent::delete_archive_for_url(&url);
    assert!(crate::torrent::archive_path_for_url(&url).is_none());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn rejects_relative_download_dir() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("relative-dir");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    settings.set_string("download-dir", "relative/dir").unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    // Configured relative dir falls back to an absolute folder.
    let dir = manager.effective_download_dir();
    assert!(std::path::Path::new(&dir).is_absolute());
    // Explicit relative destinations fall back the same way.
    let item = manager
        .enqueue("https://example.com/f.iso", Some("relative/dir"), None)
        .unwrap();
    assert!(item.dest_dir() == dir);
    // Absolute destinations still pass through untouched.
    let item2 = manager
        .enqueue("https://example.com/g.iso", Some("/tmp"), None)
        .unwrap();
    assert!(item2.dest_dir() == "/tmp");
    // Teardown BEFORE restoring keys: the max-concurrent watch fires
    // start_next, and with no Queued row left it is a no-op. Restoring
    // first would free slots while rows are still queued and spawn real
    // engines whose pump futures outlive this test and abort later tests
    // on glib thread-affinity.
    manager.cancel_all();
    // Restore: the memory backend is shared across tests.
    settings.set_string("download-dir", "").unwrap();
    settings.set_int("max-concurrent", 3).unwrap();
    let _ = std::fs::remove_file(&_qf);
}

#[test]
fn normalizes_bare_hosts() {
    assert_eq!(
        normalize_url("example.com/f.iso").as_deref(),
        Ok("https://example.com/f.iso")
    );
    assert_eq!(
        normalize_url("  example.com  ").as_deref(),
        Ok("https://example.com/")
    );
    assert_eq!(
        normalize_url("http://example.com/f.iso").as_deref(),
        Ok("http://example.com/f.iso")
    );
    assert_eq!(
        normalize_url("localhost:8080/f.iso").as_deref(),
        Ok("https://localhost:8080/f.iso")
    );
    assert!(normalize_url("not a url").is_err());
    assert!(normalize_url("").is_err());
    assert_eq!(
        normalize_url("example .com").unwrap_err(),
        "Invalid URL: example .com"
    );
    let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
    assert_eq!(
        normalize_url(&long).unwrap_err(),
        format!("URL is too long (max {MAX_URL_LEN} characters)")
    );
    let maxed = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN - 20));
    assert!(normalize_url(&maxed).is_ok());
}

#[test]
fn delete_download_removes_file_and_row() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("del");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-del-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("gone.bin");
    std::fs::write(&file, b"bye").unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let item = DownloadItem::new(
        7,
        "https://example.com/gone.bin",
        "gone.bin",
        &dir.to_string_lossy(),
    );
    item.set_status(DownloadStatus::Done);
    store.append(&item);

    assert!(manager.delete_download(7).is_ok());
    assert!(!file.exists());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_file(trash.join("files/gone.bin"));
    let _ = std::fs::remove_file(trash.join("info/gone.bin.trashinfo"));
    let item2 = DownloadItem::new(
        8,
        "https://example.com/missing.bin",
        "missing.bin",
        &dir.to_string_lossy(),
    );
    item2.set_status(DownloadStatus::Done);
    store.append(&item2);
    assert!(manager.delete_download(8).is_ok());
    assert_eq!(store.n_items(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn magnet_delete_trashes_recorded_subfolder() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("magnet-subfolder");
    let settings = test_settings();
    // Occupy the only slot so nothing spawns a real engine below.
    settings.set_int("max-concurrent", 1).unwrap();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-magnet-sub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    settings
        .set_string("download-dir", &dir.to_string_lossy())
        .unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let item = manager
        .enqueue(
            "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=gone",
            None,
            None,
        )
        .unwrap();
    // Enqueue records the engine subfolder: no archive exists to
    // recompute it from at delete time.
    let folder = dir.join("gone");
    assert_eq!(
        std::path::PathBuf::from(item.output_dir().to_string()),
        folder
    );
    // Simulate engine output: the finished row is renamed while the
    // real payload sits inside the recorded subfolder.
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("real-name.bin"), b"data").unwrap();
    item.set_filename("real-name.bin");
    item.set_status(DownloadStatus::Done);
    assert!(manager.delete_download(item.id()).is_ok());
    assert!(!folder.exists());
    assert_eq!(manager.store().n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_dir_all(trash.join("files/gone"));
    let _ = std::fs::remove_file(trash.join("info/gone.trashinfo"));
    // Teardown BEFORE restoring keys (see rejects_relative_download_dir).
    manager.cancel_all();
    let _ = std::fs::remove_dir_all(&dir);
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn finish_cleans_untoggled_placeholders() {
    // Two-file archive ("a.txt", "sub/b.txt"), only the first kept.
    let stem = format!("grab-cleanup-{}", std::process::id());
    let pseudo =
        crate::torrent::archive_torrent_file(&format!("{stem}.torrent"), &multi_torrent_bytes())
            .unwrap();
    crate::torrent::stage_selection(&pseudo, vec![0], 2);
    // Fake engine output: kept file plus untoggled leftovers.
    let folder = std::env::temp_dir().join(format!("grab-cleanup-{stem}"));
    std::fs::create_dir_all(folder.join("sub")).unwrap();
    std::fs::write(folder.join("a.txt"), b"kept").unwrap();
    std::fs::write(folder.join("sub").join("b.txt"), b"").unwrap();
    crate::torrent::cleanup_unselected(&folder, &pseudo);
    assert!(folder.join("a.txt").exists());
    assert!(!folder.join("sub").exists());
    assert!(folder.exists());
    crate::torrent::delete_archive_for_url(&pseudo);
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn delete_download_trashes_torrent_subfolder() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("del-subfolder");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-delsub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.to_string_lossy().into_owned();
    // Multi-file torrent whose meta name ("bar") differs from the
    // archive stem: the engine folder is dest/<meta-name>/, never the
    // row's stub path. Delete must trash the folder, not no-op.
    let pseudo =
        crate::torrent::archive_torrent_file("mymeta.torrent", &multi_torrent_bytes()).unwrap();
    let folder = dir.join("bar");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.txt"), b"hi").unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let item = DownloadItem::new(9, &pseudo, "mymeta", &dest);
    item.set_status(DownloadStatus::Done);
    store.append(&item);

    assert!(manager.delete_download(9).is_ok());
    assert!(!folder.exists());
    assert!(crate::torrent::archive_path_for_url(&pseudo).is_none());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    let _ = std::fs::remove_dir_all(trash.join("files/bar"));
    let _ = std::fs::remove_file(trash.join("info/bar.trashinfo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dedupes() {
    let taken = |n: &str| matches!(n, "f.iso" | "f (1).iso");
    assert_eq!(dedupe_filename("g.iso", taken), "g.iso");
    assert_eq!(dedupe_filename("f.iso", taken), "f (2).iso");
    assert_eq!(dedupe_filename("README", taken), "README");
    let taken = |n: &str| n == "README";
    assert_eq!(dedupe_filename("README", taken), "README (1)");
    let taken = |_: &str| false;
    assert_eq!(dedupe_filename(".profile", taken), ".profile");
    assert_eq!(dedupe_filename("a.tar.gz", taken), "a.tar.gz");
}

#[test]
fn shortens_long_filenames() {
    assert_eq!(shorten_filename("short.mp4"), "short.mp4");
    let long = format!("{}.mp4", "a".repeat(300));
    let short = shorten_filename(&long);
    assert!(short.len() <= 240);
    assert!(short.ends_with(".mp4"));
    let wide = format!("{}.mp4", "é".repeat(200));
    let short = shorten_filename(&wide);
    assert!(short.len() <= 240);
    assert!(short.ends_with(".mp4"));
    let no_ext = "b".repeat(300);
    assert!(shorten_filename(&no_ext).len() <= 240);
}

#[test]
fn restore_keeps_exact_filename() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("restore-exact");
    test_settings();
    let dir = std::env::temp_dir().join(format!("grab-restore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_s = dir.to_string_lossy().into_owned();
    std::fs::write(dir.join("ubuntu.iso"), b"partial").unwrap();
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    let item = manager
        .restore_existing(
            "https://example.com/ubuntu.iso",
            &dir_s,
            "ubuntu.iso",
            StoredStatus::Downloading,
            None,
        )
        .unwrap();
    assert_eq!(item.filename(), "ubuntu.iso");
    assert_eq!(item.status(), DownloadStatus::Queued);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn queue_roundtrip_and_mapping() {
    assert_eq!(
        StoredStatus::from_item(DownloadStatus::Done),
        Some(StoredStatus::Done)
    );
    assert!(StoredStatus::from_item(DownloadStatus::Cancelled).is_none());
    for s in [
        DownloadStatus::Queued,
        DownloadStatus::Paused,
        DownloadStatus::Downloading,
        DownloadStatus::Failed,
    ] {
        assert!(StoredStatus::from_item(s).is_some());
    }
    let q = StoredQueue {
        version: QUEUE_VERSION,
        items: vec![
            StoredItem {
                url: "https://example.com/a.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "a.iso".to_string(),
                status: StoredStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
            },
            StoredItem {
                url: "https://example.com/b.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "b.iso".to_string(),
                status: StoredStatus::Done,
                progress: 1.0,
                segments: None,
                selected_files: None,
                output_dir: None,
            },
        ],
    };
    let text = serde_json::to_string(&q).unwrap();
    let back: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(back.version, QUEUE_VERSION);
    assert_eq!(back.items.len(), 2);
    assert_eq!(back.items[1].filename, "b.iso");
    let legacy = r#"{"version":1,"items":[{"url":"https://example.com/c.iso","dest_dir":"/tmp","filename":"c.iso","status":"done"}]}"#;
    let back: StoredQueue = serde_json::from_str(legacy).unwrap();
    assert!((back.items[0].progress - 0.0).abs() < f64::EPSILON);
}

#[test]
fn history_restore_roundtrip() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("history");
    let settings = test_settings();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let done = DownloadItem::new(1, "https://example.com/old.iso", "old.iso", "/tmp/dl");
    done.set_progress(1.0);
    done.set_status(DownloadStatus::Done);
    done.set_detail("Finished".to_string());
    m1.store().append(&done);
    m1.persist_queue();
    assert!(qf.exists());

    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m2.restore_queue();
    assert_eq!(m2.store().n_items(), 1);
    let it = m2.store().item(0).and_downcast::<DownloadItem>().unwrap();
    assert_eq!(it.filename(), "old.iso");
    assert_eq!(it.status(), DownloadStatus::Done);
    assert!((it.progress() - 1.0).abs() < f64::EPSILON);
    // Atomic persist leaves no tmp debris behind.
    assert!(!qf.with_extension("json.tmp").exists());
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn selection_survives_persist_restore() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("selection-roundtrip");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    // Occupy the only slot so nothing spawns a real engine below.
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    m1.running.borrow_mut().insert(99, holder);
    // Archive + stage a selection like the intake dialog does.
    let pseudo =
        crate::torrent::archive_torrent_file("keep.torrent", &single_torrent_bytes()).unwrap();
    crate::torrent::stage_selection(&pseudo, vec![0], 1);
    let item = m1.enqueue(&pseudo, Some("/tmp/dl"), Some("keep")).unwrap();
    assert_eq!(item.status(), DownloadStatus::Queued);
    m1.persist_queue();
    // The queue file carries the selection...
    let text = std::fs::read_to_string(&qf).unwrap();
    let queue: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].selected_files, Some(vec![0]));
    // ...and restore re-stages it. Prune first to simulate the
    // restart that wipes the in-memory map: without the re-stage,
    // the spawn would take None and download every file.
    crate::torrent::prune_selections(&std::collections::HashSet::new());
    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let holder2 = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    m2.running.borrow_mut().insert(99, holder2);
    m2.restore_queue();
    assert_eq!(m2.store().n_items(), 1);
    assert_eq!(crate::torrent::get_selection(&pseudo), Some(vec![0]));
    // Teardown: leave no Queued row behind (a later backend restore
    // would spawn a real engine for it) and restore shared keys.
    m1.cancel_all();
    m2.cancel_all();
    crate::torrent::delete_archive_for_url(&pseudo);
    settings.set_int("max-concurrent", 3).unwrap();
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn sane_filenames() {
    assert!(sane_filename("a.iso"));
    assert!(sane_filename("my file (1).tar.gz"));
    // Backslash is an ordinary (legal, harmless) char on Linux.
    assert!(sane_filename("a\\b"));
    assert!(!sane_filename(""));
    assert!(!sane_filename("."));
    assert!(!sane_filename(".."));
    assert!(!sane_filename("/etc/passwd"));
    assert!(!sane_filename("a/b"));
    assert!(!sane_filename("a\0b"));
    // Control and bidi-override characters deceive in listings and
    // notification text, and arrive via Content-Disposition decoding.
    assert!(!sane_filename("a\nb.mp4"));
    assert!(!sane_filename("a\tb.mp4"));
    assert!(!sane_filename("evil\u{202e}mp4.txt"));
}

#[test]
fn restore_rejects_bad_filenames() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("restore-bad");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    for bad in ["/etc/passwd", "..", ".", "a/b", ""] {
        assert!(manager
            .restore_existing(
                "https://example.com/f.iso",
                "/tmp/dl",
                bad,
                StoredStatus::Queued,
                None,
            )
            .is_err());
    }
    assert!(manager
        .restore_existing(
            "https://example.com/f.iso",
            "relative/dir",
            "f.iso",
            StoredStatus::Queued,
            None,
        )
        .is_err());
    assert_eq!(manager.store().n_items(), 0);
}

#[test]
fn batch_restore_hundred_done() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("batch");
    let settings = test_settings();
    let items: Vec<StoredItem> = (0..100)
        .map(|i| StoredItem {
            url: format!("https://example.com/f{i}.iso"),
            dest_dir: "/tmp/dl".to_string(),
            filename: format!("f{i}.iso"),
            status: StoredStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
        })
        .collect();
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items,
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 100);
    let it = m.store().item(99).and_downcast::<DownloadItem>().unwrap();
    assert_eq!(it.filename(), "f99.iso");
    assert_eq!(it.status(), DownloadStatus::Done);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn cancel_frees_slot_immediately() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("cancel-slot");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let a = DownloadItem::new(41, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    a.set_status(DownloadStatus::Downloading);
    manager.store().append(&a);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(41, holder);
    let b = DownloadItem::new(42, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
    manager.store().append(&b);
    manager.cancel(41);
    assert_eq!(a.status(), DownloadStatus::Cancelled);
    assert!(!manager.running.borrow().contains_key(&41));
    assert_eq!(b.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&42));
    // Drain B's UI future on THIS thread: its Failed message is already
    // queued (dead port fails fast). Leaving it pending would let another
    // test's MainLoop poll it on the wrong thread and abort on glib's
    // thread-affinity guard.
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&42) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!manager.running.borrow().contains_key(&42));
    assert_eq!(b.status(), DownloadStatus::Failed);
}

#[test]
fn pause_starts_next_queued() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("pause-next");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let a = DownloadItem::new(61, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    a.set_status(DownloadStatus::Downloading);
    manager.store().append(&a);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(61, holder);
    let b = DownloadItem::new(62, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
    manager.store().append(&b);
    manager.pause(61);
    assert_eq!(a.status(), DownloadStatus::Paused);
    assert!(!manager.running.borrow().contains_key(&61));
    assert_eq!(b.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&62));
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&62) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!manager.running.borrow().contains_key(&62));
}

#[test]
fn raising_max_concurrent_starts_queued() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("concurrent-bump");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let a = DownloadItem::new(63, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    a.set_status(DownloadStatus::Downloading);
    manager.store().append(&a);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(63, holder);
    let b = DownloadItem::new(64, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
    manager.store().append(&b);
    assert_eq!(b.status(), DownloadStatus::Queued);
    // The preferences SpinRow writes this key; the queued row must start
    // without any other queue event. Port 9 is closed so the spawned
    // engine fails fast during the drain below.
    settings.set_int("max-concurrent", 2).unwrap();
    assert_eq!(b.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&64));
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&64) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!manager.running.borrow().contains_key(&64));
    // Restore: the memory backend is shared across tests.
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn defer_yields_slot_and_goes_last() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("defer");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    // Both slots busy with stand-in holders: deferring must not spawn
    // any real engine, keeping this test fully synchronous (no main
    // loop pumping, which would race other tests' glib sources).
    for (id, progress) in [(71, 0.5), (72, 0.0)] {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        it.set_status(DownloadStatus::Downloading);
        it.set_progress(progress);
        manager.store().append(&it);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(id, holder);
    }
    manager.defer(71);
    // Progress kept, row parked behind the other one, slot count kept.
    let a = manager.find(71).unwrap();
    assert_eq!(a.status(), DownloadStatus::Queued);
    assert_eq!(a.progress(), 0.5);
    let order: Vec<u64> = (0..manager.store.n_items())
        .filter_map(|i| manager.store().item(i).and_downcast::<DownloadItem>())
        .map(|it| it.id())
        .collect();
    assert_eq!(order, vec![72, 71]);
    assert!(manager.running.borrow().contains_key(&72));
    assert!(!manager.running.borrow().contains_key(&71));
    // Teardown through the real API: cancelling everything first means
    // no row is queued, so the backend restore below can't start_next a
    // real engine whose UI future would outlive this test. The fake
    // holders are aborted by the cancel itself.
    manager.cancel_all();
    assert!(!manager.running.borrow().contains_key(&72));
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn lowering_max_concurrent_parks_newest() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("preempt");
    let settings = test_settings();
    settings.set_int("max-concurrent", 2).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    for id in [81, 82] {
        let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
        it.set_status(DownloadStatus::Downloading);
        manager.store().append(&it);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(id, holder);
    }
    let c = DownloadItem::new(83, "https://example.com/g.bin", "g.bin", "/tmp/dl");
    manager.store().append(&c);
    // The preferences SpinRow writes this key; the newest running row
    // must park itself without any other queue event.
    settings.set_int("max-concurrent", 1).unwrap();
    assert_eq!(
        manager.find(81).unwrap().status(),
        DownloadStatus::Downloading
    );
    assert_eq!(manager.find(82).unwrap().status(), DownloadStatus::Queued);
    assert!(manager.running.borrow().contains_key(&81));
    assert!(!manager.running.borrow().contains_key(&82));
    // Same teardown constraint as the defer test: leave no queued row
    // behind, or the backend restore spawns a real engine for it.
    manager.cancel_all();
    assert!(manager.running.borrow().is_empty());
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn restore_preserves_intent() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("intent");
    let settings = test_settings();
    let items: Vec<StoredItem> = [
        ("p.iso", StoredStatus::Paused),
        ("f.iso", StoredStatus::Failed),
        ("d.iso", StoredStatus::Done),
    ]
    .into_iter()
    .map(|(f, status)| StoredItem {
        url: format!("https://example.com/{f}"),
        dest_dir: "/tmp/dl".to_string(),
        filename: f.to_string(),
        status,
        progress: 0.5,
        segments: None,
        selected_files: None,
        output_dir: None,
    })
    .collect();
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items,
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m.restore_queue();
    assert_eq!(m.store().n_items(), 3);
    assert!(m.running.borrow().is_empty());
    let status_of = |name: &str| {
        (0..m.store().n_items())
            .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
            .find(|it| it.filename() == name)
            .map(|it| it.status())
    };
    assert_eq!(status_of("p.iso"), Some(DownloadStatus::Paused));
    assert_eq!(status_of("f.iso"), Some(DownloadStatus::Failed));
    assert_eq!(status_of("d.iso"), Some(DownloadStatus::Done));
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn restored_status_mapping() {
    assert_eq!(
        restored_status(StoredStatus::Paused),
        DownloadStatus::Paused
    );
    assert_eq!(
        restored_status(StoredStatus::Failed),
        DownloadStatus::Failed
    );
    assert_eq!(
        restored_status(StoredStatus::Queued),
        DownloadStatus::Queued
    );
    assert_eq!(
        restored_status(StoredStatus::Downloading),
        DownloadStatus::Queued
    );
}

#[test]
fn retry_persists_immediately() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("retry-persist");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let item = DownloadItem::new(51, "https://example.com/r.bin", "r.bin", "/tmp/dl");
    item.set_status(DownloadStatus::Cancelled);
    manager.store().append(&item);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(52, holder);
    manager.retry(51);
    assert_eq!(item.status(), DownloadStatus::Queued);
    assert_eq!(manager.running.borrow().len(), 1);
    let text = std::fs::read_to_string(&qf).unwrap();
    let queue: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].filename, "r.bin");
    assert_eq!(queue.items[0].status, StoredStatus::Queued);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn manager_pause_resume_cancel() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("lifecycle");
    let settings = test_settings();

    let dir = std::env::temp_dir().join(format!("grab-test-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(srv.join("t.bin"), &payload).unwrap();

    let port = test_port(0);
    let server_log = dir.join("server.log");
    let server_log_file = std::fs::File::create(&server_log).unwrap();
    let ranges_log = dir.join("ranges.log");
    let server_py = format!("{}/tests/throttled_server.py", env!("CARGO_MANIFEST_DIR"));
    let server = std::process::Command::new("python3")
        .arg(&server_py)
        .arg(port.to_string())
        .arg(srv.join("t.bin"))
        .arg(&ranges_log)
        .stdout(std::process::Stdio::null())
        .stderr(server_log_file)
        .spawn()
        .expect("python3 throttled server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");

    settings.set_boolean("show-notifications", false).unwrap();

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/t.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    let server = Rc::new(RefCell::new(server));
    glib::MainContext::default().spawn_local(async move {
        let server_kill = Rc::clone(&server);
        let path = dl.join("t.bin");
        let path_dbg = path.clone();
        let fail = move |msg: &str| -> ! {
            eprintln!("TEST FAILURE: {msg}");
            eprintln!(
                "TEST server log:\n{}",
                std::fs::read_to_string(&server_log).unwrap_or_default()
            );
            eprintln!(
                "TEST partial: {:?}",
                std::fs::metadata(&path_dbg).map(|m| m.len())
            );
            let _ = server_kill.borrow_mut().kill();
            std::process::exit(1);
        };
        let item = manager
            .enqueue(&url, Some(&dest), Some("t.bin"))
            .unwrap_or_else(|e| fail(&e));
        let id = item.id();

        glib::timeout_future(std::time::Duration::from_secs(1)).await;
        if item.status() != DownloadStatus::Downloading {
            fail("expected Downloading after 1s");
        }
        manager.pause(id);
        assert_eq!(item.status(), DownloadStatus::Paused);
        let size_a = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        glib::timeout_future(std::time::Duration::from_millis(600)).await;
        let size_b = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(size_a, size_b, "bytes moved while paused");

        manager.resume(id);
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 300 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            fail(&format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&path).unwrap() != payload {
            fail("bytes differ");
        }
        let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
        if !ranges.lines().any(|l| l.starts_with("bytes=")) {
            fail("resume never sent a Range request (206 path untested)");
        }

        let item2 = manager.enqueue(&url, Some(&dest), Some("t2.bin")).unwrap();
        manager.cancel(item2.id());
        assert_eq!(item2.status(), DownloadStatus::Cancelled);

        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 90);
}

#[test]
fn segmented_multi_connection_download() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("segmented");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    // 20 MB clears the 4 x 4 MB split threshold; unthrottled loopback
    // keeps the test to a few seconds.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("seg", "big.bin", 20_000_000, "0", &[], 13);

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let path = dl.join("big.bin");
        let item = manager
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        if !item.detail().starts_with("Finished \u{2022} ") {
            abort(
                &server,
                &format!("expected size in detail, got {:?}", item.detail()),
            );
        }
        // Distinct bounded ranges prove parallel segmented fetching
        // (a single stream would log one open-ended "bytes=0" line).
        let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
        let mut distinct = std::collections::HashSet::new();
        for line in ranges.lines().filter(|l| l.starts_with("bytes=")) {
            if line.contains('-') && !line.ends_with('-') && line.split('-').count() == 2 {
                distinct.insert(line.to_string());
            }
        }
        if distinct.len() < 2 {
            abort(
                &server,
                &format!("expected 2+ distinct ranges, log: {ranges:?}"),
            );
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn adopts_content_disposition_filename() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("disposition");
    let settings = test_settings();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "cd",
        "clip.mp4",
        300_000,
        "0.05",
        &["attachment; filename=\"movie.mp4\""],
        17,
    );

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    // Extensionless path, like CDN /videoplayback links.
    let url = format!("http://127.0.0.1:{port}/getfile");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let item = manager
            .enqueue(&url, Some(&dest), None)
            .unwrap_or_else(|e| abort(&server, &e));
        assert_eq!(item.filename(), "getfile");
        let mut waited = 0;
        while (item.filename() != "movie.mp4" || item.status() != DownloadStatus::Done)
            && waited < 600
        {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.filename() != "movie.mp4" {
            abort(
                &server,
                &format!("expected rename, got {:?}", item.filename()),
            );
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        let new_path = dl.join("movie.mp4");
        if std::fs::read(&new_path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        if dl.join("getfile").exists() {
            abort(&server, "stale file left behind");
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn prefers_shorter_content_disposition_filename() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("disposition-short");
    let settings = test_settings();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "cds",
        "short.zip",
        300_000,
        "0.05",
        &["attachment; filename=\"short.zip\""],
        41,
    );

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    // Long tracked/tokenized URL name with an extension, like file hosts emit.
    let long_name = format!("{}.zip", "a".repeat(120));
    let url = format!("http://127.0.0.1:{port}/{long_name}");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let item = manager
            .enqueue(&url, Some(&dest), None)
            .unwrap_or_else(|e| abort(&server, &e));
        assert_eq!(item.filename(), long_name);
        let mut waited = 0;
        while (item.filename() != "short.zip" || item.status() != DownloadStatus::Done)
            && waited < 600
        {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.filename() != "short.zip" {
            abort(
                &server,
                &format!("expected rename, got {:?}", item.filename()),
            );
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        let new_path = dl.join("short.zip");
        if std::fs::read(&new_path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn stale_pump_future_ignores_respawned_row() {
    // Deferring (or a quick pause-resume) aborts the engine and starts a
    // new one: the old pump future must not drag progress backwards,
    // fail the row, or steal the new engine's handle. The hanging server
    // keeps the second engine mid-transfer so any clobbering is visible.
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("stale-pump");
    let settings = test_settings();
    settings.set_int("max-concurrent", 1).unwrap();

    let Fixture {
        dir,
        dl,
        payload: _,
        port,
        server,
    } = spawn_fixture("hang", "h.bin", 20_000, "30", &[], 41);

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/h.bin");
    let dest = dl.to_string_lossy().into_owned();
    let item = manager
        .enqueue(&url, Some(&dest), Some("h.bin"))
        .unwrap_or_else(|e| abort(&server, &e));
    let id = item.id();
    assert_eq!(item.status(), DownloadStatus::Downloading);
    // Let the first engine get going, then defer: park aborts it and a
    // second engine takes over the same row.
    std::thread::sleep(std::time::Duration::from_millis(500));
    manager.defer(id);
    assert_eq!(item.status(), DownloadStatus::Downloading);
    // Let the aborted engine die and the hanging one settle, then pump:
    // the stale future's tail runs here and must stay silent.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let ctx = glib::MainContext::default();
    for _ in 0..20 {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if item.status() != DownloadStatus::Downloading {
        abort(
            &server,
            &format!(
                "stale pump future clobbered the respawned row: {:?}",
                item.detail(),
            ),
        );
    }
    if !manager.running.borrow().contains_key(&id) {
        abort(&server, "respawned engine lost its handle");
    }
    manager.cancel(id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if manager.running.borrow().contains_key(&id) {
        abort(&server, "cancelled engine never exited");
    }
    cleanup(&server, &dir);
    settings.set_int("max-concurrent", 3).unwrap();
}

#[test]
fn piece_rejects_changed_file_version() {
    // No locks: pure tokio + unique fixture, nothing shared.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("verc", "v.bin", 300_000, "0", &[], 43);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            timeout: 30,
            ..Default::default()
        },
        timeout: Duration::from_secs(30),
        tx,
    };
    let total = payload.len() as u64;
    // Bogus total: the server's Content-Range disagrees, so this must
    // fail Changed at once instead of burning retries.
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, 1)) {
        Err(AttemptFail::Changed(_)) => {}
        _ => abort(&server, "wrong-total piece must fail Changed"),
    }
    // Correct total: the piece comes back whole.
    match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, total)) {
        Ok(body) => assert_eq!(body.len(), 1024),
        _ => abort(&server, "correct-total piece must succeed"),
    }
    cleanup(&server, &dir);
}

#[test]
fn falls_back_to_single_stream_when_throttled() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("throttle");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    // Big enough to split; the server 403s every range except the probe,
    // so the engine must downgrade to one stream and still finish intact.
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture(
        "thr",
        "big.bin",
        20_000_000,
        "0",
        &["", "throttle-ranges"],
        19,
    );

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let path = dl.join("big.bin");
        let item = manager
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&path).unwrap() != payload {
            abort(&server, "bytes differ");
        }
        // A full (unranged) request proves the single-stream fallback ran:
        // pure multi would only ever log bounded ranges.
        let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
        if !ranges.lines().any(|l| l == "full") {
            abort(
                &server,
                &format!("expected single-stream fallback, log: {ranges:?}"),
            );
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn segmented_pause_resume_keeps_bytes() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("seg-pause");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    // 20 MB clears the split threshold. Slightly throttled (~3 MB/s,
    // ~6 s total): the transfer must stay well above the 100 ms progress
    // granularity or no poll can land mid-transfer (unthrottled loopback
    // finishes in ~100 ms and polls only ever see 0% then Done).
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("segp", "big.bin", 20_000_000, "0.002", &[], 23);

    settings.set_boolean("show-notifications", false).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let path = dl.join("big.bin");
        let item = manager
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        // Wait until at least one 1 MB piece (5%) landed, then pause.
        // The status check is part of the loop condition (same thread runs
        // to pause() with no await in between, so it cannot slip to Done).
        let mut waited = 0;
        while item.status() == DownloadStatus::Downloading
            && item.progress() < 0.05
            && waited < 2000
        {
            glib::timeout_future(std::time::Duration::from_millis(5)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Downloading {
            abort(&server, "finished before pause could land mid-transfer");
        }
        manager.pause(id);
        if item.status() != DownloadStatus::Paused {
            abort(
                &server,
                &format!("expected Paused, got {:?}", item.status()),
            );
        }
        // Resume must reuse the bitmap (partially done, not all).
        let mid: bool = manager
            .segment_state
            .borrow()
            .get(&id)
            .map(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b))
            .unwrap_or(false);
        if !mid {
            abort(&server, "resume bitmap is not mid-transfer");
        }
        manager.resume(id);
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        // The regression: resume used to truncate completed pieces to
        // zero while the bitmap still claimed them as done.
        if std::fs::read(&path).unwrap() != payload {
            abort(&server, "bytes differ after pause/resume");
        }
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn bitmap_persists_across_managers() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("segpersist");
    let settings = test_settings();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let item = DownloadItem::new(1, "https://example.com/big.bin", "big.bin", "/tmp/dl");
    item.set_status(DownloadStatus::Paused);
    m1.store().append(&item);
    let mut st = SegmentState::new(4 * PIECE_MIN);
    st.mark(0);
    st.mark(2);
    m1.segment_state.borrow_mut().insert(1, st);
    m1.persist_queue();

    // v1 readers ignore the unknown field; v2 keeps it.
    let text = std::fs::read_to_string(&qf).unwrap();
    assert!(text.contains("\"segments\""));
    let legacy = r#"{"version":1,"items":[{"url":"https://example.com/a.iso","dest_dir":"/tmp/dl","filename":"a.iso","status":"queued","progress":0.0}]}"#;
    let back: StoredQueue = serde_json::from_str(legacy).unwrap();
    assert!(back.items[0].segments.is_none());

    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m2.restore_queue();
    let restored = m2.segment_state.borrow();
    let st = restored.get(&1).expect("bitmap restored");
    assert_eq!(st.total, 4 * PIECE_MIN);
    assert_eq!(
        st.missing(),
        vec![
            (1, PIECE_MIN, 2 * PIECE_MIN - 1),
            (3, 3 * PIECE_MIN, 4 * PIECE_MIN - 1),
        ]
    );
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn killed_segmented_resume_starts_over() {
    // Simulates SIGKILL mid-segmented-download: a sparse full-size file
    // with holes plus a stale Queued entry, no resume bitmap (RAM died
    // with the process). Restart must discard and re-fetch, never mark
    // the holey file Done via the 416 shortcut.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("kill");
    let settings = test_settings();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("kill", "big.bin", 20_000_000, "0", &[], 29);

    // Sparse corpse: full size, only the first megabyte real.
    let dest = dl.join("big.bin");
    {
        let f = std::fs::File::create(&dest).unwrap();
        f.set_len(20_000_000).unwrap();
        use std::io::Write;
        let mut f = f;
        f.write_all(&payload[..1_000_000]).unwrap();
    }
    assert!(has_holes(&dest));
    std::fs::write(
        &qf,
        serde_json::to_string(&StoredQueue {
            version: QUEUE_VERSION,
            items: vec![StoredItem {
                url: format!("http://127.0.0.1:{port}/big.bin"),
                dest_dir: dl.to_string_lossy().into_owned(),
                filename: "big.bin".to_string(),
                status: StoredStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
            }],
        })
        .unwrap(),
    )
    .unwrap();

    settings.set_boolean("show-notifications", false).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        manager.restore_queue();
        if manager.store().n_items() != 1 {
            abort(&server, "queue did not restore");
        }
        let item = manager
            .store()
            .item(0)
            .and_downcast::<DownloadItem>()
            .unwrap();
        let mut waited = 0;
        while item.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item.status()));
        }
        if std::fs::read(&dest).unwrap() != payload {
            abort(&server, "holey file was marked Done instead of re-fetched");
        }
        cleanup(&server, &dir);
        let _ = std::fs::remove_file(&qf);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn shutdown_restart_resumes_segmented() {
    // Full kill-restart cycle in-process: manager1 downloads segmented,
    // shuts down mid-transfer (abort + truncate + persist WITH bitmap),
    // then a fresh manager2 on the same queue file resumes segmented.
    let (_lock, _loop) = test_locks();
    let qf = test_queue_file("segrestart");
    let settings = test_settings();
    settings.set_int("connections", 4).unwrap();

    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("segr", "big.bin", 20_000_000, "0.002", &[], 31);

    settings.set_boolean("show-notifications", false).unwrap();
    let manager1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let dest = dl.to_string_lossy().into_owned();
        let item = manager1
            .enqueue(&url, Some(&dest), Some("big.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        let mut waited = 0;
        while item.status() == DownloadStatus::Downloading
            && item.progress() < 0.05
            && waited < 2000
        {
            glib::timeout_future(std::time::Duration::from_millis(5)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Downloading {
            abort(&server, "finished before shutdown could land mid-transfer");
        }
        manager1.shutdown();
        // Bitmap survived shutdown, mid-transfer, and was persisted.
        let mid1 = manager1
            .segment_state
            .borrow()
            .get(&id)
            .map(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b))
            .unwrap_or(false);
        if !mid1 {
            abort(&server, "no mid-transfer bitmap at shutdown");
        }
        let text = std::fs::read_to_string(&qf).unwrap();
        if !text.contains("\"segments\"") {
            abort(&server, "bitmap was not persisted");
        }
        // Fresh manager, same queue file: must pick up the bitmap and
        // resume segmented, not restart.
        let manager2 =
            DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        manager2.restore_queue();
        if manager2.store().n_items() != 1 {
            abort(&server, "queue did not restore");
        }
        let mid2 = manager2
            .segment_state
            .borrow()
            .values()
            .any(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b));
        if !mid2 {
            abort(&server, "restored manager has no segmented resume state");
        }
        let item2 = manager2
            .store()
            .item(0)
            .and_downcast::<DownloadItem>()
            .unwrap();
        let mut waited = 0;
        while item2.status() != DownloadStatus::Done && waited < 600 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item2.status() != DownloadStatus::Done {
            abort(&server, &format!("expected Done, got {:?}", item2.status()));
        }
        if std::fs::read(dl.join("big.bin")).unwrap() != payload {
            abort(&server, "bytes differ after kill-restart-resume");
        }
        cleanup(&server, &dir);
        let _ = std::fs::remove_file(&qf);
        quit.quit();
    });
    run_loop(&main_loop, 120);
}

#[test]
fn aborted_engine_marks_failed() {
    // Simulates an engine task dying without reporting (panic): aborting
    // its handle must fail the row instead of stranding it Downloading.
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("abortwatch");
    let settings = test_settings();

    let dir = std::env::temp_dir().join(format!("grab-abw-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(srv.join("t.bin"), &payload).unwrap();

    let port = test_port(37);
    let ranges_log = dir.join("ranges.log");
    let server = std::process::Command::new("python3")
        .arg(format!(
            "{}/tests/throttled_server.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(port.to_string())
        .arg(srv.join("t.bin"))
        .arg(&ranges_log)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 range server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");

    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings);
    let url = format!("http://127.0.0.1:{port}/t.bin");
    let dest = dl.to_string_lossy().into_owned();

    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    let server = Rc::new(RefCell::new(server));
    glib::MainContext::default().spawn_local(async move {
        let item = manager
            .enqueue(&url, Some(&dest), Some("t.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        let mut waited = 0;
        while item.status() != DownloadStatus::Downloading && waited < 200 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Downloading {
            abort(&server, "never started downloading");
        }
        // Kill the engine task without touching row state, as a panic would.
        if let Some(handle) = manager.running.borrow().get(&id) {
            handle.abort();
        }
        let mut waited = 0;
        while item.status() != DownloadStatus::Failed && waited < 100 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Failed {
            abort(
                &server,
                &format!("dead engine left row {:?}", item.status()),
            );
        }
        assert_eq!(item.detail(), "Download interrupted");
        cleanup(&server, &dir);
        quit.quit();
    });
    run_loop(&main_loop, 60);
}

#[test]
fn failed_download_reports_cause() {
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("http-error");
    let settings = test_settings();
    let port = test_port(7);
    let server = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 http.server");
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let server = Rc::new(RefCell::new(server));
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings);
        let item = manager
            .enqueue(
                &format!("http://127.0.0.1:{port}/nope.bin"),
                None,
                Some("nope.bin"),
            )
            .unwrap_or_else(|e| abort(&server, &e));
        let mut waited = 0;
        while item.status() != DownloadStatus::Failed && waited < 200 {
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        if item.status() != DownloadStatus::Failed {
            abort(
                &server,
                &format!("expected Failed, got {:?}", item.status()),
            );
        }
        if !item.detail().contains("404") {
            abort(
                &server,
                &format!("expected 404 hint, got {:?}", item.detail()),
            );
        }
        let _ = server.borrow_mut().kill();
        quit.quit();
    });
    run_loop(&main_loop, 60);
}

#[test]
fn restart_with_smaller_file_keeps_partial() {
    // A resume the server answers from zero with a SMALLER object than
    // we hold is a different file (login wall, throttle page): fail
    // loudly and keep the partial bytes instead of truncating them.
    // Plain http.server ignores Range, which is exactly the shape.
    let (_lock, _loop) = test_locks();
    let _qf = test_queue_file("shrink-guard");
    let settings = test_settings();
    let dir = std::env::temp_dir().join(format!("grab-shrink-{}", std::process::id()));
    let srv = dir.join("srv");
    let dl = dir.join("dl");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    std::fs::write(srv.join("t.bin"), vec![7u8; 10 * 1024]).unwrap();
    std::fs::write(dl.join("t.bin"), vec![0u8; 1024 * 1024]).unwrap();

    let port = test_port(53);
    let server = std::process::Command::new("python3")
        .args([
            "-m",
            "http.server",
            &port.to_string(),
            "--directory",
            &srv.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 http.server");
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "test HTTP server did not listen on port {port}");

    let server = Rc::new(RefCell::new(server));
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store, settings.clone());
    let url = format!("http://127.0.0.1:{port}/t.bin");
    let dest = dl.to_string_lossy().into_owned();
    // restore_existing, not enqueue: enqueue would dedupe away from the
    // pre-written partial, and the restore path is synchronous, so no
    // race with the engine's first metadata read.
    let item = manager
        .restore_existing(&url, &dest, "t.bin", StoredStatus::Downloading, None)
        .unwrap_or_else(|e| abort(&server, &e));
    let id = item.id();
    // Drain the engine's pump future on this thread (see MAIN_LOOP_LOCK).
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if item.status() != DownloadStatus::Failed {
        abort(
            &server,
            &format!("expected loud failure, got {:?}", item.status()),
        );
    }
    if !item.detail().contains("smaller file") {
        abort(&server, &format!("wrong cause: {:?}", item.detail()));
    }
    if std::fs::metadata(dl.join("t.bin"))
        .map(|m| m.len())
        .unwrap_or(0)
        != 1024 * 1024
    {
        abort(&server, "partial file was truncated");
    }
    cleanup(&server, &dir);
}
