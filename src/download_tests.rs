use super::*;
use crate::download_fetch::{
    AttemptFail, FetchCtx, StartMode, fetch_piece, filename_from_content_disposition, has_holes,
    parse_content_range, rejects_unexpected_restart, response_total, run_download, stamp_request,
    truncate_to_prefix,
};
use crate::download_intake::{MAX_URL_LEN, normalize_url};
use crate::download_net::{
    DownloadOptions, PROXY_MODE_DIRECT, PROXY_MODE_SYSTEM, http_client, normalize_no_proxy,
    proxied_pool_len, proxy_mode_index, proxy_mode_labels, proxy_mode_value, proxy_type_index,
    proxy_type_labels, proxy_type_value,
};
use crate::download_pieces::{BLOCK_CELLS, SegmentState, aggregate, plan_pieces, split_count};
use crate::download_rate::{fmt_eta, format_amounts, live_rate_limit, parse_rate};
use crate::download_row::DownloadItem;
use crate::download_store::{QUEUE_VERSION, StoredItem, StoredQueue};
use crate::file_names::{
    PIECE_MAX, PIECE_MIN, dedupe_filename, filename_from_url, piece_len, rename_noreplace,
    shorten_filename,
};

use crate::video::test_support::NoVideoTools;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// A minimal persisted row for `restore_existing` fixtures: no persisted
/// id (so restore allocates, which is the pre-v3 path), no resume state,
/// and no video source.
fn stored_row(url: &str, dest_dir: &str, filename: &str, status: DownloadStatus) -> StoredItem {
    StoredItem {
        id: None,
        url: url.to_string(),
        dest_dir: dest_dir.to_string(),
        filename: filename.to_string(),
        status,
        progress: 0.0,
        segments: None,
        selected_files: None,
        output_dir: None,
        video_source: None,
    }
}

fn test_queue_file(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "grab-q-{:?}-{tag}.json",
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    // SAFETY: test setup runs before any test thread spawns.
    unsafe { std::env::set_var("GRAB_QUEUE_FILE", &p) };
    p
}

/// Spin the default MainContext until the row's engine slot frees (or the
/// deadline passes), then drain to quiescence like [`run_loop`]: the pump
/// tail runs after the slot frees, and a woken-but-unpolled tail left
/// behind would abort a later test on the thread guard. The caller must
/// hold MAIN_LOOP_LOCK (via test_locks).
fn drain_engine(manager: &DownloadManager, id: u64) {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    quiesce(&ctx);
}

/// Pump until the context goes quiet (50 idle rounds), so no woken tail is
/// left for another test's loop to trip over. Shared tail for drains that
/// don't go through [`run_loop`].
fn quiesce(ctx: &glib::MainContext) {
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

fn test_settings() -> crate::settings::AppSettings {
    // SAFETY: single-threaded setup phase (cargo runs with --test-threads=1).
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }
    crate::settings::AppSettings::new()
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
    // CI runners sometimes fail to bring the fixture server up on the
    // first port (slow spawn, stale listener). Retry on fresh ports
    // instead of failing the test: a panic here poisons the shared test
    // locks and cascades into every later test.
    let mut last_port = 0;
    for _ in 0..3 {
        let port = test_port(port_offset);
        last_port = port;
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
        let server = Rc::new(RefCell::new(
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("python3 range server"),
        ));
        let mut ready = false;
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if ready {
            return Fixture {
                dir,
                dl,
                payload,
                port,
                server,
            };
        }
        let _ = server.borrow_mut().kill();
    }
    panic!("test HTTP server did not listen on port {last_port}");
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
fn rename_noreplace_spans_filesystems() {
    // Staging lives on tmpfs while downloads sit on disk: the move must
    // survive EXDEV. /dev/shm is a separate tmpfs on Linux, so moving
    // out of it exercises the copy fallback; elsewhere it exercises the
    // rename path — the contract (moved, source gone, never clobbers)
    // holds on both.
    let shm = std::path::Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("grab-xdev-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = shm.join(format!("grab-xdev-src-{}", std::process::id()));
    let busy = dir.join("busy.bin");
    let free = dir.join("free.bin");
    std::fs::write(&src, b"cross-device bytes").unwrap();
    std::fs::write(&busy, b"victim").unwrap();
    // Occupied destination: error, victim and source untouched.
    assert!(rename_noreplace(&src, &busy).is_err());
    assert_eq!(std::fs::read(&busy).unwrap(), b"victim");
    // Free destination: content moved, source gone.
    assert!(rename_noreplace(&src, &free).is_ok());
    assert!(!src.exists());
    assert_eq!(std::fs::read(&free).unwrap(), b"cross-device bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overcap_queue_keeps_active_first() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("overcap");
    let settings = test_settings();
    let mut items = vec![
        StoredItem {
            id: None,
            url: "https://example.com/active.iso".to_string(),
            dest_dir: "/tmp/dl".to_string(),
            filename: "active.iso".to_string(),
            status: DownloadStatus::Queued,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
        },
        StoredItem {
            id: None,
            url: "https://example.com/paused.iso".to_string(),
            dest_dir: "/tmp/dl".to_string(),
            filename: "paused.iso".to_string(),
            status: DownloadStatus::Paused,
            progress: 0.5,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
        },
    ];
    for i in 0..1000 {
        items.push(StoredItem {
            id: None,
            url: format!("https://example.com/f{i}.iso"),
            dest_dir: "/tmp/dl".to_string(),
            filename: format!("f{i}.iso"),
            status: DownloadStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
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
fn enqueue_video_spawns_and_fails_without_tools() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir()
        .join("grab-video-enqueue")
        .to_string_lossy()
        .into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest),
            Some("My Video.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    let id = item.id();
    // The worker runs and fails fast on the missing tools (no network).
    drain_engine(&manager, id);
    assert_eq!(item.status(), DownloadStatus::Failed);
    assert_eq!(
        item.detail(),
        "Media downloads need the yt-dlp support tools"
    );
    // The Page marker survives the failure, so Retry replays the pipeline.
    assert!(matches!(
        manager.video_source(id),
        Some(crate::media_types::VideoSource::Page { .. })
    ));
    // Engine slot and abort sender are both released.
    assert!(!manager.running.borrow().contains_key(&id));
    assert!(!manager.video_abort.borrow().contains_key(&id));
    crate::video::clean_staging(&crate::video::staging_dir(id));
}

#[test]
fn enqueue_video_restrict_filenames_folds_name() {
    // The opt-in applies where Grab names the file (enqueue), not via a
    // yt-dlp flag: yt-dlp only sanitizes its own template fields, while
    // Grab passes literal output paths.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-restrict");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    settings.set_boolean("restrict-filenames", true).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join("grab-video-restrict");
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest),
            Some("Café & Croissants.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "Cafe_Croissants.mp4");
    let id = item.id();
    drain_engine(&manager, id);
    crate::video::clean_staging(&crate::video::staging_dir(id));
}

#[test]
fn a_restored_row_keeps_its_id_so_its_staging_stays_reachable() {
    // The id IS the staging key: `staging_dir(item_id)`. Restore used to
    // call `alloc_id()`, handing every restored row a *fresh* number. A
    // retry after a restart therefore scanned a different directory than
    // the attempt that left an unplaceable recording in it -- and a later
    // row handed the same number could `clean_staging` it away.
    //
    // The gap is what makes this observable. Restoring a contiguous queue
    // re-allocates the same numbers in the same order, so the obvious
    // version of this test passes against the broken code -- which is why
    // the single-row restore test beside this one never caught it. Remove
    // the middle row first and the survivors shift down by one.
    //
    // Rows are created through `restore_existing` in a Paused state, which
    // spawns nothing: no tool scrubbing, no network, and nothing that
    // needs the process-global environment other tests also depend on.
    let (_q, _l) = test_locks();
    let qf = test_queue_file("restore-id");
    let dest = std::env::temp_dir().join("grab-restore-id");
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();

    let add_paused = |manager: &Rc<DownloadManager>, url: &str, name: &str| {
        manager
            .restore_existing(&stored_row(url, &dest, name, DownloadStatus::Paused))
            .expect("row created")
            .id()
    };

    let (first, middle, last) = {
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let first = add_paused(&manager, "https://example.com/a.bin", "a.bin");
        let middle = add_paused(&manager, "https://example.com/b.bin", "b.bin");
        let last = add_paused(&manager, "https://example.com/c.bin", "c.bin");
        // Remove the middle row so the survivors are no longer contiguous.
        manager.remove(middle);
        (first, middle, last)
    };
    assert!(
        last > middle && middle > first,
        "the fixture needs three increasing ids ({first}, {middle}, {last})"
    );

    // A fresh manager over the same queue file.
    let settings = test_settings();
    let manager2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    manager2.restore_queue();
    for (id, url) in [
        (first, "https://example.com/a.bin"),
        (last, "https://example.com/c.bin"),
    ] {
        let row = (0..manager2.store().n_items())
            .filter_map(|i| manager2.store().item(i).and_downcast::<DownloadItem>())
            .find(|it| it.url() == url)
            .unwrap_or_else(|| panic!("row for {url} vanished across the restart"));
        assert_eq!(
            row.id(),
            id,
            "the row came back as {} so its staging dir \
             (<temp>/grab-video/{id}) is no longer the one its attempt wrote to",
            row.id()
        );
    }

    // A row created after the restore must not be handed a number a
    // restored row still owns, or the two would share a staging directory.
    let fresh = add_paused(&manager2, "https://example.com/d.bin", "d.bin");
    assert!(
        fresh != first && fresh != last,
        "a new row was handed id {fresh}, which a restored row still owns, so \
         both would share one staging directory"
    );

    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn a_pre_upgrade_row_never_lands_on_a_leftover_staging_dir() {
    // A queue written before ids were persisted restores its rows with
    // fresh ids. If the allocator then hands out a number that some other
    // row's leftover staging directory still occupies, that row's
    // `clean_staging` deletes a recording it never made -- the exact data
    // loss #178 exists to prevent, just arriving via the upgrade.
    //
    // The leftovers are left alone on disk, deliberately: an unreachable
    // recording is recoverable by hand, a deleted one is not.
    let (_q, _l) = test_locks();
    let qf = test_queue_file("upgrade-guard");
    let dest = std::env::temp_dir().join("grab-upgrade-guard");
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();

    // A leftover from "a previous session", holding something precious.
    let leftover = crate::video::staging_dir(9_000);
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("final.1.mp4"), b"someone's recording").unwrap();

    // A pre-v3 queue: no persisted id, so restore allocates.
    std::fs::write(
        &qf,
        serde_json::to_string_pretty(&StoredQueue {
            version: 2,
            items: vec![StoredItem {
                id: None,
                url: "https://example.com/legacy.bin".to_string(),
                dest_dir: dest.clone(),
                filename: "legacy.bin".to_string(),
                status: DownloadStatus::Paused,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
            }],
        })
        .unwrap(),
    )
    .unwrap();

    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    manager.restore_queue();
    let item = (0..manager.store().n_items())
        .filter_map(|i| manager.store().item(i).and_downcast::<DownloadItem>())
        .find(|it| it.url() == "https://example.com/legacy.bin")
        .expect("legacy row restored");
    assert!(
        item.id() > 9_000,
        "the restored row took id {} which a leftover staging dir still owns",
        item.id()
    );
    assert_eq!(
        std::fs::read(leftover.join("final.1.mp4")).unwrap(),
        b"someone's recording",
        "the leftover was destroyed instead of merely left unreachable"
    );
    let _ = std::fs::remove_dir_all(&leftover);
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn video_source_survives_restore_and_retry() {
    let (_q, _l) = test_locks();
    let qf = test_queue_file("video-restore");
    let _notools = NoVideoTools::apply();
    let dest = std::env::temp_dir().join("grab-video-restore");
    std::fs::create_dir_all(&dest).unwrap();
    let dest = dest.to_string_lossy().into_owned();
    let id = {
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let item = manager
            .enqueue_video(
                "https://vimeo.com/123456",
                Some(&dest),
                Some("Clip.mp4"),
                crate::media_types::VideoChoices {
                    quality: "720p".to_string(),
                    audio_only: false,
                    video_format_id: None,
                    is_live: false,
                    playlist_item_id: None,
                },
            )
            .expect("video enqueue");
        // Persisted with the Page marker (not silently dropped).
        let text = std::fs::read_to_string(&qf).unwrap();
        assert!(text.contains("\"video_source\""));
        assert!(text.contains("vimeo.com/123456"));
        let id = item.id();
        drain_engine(&manager, id);
        id
    };
    // Fresh manager over the same queue file re-stages the source and
    // replays the worker (which fails fast here, proving the dispatch).
    let settings = test_settings();
    let manager2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    manager2.restore_queue();
    let restored = manager2.find(id).expect("restored video row");
    drain_engine(&manager2, id);
    assert_eq!(restored.status(), DownloadStatus::Failed);
    let stored = manager2.video_source(id).expect("re-staged source");
    assert!(
        matches!(stored, crate::media_types::VideoSource::Page { ref quality, .. } if quality == "720p")
    );
    crate::video::clean_staging(&crate::video::staging_dir(id));
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn mismatched_video_source_dropped_on_restore() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("video-mismatch");
    // Hand-edited queue: the Page marker names another video.
    let queue = StoredQueue {
        version: QUEUE_VERSION,
        items: vec![StoredItem {
            id: None,
            url: "https://example.com/f.iso".into(),
            dest_dir: "/tmp/dl".into(),
            filename: "f.iso".into(),
            status: DownloadStatus::Paused,
            progress: 0.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: Some(crate::media_types::VideoSource::Page {
                page_url: "https://vimeo.com/OTHER".into(),
                media_url: None,
                expires_at: None,
                quality: "1080p".into(),
                audio_only: false,
                is_live: false,
                video_format_id: None,
                playlist_item_id: None,
            }),
        }],
    };
    std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    // Paused stays parked: no spawn, no network.
    manager.restore_queue();
    assert_eq!(manager.store().n_items(), 1);
    let item = manager
        .store()
        .item(0)
        .and_downcast::<DownloadItem>()
        .unwrap();
    assert_eq!(manager.video_source(item.id()), None);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn unremove_restores_video_source() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("video-unremove");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir()
        .join("grab-video-unremove")
        .to_string_lossy()
        .into_owned();
    let item = manager
        .enqueue_video(
            "https://vimeo.com/123456",
            Some(&dest),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "720p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    let id = item.id();
    drain_engine(&manager, id);
    assert_eq!(item.status(), DownloadStatus::Failed);
    let snap = RemovedSnapshot {
        url: item.url().to_string(),
        dest_dir: item.dest_dir().to_string(),
        filename: item.filename().to_string(),
        status: item.status(),
        progress: item.progress(),
        detail: item.detail().to_string(),
        output_dir: item.output_dir().to_string(),
        segments: None,
        video_source: manager.video_source(id),
    };
    manager.remove(id);
    assert_eq!(manager.video_source(id), None);
    let revived = manager.unremove(snap);
    let new_id = revived.id();
    // Failed requeues and replays the worker (fast tools failure here).
    drain_engine(&manager, new_id);
    assert_eq!(revived.status(), DownloadStatus::Failed);
    assert!(matches!(
        manager.video_source(new_id),
        Some(crate::media_types::VideoSource::Page { .. })
    ));
    crate::video::clean_staging(&crate::video::staging_dir(id));
    crate::video::clean_staging(&crate::video::staging_dir(new_id));
}

#[test]
fn queue_file_never_carries_cookies() {
    // Secrets must not reach the persisted queue, whatever the settings.
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _notools = NoVideoTools::apply();
    let qf = test_queue_file("video-cookies-persist");
    let settings = test_settings();
    settings.set_string("cookies-browser", "firefox").unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let item = manager
        .enqueue_video(
            "https://vimeo.com/123456",
            Some("/tmp/dl"),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    // Drain the spawned worker (fails fast without tools) so no woken pump
    // tail is left for another test's loop to trip over (glib thread guard).
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_dir(item.id()));
    let text = std::fs::read_to_string(&qf).unwrap();
    assert!(
        !text.contains("cookies"),
        "persisted queue must not mention cookies: {text}"
    );
    let _ = std::fs::remove_file(&qf);
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
    assert_eq!(fmt_eta(0), "0 seconds");
    assert_eq!(fmt_eta(1), "1 second");
    assert_eq!(fmt_eta(45), "45 seconds");
    assert_eq!(fmt_eta(125), "2 minutes");
    assert_eq!(fmt_eta(3723), "1 hour");
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
    crate::torrent::stage_selection(url, vec![2]);
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
    assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
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
    let dbg = manager.delete_download(7);
    eprintln!(
        "DEBUG delete={:?} user_data={:?} exists={}",
        dbg.as_ref().err(),
        glib::user_data_dir(),
        dir.display()
    );
    assert!(dbg.is_ok());
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
    crate::torrent::stage_selection(&pseudo, vec![0]);
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
fn restrict_filename_ascii_folds_to_ascii() {
    // Accented Latin folds to its base letter; spaces, "&" and other
    // punctuation become "_"; the extension survives.
    assert_eq!(
        restrict_filename_ascii("Café & Croissants.mp4"),
        "Cafe_Croissants.mp4"
    );
    assert_eq!(
        restrict_filename_ascii("naïve façade — 50%.mkv"),
        "naive_facade_50.mkv"
    );
    assert_eq!(
        restrict_filename_ascii("Straße æther œil.mp4"),
        "Strasse_aether_oeil.mp4"
    );
    // Plain ASCII names pass through untouched.
    assert_eq!(
        restrict_filename_ascii("my-video_01.final.mp4"),
        "my-video_01.final.mp4"
    );
    // Runs of "_" collapse; leading/trailing "_" are stripped.
    assert_eq!(restrict_filename_ascii("a  b.mp4"), "a_b.mp4");
    assert_eq!(restrict_filename_ascii(" _x_ .mp4"), "x.mp4");
    // Quotes and control characters are dropped, not underscored.
    assert_eq!(restrict_filename_ascii("a\"b.mp4"), "ab.mp4");
    // A stem that folds to nothing falls back instead of going empty.
    assert_eq!(restrict_filename_ascii("日本語"), "file");
    assert_eq!(restrict_filename_ascii("日本語.mp4"), "file.mp4");
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
        .restore_existing(&stored_row(
            "https://example.com/ubuntu.iso",
            &dir_s,
            "ubuntu.iso",
            DownloadStatus::Downloading,
        ))
        .unwrap();
    assert_eq!(item.filename(), "ubuntu.iso");
    assert_eq!(item.status(), DownloadStatus::Queued);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn queue_roundtrip_and_mapping() {
    // Old queue files use the same lowercase status words.
    let legacy: StoredQueue = serde_json::from_str(
        r#"{"version":2,"items":[{"url":"https://example.com/c.iso","dest_dir":"/tmp","filename":"c.iso","status":"done"}]}"#,
    )
    .unwrap();
    assert_eq!(legacy.items[0].status, DownloadStatus::Done);
    let q = StoredQueue {
        version: QUEUE_VERSION,
        items: vec![
            StoredItem {
                id: None,
                url: "https://example.com/a.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "a.iso".to_string(),
                status: DownloadStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
            },
            StoredItem {
                id: None,
                url: "https://example.com/b.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "b.iso".to_string(),
                status: DownloadStatus::Done,
                progress: 1.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
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
    crate::torrent::stage_selection(&pseudo, vec![0]);
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
        assert!(
            manager
                .restore_existing(&stored_row(
                    "https://example.com/f.iso",
                    "/tmp/dl",
                    bad,
                    DownloadStatus::Queued
                ))
                .is_err()
        );
    }
    assert!(
        manager
            .restore_existing(&stored_row(
                "https://example.com/f.iso",
                "relative/dir",
                "f.iso",
                DownloadStatus::Queued
            ))
            .is_err()
    );
    assert_eq!(manager.store().n_items(), 0);
}

#[test]
fn batch_restore_hundred_done() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("batch");
    let settings = test_settings();
    let items: Vec<StoredItem> = (0..100)
        .map(|i| StoredItem {
            id: None,
            url: format!("https://example.com/f{i}.iso"),
            dest_dir: "/tmp/dl".to_string(),
            filename: format!("f{i}.iso"),
            status: DownloadStatus::Done,
            progress: 1.0,
            segments: None,
            selected_files: None,
            output_dir: None,
            video_source: None,
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
        ("p.iso", DownloadStatus::Paused),
        ("f.iso", DownloadStatus::Failed),
        ("d.iso", DownloadStatus::Done),
    ]
    .into_iter()
    .map(|(f, status)| StoredItem {
        id: None,
        url: format!("https://example.com/{f}"),
        dest_dir: "/tmp/dl".to_string(),
        filename: f.to_string(),
        status,
        progress: 0.5,
        segments: None,
        selected_files: None,
        output_dir: None,
        video_source: None,
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
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("restored-mapping");
    let settings = test_settings();
    // Occupy the only slot so restored rows never spawn an engine.
    settings.set_int("max-concurrent", 1).unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let holder = tokio_rt().spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    manager.running.borrow_mut().insert(99, holder);
    // Settled rows keep their status; anything resumable requeues.
    for (stored, expected) in [
        (DownloadStatus::Paused, DownloadStatus::Paused),
        (DownloadStatus::Failed, DownloadStatus::Failed),
        (DownloadStatus::Done, DownloadStatus::Done),
        (DownloadStatus::Queued, DownloadStatus::Queued),
        (DownloadStatus::Downloading, DownloadStatus::Queued),
    ] {
        let item = manager
            .restore_existing(&stored_row(
                "https://example.com/m.iso",
                "/tmp/dl",
                "m.iso",
                stored,
            ))
            .unwrap();
        assert_eq!(item.status(), expected);
        manager.remove(item.id());
    }
    manager.cancel_all();
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
    assert_eq!(queue.items[0].status, DownloadStatus::Queued);
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
    // Quiesce like run_loop: the cancelled pump's woken tail must finish
    // here, not on a later test's thread (thread-guard abort).
    quiesce(&ctx);
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
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
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
                id: None,
                url: format!("http://127.0.0.1:{port}/big.bin"),
                dest_dir: dl.to_string_lossy().into_owned(),
                filename: "big.bin".to_string(),
                status: DownloadStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
                output_dir: None,
                video_source: None,
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
        .restore_existing(&stored_row(
            &url,
            &dest,
            "t.bin",
            DownloadStatus::Downloading,
        ))
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

#[test]
fn aggregate_downsamples_by_half() {
    assert!(aggregate(&[], 8).is_empty());
    assert!(aggregate(&[true, false], 0).is_empty());
    assert_eq!(aggregate(&[true, true, true, false], 2), vec![true, true]);
    assert_eq!(
        aggregate(&[true, false, false, false], 2),
        vec![true, false]
    );
    // Fewer pieces than cells: one true piece lights its own cells.
    let cells = aggregate(&[false, false, true, false], 8);
    assert_eq!(cells.len(), 8);
    assert!(cells[4] && cells[5]);
    assert_eq!(cells.iter().filter(|b| **b).count(), 2);
    assert_eq!(
        aggregate(&[true; 4096], BLOCK_CELLS),
        vec![true; BLOCK_CELLS]
    );
}

#[test]
fn piece_bitmap_prefers_segment_then_torrent_then_bytes() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("piece-bitmap");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    assert!(manager.piece_bitmap(7).is_empty());

    let item = DownloadItem::new(7, "https://example.com/big.bin", "big.bin", "/tmp/dl");
    manager.store().append(&item);
    // No bytes yet: nothing known.
    assert!(manager.piece_bitmap(7).is_empty());
    // Single-stream fallback fills a byte-progress prefix at BLOCK_CELLS.
    item.set_progress(0.5);
    let prefix = manager.piece_bitmap(7);
    assert_eq!(prefix.len(), BLOCK_CELLS);
    assert!(prefix[..128].iter().all(|b| *b));
    assert!(prefix[128..].iter().all(|b| !b));
    // A live segmented bitmap wins over the byte fill.
    let mut st = SegmentState::new(4 * PIECE_MIN);
    st.mark(1);
    manager.segment_state.borrow_mut().insert(7, st);
    assert_eq!(
        manager.piece_bitmap(7),
        manager.segment_state.borrow().get(&7).unwrap().done
    );

    // Torrent haves win over bytes but lose nothing: no segment entry here.
    let torrent = DownloadItem::new(
        8,
        "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=t",
        "t",
        "/tmp/dl",
    );
    manager.store().append(&torrent);
    // Magnets never byte-fill: no session haves yet, so empty.
    torrent.set_progress(0.5);
    assert!(manager.piece_bitmap(8).is_empty());
    let haves = vec![true, false, true];
    manager.torrent_pieces.borrow_mut().insert(8, haves.clone());
    assert_eq!(manager.piece_bitmap(8), haves);
}

#[test]
fn remove_cleans_video_staging() {
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-staging");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 910_000 + std::process::id() as u64;
    let dir = crate::video::staging_dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
    // Dest-dir parts (yt-dlp defaults) go with the row too; the
    // finished file and foreign neighbors stay.
    let destdir = std::env::temp_dir().join(format!("grab-remove-parts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&destdir);
    std::fs::create_dir_all(&destdir).unwrap();
    for n in ["v.mp4", "v.srt", "v.video.mp4", "v.audio.webm.part"] {
        std::fs::write(destdir.join(n), b"x").unwrap();
    }
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        destdir.to_str().unwrap(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.remove(id);
    assert!(!dir.exists(), "staged sidecar must go with the row");
    assert!(!destdir.join("v.video.mp4").exists(), "dest parts go too");
    assert!(
        !destdir.join("v.audio.webm.part").exists(),
        "part shells go too"
    );
    assert!(destdir.join("v.mp4").exists(), "finished file stays");
    assert!(destdir.join("v.srt").exists(), "foreign files stay");
    let _ = std::fs::remove_dir_all(&destdir);
}

#[test]
fn removing_a_plain_row_still_aborts_its_task_and_frees_the_slot() {
    // Regression guard. An earlier attempt made removal cooperative for
    // *every* row, so a plain HTTP or torrent row -- which has no
    // `video_abort` sender and never reaches the video cleanup path -- kept
    // its engine writing after the row was gone, and the completed handle
    // then leaked a concurrency slot, because the pump tail early-returns
    // on the epoch entry `remove` drops and so never clears it.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-plain-abort");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 923_000 + std::process::id() as u64;
    let item = DownloadItem::new(id, "https://example.com/x.bin", "x.bin", "/tmp/dl");
    manager.store().append(&item);
    manager.epoch.borrow_mut().insert(id, 1);

    // A Drop flag inside the task, not an absence checked after a sleep:
    // asserting a flag was *not* set passes just as well when the task was
    // detached as when it was aborted.
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dropped_task = std::sync::Arc::clone(&dropped);
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_task = std::sync::Arc::clone(&started);
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let _flag = DropFlag(dropped_task);
        started_task.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    manager.running.borrow_mut().insert(id, handle);
    // The task must be genuinely up before the removal: aborting a
    // never-polled task would pass without proving anything ran.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        started.load(Ordering::SeqCst),
        "the stand-in engine never started, so the removal proved nothing"
    );

    manager.remove(id);

    assert!(
        manager.running.borrow().get(&id).is_none(),
        "the concurrency slot was not freed immediately, so a long session \
         would run out of slots"
    );
    // The abort must have destroyed the task, not detached it: dropping
    // the handle alone leaves the future (and its flag) alive.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dropped.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        dropped.load(Ordering::SeqCst),
        "a plain row's engine was detached rather than aborted: it kept running \
         after its row was removed"
    );
}

#[test]
fn remove_tells_a_live_worker_to_discard_and_waits_for_it_to_stop() {
    // Two things the old `remove` could neither do nor prove: say *how* to
    // stop, and wait for the worker before reclaiming anything.
    //
    // The construction matters more than the assertions. The stand-in
    // worker **recreates** the directories before its late write, because
    // that is what a dying recorder does -- an earlier version of this
    // test wrote into staging without recreating it, so an inline sweep
    // had already removed the parent, the write failed, `.ok()` swallowed
    // it, and the test passed against the very bug it claimed to catch.
    // Recreating the parent is what makes the two orderings differ: under
    // an inline sweep the late files reappear and this fails, and only a
    // sweep that waits for teardown removes them.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-discard-live");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 922_000 + std::process::id() as u64;
    // A destination unique to this test: `clean_dest_parts` scans the
    // directory, so a shared one would let it delete files it does not own.
    let dest_dir = std::env::temp_dir().join(format!("grab-rmdiscard-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let staging = crate::video::staging_dir(id);
    std::fs::create_dir_all(&staging).unwrap();
    let part = dest_dir.join("v.live.mp4.part");
    std::fs::write(&part, b"recorded").unwrap();

    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: true,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.live_rows.borrow_mut().insert(id);
    manager.epoch.borrow_mut().insert(id, 1);

    let (intent_tx, intent_rx) = tokio::sync::oneshot::channel();
    manager.video_abort.borrow_mut().insert(id, intent_tx);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen_task = std::sync::Arc::clone(&seen);
    let done = std::sync::Arc::new(std::sync::Mutex::new(false));
    let done_task = std::sync::Arc::clone(&done);
    let wrote = std::sync::Arc::new(std::sync::Mutex::new((false, false)));
    let wrote_task = std::sync::Arc::clone(&wrote);
    let staging_task = staging.clone();
    let dest_task = dest_dir.clone();
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let intent = intent_rx.await.ok();
        *seen_task.lock().unwrap() = intent;
        // A dying recorder is SIGKILLed, not politely shut down: give the
        // teardown a beat, then recreate scratch the way a late write
        // would -- recreating the parents, which is the whole point.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let staging_ok = std::fs::create_dir_all(&staging_task).is_ok()
            && std::fs::write(staging_task.join("late-remux"), b"late").is_ok();
        let dest_ok = std::fs::write(dest_task.join("v.live.mp4"), b"late delivery").is_ok();
        *wrote_task.lock().unwrap() = (staging_ok, dest_ok);
        *done_task.lock().unwrap() = true;
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    // Cleanup is deferred to a finalizer that waits for the worker, so it
    // cannot have finished when `remove` returns. The worker must still
    // finish writing first -- a fixed sleep here would make the whole test
    // a coin flip on machine speed, so poll with a deadline instead.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !*done.lock().unwrap() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        *done.lock().unwrap(),
        "the stand-in worker never ran to completion"
    );
    // Both late writes must actually have landed: a fixture that silently
    // failed to write would pass against the very bug this catches, since
    // absent files are what the sweep is supposed to leave behind.
    let (staging_ok, dest_ok) = *wrote.lock().unwrap();
    assert!(
        staging_ok && dest_ok,
        "the stand-in worker failed to write its late files (staging: {staging_ok}, \
         dest: {dest_ok}), so the sweep below proved nothing"
    );
    // The finalizer handle is the deterministic signal: awaiting it proves
    // the sweep ran to completion after the worker stopped, with no
    // polling and no shared deadline to starve on a loaded machine. The
    // previous version inferred completion by polling `staging.exists()`,
    // which left the tail assertions racing scheduler delays they could
    // not observe -- exactly the shape of the one unattributed CI failure
    // this test ever produced.
    let finalizer = manager
        .discards
        .borrow_mut()
        .remove(&id)
        .expect("remove tracks a finalizer for a live video row")
        .finalizer;
    let _ = crate::runtime::tokio_rt().block_on(finalizer);
    let listing = || {
        std::fs::read_dir(&dest_dir)
            .map(|r| {
                r.filter_map(|e| e.ok())
                    .map(|e| format!("{:?}", e.file_name()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|e| vec![format!("READDIR-ERR {e:?}")])
    };
    assert!(
        !staging.exists(),
        "staging survived the row: nothing reclaims it once the row is gone"
    );
    assert!(
        !staging.join("late-remux").exists(),
        "the manager swept before the worker stopped, so scratch recreated \
         during teardown outlived the row"
    );
    if part.exists() || dest_dir.join("v.live.mp4").exists() {
        panic!(
            "the recorder's scratch outlived the row, including anything it \
             recreated after the sweep; dest_dir holds {:?}",
            listing()
        );
    }
    assert_eq!(
        *seen.lock().unwrap(),
        Some(crate::video::StopIntent::Discard),
        "remove must say *how* to stop: a removed row told to Preserve runs \
         the finalize path and delivers a file that has no row to belong to"
    );
    assert!(
        !manager.live_rows.borrow().contains(&id),
        "a removed row stayed marked live, so a later row reusing the id would \
         take the live signal-only paths and skip the abort"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn remove_leaves_plain_rows_staging_alone() {
    // Non-video rows never stage: no video source, no cleanup attempt.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-plain");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 930_000 + std::process::id() as u64;
    let dir = crate::video::staging_dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    let item = DownloadItem::new(id, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    manager.store().append(&item);
    manager.remove(id);
    assert!(dir.exists(), "plain rows must not trigger staging cleanup");
    let _ = std::fs::remove_dir_all(&dir);
}

fn proxy_opts(mode: &str, ptype: &str, host: &str, port: i32) -> DownloadOptions {
    DownloadOptions {
        limit_rate: String::new(),
        connections: 4,
        proxy_mode: mode.into(),
        proxy_type: ptype.into(),
        proxy_host: host.into(),
        proxy_port: port,
        cookies_browser: String::new(),
    }
}

#[test]
fn proxy_direct_and_unknown_modes_go_direct() {
    assert!(
        proxy_opts("direct", "socks5", "127.0.0.1", 9050)
            .proxy_config()
            .expect("valid")
            .is_none()
    );
    // Unknown values fail open like system (lenient stored-value rule):
    // the system attempt itself never errors, whatever the desktop
    // holds, so only well-formedness is asserted here.
    assert!(
        proxy_opts("mystery", "socks5", "127.0.0.1", 9050)
            .proxy_config()
            .is_ok()
    );
}

#[test]
fn proxy_manual_builds_remote_dns_socks() {
    let proxy = proxy_opts("manual", "socks5", "127.0.0.1", 9050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    // socks5h: names resolve remotely, never beside the tunnel.
    assert_eq!(proxy.cli_url, "socks5h://127.0.0.1:9050");
    assert!(proxy.no_proxy_env.contains("localhost"));
    // Pooled clients key on the full config: identical configs share
    // a pool entry, differing ones do not.
    let again = proxy_opts("manual", "socks5", "127.0.0.1", 9050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    assert_eq!(proxy.cache_key, again.cache_key);
    let other = proxy_opts("manual", "socks5", "127.0.0.1", 9051)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    assert_ne!(proxy.cache_key, other.cache_key);
    let _ = http_client_for(Some(&proxy));
}

#[test]
fn proxy_manual_rejects_garbage_loudly() {
    // A typo must fail the row, never leak direct.
    assert!(
        proxy_opts("manual", "socks5", "   ", 9050)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "socks5", "127.0.0.1", 0)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "socks5", "127.0.0.1", 65536)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "socks5", "127.0.0.1", -1)
            .proxy_config()
            .is_err()
    );
    assert!(
        proxy_opts("manual", "gopher", "127.0.0.1", 9050)
            .proxy_config()
            .is_err()
    );
    // URL-smuggling hosts must fail, never build `http://a@b:port`.
    for hostile in [
        "proxy.lan@evil.com",
        "proxy.lan/path",
        "proxy.lan?x=1",
        "proxy.lan#frag",
        "proxy lan",
        "proxy.lan\nX-Injected: 1",
    ] {
        assert!(
            proxy_opts("manual", "http", hostile, 8080)
                .proxy_config()
                .is_err(),
            "hostile host accepted: {hostile:?}"
        );
    }
    // IPv6 literals and underscores stay valid (brackets required).
    assert!(
        proxy_opts("manual", "socks5", "[::1]", 9050)
            .proxy_config()
            .expect("valid")
            .is_some()
    );
    // HTTP covers both schemes on one URL.
    let proxy = proxy_opts("manual", "http", "proxy.lan", 8080)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    assert_eq!(proxy.cli_url, "http://proxy.lan:8080");
}

#[test]
fn proxy_clients_are_pooled_per_config() {
    // Proxied configs share one pooled client per proxy config, so a
    // second attempt with the same config reuses the client.
    let proxy = proxy_opts("manual", "socks5", "127.0.0.1", 19050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    let before = proxied_pool_len();
    let _ = http_client_for(Some(&proxy));
    assert_eq!(proxied_pool_len(), before + 1);
    let _ = http_client_for(Some(&proxy));
    assert_eq!(proxied_pool_len(), before + 1);
}

#[test]
fn proxy_mode_and_type_mappings() {
    assert_eq!(proxy_mode_index("manual"), 1);
    assert_eq!(proxy_mode_index("mystery"), 0);
    assert_eq!(proxy_mode_value(2), PROXY_MODE_DIRECT);
    assert_eq!(proxy_mode_value(9), PROXY_MODE_SYSTEM);
    assert_eq!(proxy_type_index("http"), 0);
    assert_eq!(proxy_type_index("mystery"), 2);
    assert_eq!(proxy_type_value(0), "http");
    assert_eq!(proxy_type_value(9), "socks5");
    assert_eq!(proxy_mode_labels().len(), 3);
    assert_eq!(proxy_type_labels(), vec!["HTTP", "HTTPS", "SOCKS5"]);
}

#[test]
fn normalize_no_proxy_entries() {
    assert_eq!(
        normalize_no_proxy(&["*.local".into(), ".example.com ".into(), "  ".into()]),
        "local,example.com"
    );
    assert_eq!(normalize_no_proxy(&[]), "");
}

#[test]
fn cookies_parse_netscape_matrix() {
    let text = "# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n#HttpOnly_.example.com\tTRUE\t/\tTRUE\t9999999999\ttok\tse cret\n#HttpOnly_.secure.example\tTRUE\t/\tTRUE\t9999999999\ts\t1\nbadline\nshort\ta\tb\nsemi.example\tTRUE\t/\tFALSE\t1\tn\tv;w\n.empty\tTRUE\t/\tFALSE\t1\t\tv\n";
    let (jar, count) = crate::cookies::jar_from_export(text);
    // sid + tok survive; short lines, empty names and semicolon
    // values are dropped rather than sent mangled.
    assert_eq!(count, 3);
    let header =
        crate::cookies::cookie_header_for(&jar, "https://example.com/v").expect("in-scope cookies");
    let header = header.to_str().unwrap();
    assert!(header.contains("sid=abc123"), "{header}");
    assert!(header.contains("tok=se cret"), "{header}");
    // Wrong domain and empty jar send nothing.
    assert!(crate::cookies::cookie_header_for(&jar, "https://other.org/").is_none());
    let (empty, _) = crate::cookies::jar_from_export("# nothing here\n");
    assert!(crate::cookies::cookie_header_for(&empty, "https://example.com/").is_none());
}

#[test]
fn cookies_secure_flag_is_scheme_aware() {
    // Secure cookies ride https only, exactly like the browser and
    // yt-dlp treat them; plain cookies ride both schemes identically.
    let (jar, _) = crate::cookies::jar_from_export(
        ".secure.example\tTRUE\t/\tTRUE\t9999999999\ts\t1\n.plain.example\tTRUE\t/\tFALSE\t9999999999\tp\t2\n",
    );
    let https = crate::cookies::cookie_header_for(&jar, "https://secure.example/v")
        .expect("secure over https");
    assert_eq!(https, "s=1");
    assert!(crate::cookies::cookie_header_for(&jar, "http://secure.example/v").is_none());
    let plain =
        crate::cookies::cookie_header_for(&jar, "http://plain.example/v").expect("plain over http");
    assert_eq!(plain, "p=2");
}

#[test]
fn cookies_stamp_request_headers() {
    let (jar, _) =
        crate::cookies::jar_from_export(".example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n");
    let built = stamp_request(
        http_client().get("https://example.com/v"),
        "Grab-test/1.0",
        Some(&jar),
        "https://example.com/v",
    )
    .build()
    .unwrap();
    assert_eq!(built.headers()["user-agent"], "Grab-test/1.0");
    assert_eq!(built.headers()["cookie"], "sid=abc123");
    // No jar, no Cookie header (plain rows unchanged).
    let built = stamp_request(
        http_client().get("https://example.com/v"),
        "",
        None,
        "https://example.com/v",
    )
    .build()
    .unwrap();
    assert!(!built.headers().contains_key("cookie"));
    assert!(!built.headers().contains_key("user-agent"));
}

#[test]
fn cookies_export_roundtrip() {
    // Fake yt-dlp honoring `--cookies PATH`: proves the export glue
    // (spec, temp file, parse) without a browser on the machine.
    let dir = std::env::temp_dir().join(format!("grab-fakecookies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-cookies");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cookies" ]; then out="$a"; fi
    prev="$a"
done
printf '.example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt()
        .block_on(crate::cookies::jar_for_browser(
            "firefox",
            &bin,
            "https://example.com/v",
        ))
        .expect("exported jar");
    let header = crate::cookies::cookie_header_for(&jar, "https://sub.example.com/v")
        .expect("subdomain in scope");
    assert_eq!(header, "sid=abc123");
    // The off switch never spawns.
    let none = crate::runtime::tokio_rt().block_on(crate::cookies::jar_for_browser(
        "none",
        &bin,
        "https://example.com/v",
    ));
    assert!(none.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cookie_export_ignores_ambient_configs() {
    // --ignore-config must reach the export spawn: an ambient user
    // config could otherwise reshape it (its own --cookies/--output),
    // silently degrading export to plain requests. The fake refuses to
    // write the jar without the flag, so a regression surfaces as a
    // jar-less `None` and the expect below fails loudly.
    let dir = std::env::temp_dir().join(format!("grab-ignorecfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-ignorecfg");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
has=0
for a in "$@"; do
    if [ "$a" = "--ignore-config" ]; then has=1; fi
done
[ "$has" = 1 ] || exit 1
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cookies" ]; then out="$a"; fi
    prev="$a"
done
printf '.example.com\tTRUE\t/\tFALSE\t9999999999\tsid\tabc123\n' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt()
        .block_on(crate::cookies::jar_for_browser(
            "firefox",
            &bin,
            "https://example.com/v",
        ))
        .expect("exported jar with --ignore-config");
    assert!(crate::cookies::cookie_header_for(&jar, "https://example.com/v").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_mode_ignores_proxy_env() {
    // Ambient HTTP_PROXY-style variables must never steer Direct rows:
    // proxying is explicit settings or nothing.
    let (_lock, _loop) = test_locks();
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("noenv", "v.bin", 20_000, "0", &[], 44);
    // SAFETY: serial suite; the guard below restores on all paths
    // including panic unwinds.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same serial-suite context as the setters.
            unsafe {
                std::env::remove_var("HTTP_PROXY");
                std::env::remove_var("HTTPS_PROXY");
                std::env::remove_var("ALL_PROXY");
            }
        }
    }
    unsafe {
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:9");
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        std::env::set_var("ALL_PROXY", "http://127.0.0.1:9");
    }
    let _env = EnvGuard;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
    };
    tokio_rt().block_on(run_download(ctx, 1, StartMode::Single));
    let got = std::fs::read(dl.join("v.bin")).unwrap_or_default();
    if got != payload {
        abort(&server, "direct download must ignore proxy env");
    }
    cleanup(&server, &dir);
    drop(_env);
}

#[test]
fn proxy_argv_precedes_end_of_options() {
    // optparse treats everything after `--` positionally: a `--proxy`
    // placed there would download as a second URL. Proxied argv must
    // flag before the separator on every builder.
    let proxy = proxy_opts("manual", "socks5", "127.0.0.1", 9050)
        .proxy_config()
        .expect("valid")
        .expect("proxied");
    let job = crate::video::VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        playlist_item_id: None,
        quality: "best".into(),
        audio_only: false,
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
        speed_limit: None,
        keep_server_date: false,
        video_format_id: None,
        is_live: false,
        live_from_start: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        embed_subs: false,
        sponsorblock_remove: false,
        sponsorblock_mark: false,
        remux_video: None,
        embed_chapters: false,
        proxy: Some(proxy),
    };
    for argv in [
        crate::video_argv::unified_download_argv(
            &job,
            "v+a/bv*+ba/b",
            true,
            "mp4",
            std::path::Path::new("/usr/bin/ffmpeg"),
            std::path::Path::new("/tmp/staging/grab-media.%(ext)s"),
        ),
        crate::video_argv::live_capture_argv(&job, "h", std::path::Path::new("/tmp/x.mp4")),
    ] {
        let flag = argv
            .iter()
            .position(|a| a == "--proxy")
            .expect("proxy flag");
        let sep = argv.iter().position(|a| a == "--").expect("separator");
        assert!(flag < sep, "{argv:?}");
        assert_eq!(argv[flag + 1], "socks5h://127.0.0.1:9050");
    }
}

#[test]
fn single_connection_skips_probe() {
    // Single-use token URLs die on any pre-request: with one
    // connection the engine must never probe, single GET only.
    let (_lock, _loop) = test_locks();
    let Fixture {
        dir,
        dl,
        payload,
        port,
        server,
    } = spawn_fixture("noprobe", "v.bin", 20_000, "0", &[], 45);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = FetchCtx {
        client: http_client().clone(),
        url: format!("http://127.0.0.1:{port}/v.bin"),
        dest: dl.join("v.bin"),
        opts: DownloadOptions {
            ..Default::default()
        },
        cookies: None,
        timeout: Duration::from_secs(30),
        tx,
    };
    tokio_rt().block_on(run_download(ctx, 1, StartMode::Fresh));
    let got = std::fs::read(dl.join("v.bin")).unwrap_or_default();
    if got != payload {
        abort(&server, "single-stream download must complete");
    }
    let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
    assert!(
        ranges.lines().all(|l| l == "full"),
        "no Range request may precede the download: {ranges:?}"
    );
    cleanup(&server, &dir);
}

#[test]
fn stamp_request_sends_self_origin_referer() {
    // Hotlink guards commonly accept the file's own origin; the
    // Referer carries scheme+host only, never path or query.
    let built = stamp_request(
        http_client().get("http://127.0.0.1:8080/a/b?token=secret"),
        "",
        None,
        "http://127.0.0.1:8080/a/b?token=secret",
    )
    .build()
    .unwrap();
    assert_eq!(built.headers()["referer"], "http://127.0.0.1:8080");
    let built = stamp_request(
        http_client().get("https://example.com/v"),
        "",
        None,
        "https://example.com/v",
    )
    .build()
    .unwrap();
    assert_eq!(built.headers()["referer"], "https://example.com");
}

#[test]
fn cookies_export_failure_means_plain_requests() {
    // A failing export (bad profile, locked browser) degrades to None —
    // plain requests — instead of bricking authed downloads. Distinct
    // browser name: the roundtrip test's cached jar must not leak in.
    let dir = std::env::temp_dir().join(format!("grab-fakecookies-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-cookies");
    std::fs::write(&bin, "#!/bin/sh\necho 'locked profile' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt().block_on(crate::cookies::jar_for_browser(
        "chrome",
        &bin,
        "https://example.com/v",
    ));
    assert!(jar.is_none(), "failed export degrades to plain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cookies_empty_export_still_returns_jar() {
    // Logged-out profile (valid but empty export) is a usable answer:
    // callers send no Cookie header and move on.
    let dir = std::env::temp_dir().join(format!("grab-fakecookies-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp-cookies");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cookies" ]; then out="$a"; fi
    prev="$a"
done
printf '# Netscape HTTP Cookie File\n' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let jar = crate::runtime::tokio_rt()
        .block_on(crate::cookies::jar_for_browser(
            "opera",
            &bin,
            "https://example.com/v",
        ))
        .expect("empty export is still an answer");
    assert!(
        crate::cookies::cookie_header_for(&jar, "https://example.com/v").is_none(),
        "nothing in scope, no header"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_finished_drops_only_done() {
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("clear-finished");
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let add = |id: u64, url: &str, name: &str, status: DownloadStatus| {
        let it = DownloadItem::new(id, url, name, "/tmp/dl");
        it.set_status(status);
        m.store().append(&it);
    };
    add(
        1,
        "https://example.com/a.iso",
        "a.iso",
        DownloadStatus::Done,
    );
    add(
        2,
        "https://example.com/b.iso",
        "b.iso",
        DownloadStatus::Done,
    );
    add(
        3,
        "https://example.com/c.iso",
        "c.iso",
        DownloadStatus::Queued,
    );
    add(
        4,
        "https://example.com/d.iso",
        "d.iso",
        DownloadStatus::Failed,
    );
    assert_eq!(m.finished_count(), 2);
    assert_eq!(m.clear_finished(), 2);
    assert_eq!(m.finished_count(), 0);
    assert_eq!(m.store().n_items(), 2);
    // The persisted queue carries only the survivors.
    let text = std::fs::read_to_string(&qf).unwrap();
    let queue: StoredQueue = serde_json::from_str(&text).unwrap();
    assert_eq!(queue.items.len(), 2);
    assert!(
        queue
            .items
            .iter()
            .all(|it| it.status != DownloadStatus::Done),
        "no Done rows may persist"
    );
    // Empty clear is a quiet no-op (no file churn, no UI sync).
    let mtime = std::fs::metadata(&qf).and_then(|m| m.modified()).ok();
    let syncs = std::rc::Rc::new(std::cell::Cell::new(0u32));
    m.set_on_change({
        let syncs = syncs.clone();
        move || {
            syncs.set(syncs.get() + 1);
        }
    });
    assert_eq!(m.clear_finished(), 0);
    assert_eq!(syncs.get(), 0, "empty clear must not sync the UI");
    assert_eq!(
        std::fs::metadata(&qf).and_then(|m| m.modified()).ok(),
        mtime,
        "empty clear must not rewrite the queue file"
    );
    m.cancel_all();
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn restore_dedups_finished_urls() {
    // Queue files written before dedup may hold several Done rows per
    // URL; restore collapses them so the newest (last) one wins.
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let qf = test_queue_file("history-dedup");
    let settings = test_settings();
    let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
    for (id, name) in [(1u64, "old.iso"), (2, "new.iso")] {
        let it = DownloadItem::new(id, "https://example.com/x.iso", name, "/tmp/dl");
        it.set_progress(1.0);
        it.set_status(DownloadStatus::Done);
        it.set_detail("Finished".to_string());
        m1.store().append(&it);
    }
    m1.persist_queue();

    let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    m2.restore_queue();
    assert_eq!(m2.store().n_items(), 1);
    let it = m2.store().item(0).and_downcast::<DownloadItem>().unwrap();
    assert_eq!(it.filename(), "new.iso");
    assert_eq!(it.status(), DownloadStatus::Done);
    let _ = std::fs::remove_file(&qf);
}

#[test]
fn drop_finished_duplicates_keeps_active_and_newest() {
    // The finish hook's policy, directly: the just-finished row stays,
    // older Done rows for the URL go, everything else is user intent.
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("history-dedup-live");
    let settings = test_settings();
    let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let add = |id: u64, url: &str, status: DownloadStatus| {
        let it = DownloadItem::new(id, url, &format!("f{id}.iso"), "/tmp/dl");
        it.set_status(status);
        m.store().append(&it);
    };
    add(1, "https://example.com/a.iso", DownloadStatus::Done);
    add(2, "https://example.com/a.iso", DownloadStatus::Done);
    add(3, "https://example.com/a.iso", DownloadStatus::Queued);
    add(4, "https://example.com/b.iso", DownloadStatus::Done);
    // Row 2 just finished: row 1 (older Done, same URL) drops; the
    // queued row and the other URL are untouched.
    m.drop_finished_duplicates("https://example.com/a.iso", 2);
    let remaining: Vec<(u64, DownloadStatus)> = (0..m.store().n_items())
        .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
        .map(|it| (it.id(), it.status()))
        .collect();
    assert_eq!(
        remaining,
        vec![
            (2, DownloadStatus::Done),
            (3, DownloadStatus::Queued),
            (4, DownloadStatus::Done)
        ]
    );
    // Unnormalizable URLs skip quietly instead of dropping anything.
    m.drop_finished_duplicates("", u64::MAX);
    assert_eq!(m.store().n_items(), 3);
}

#[test]
fn enqueue_video_reserves_part_namespaced_stems() {
    // A foreign file under Grab's part namespace reserves the stem: the
    // intake must dedupe onward so a later clean_dest_parts sweep can
    // never touch files Grab didn't write.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video-reserve");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-video-reserve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("Clip.video.mp4"), b"foreign").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "Clip (1).mp4");
    // The foreign file is untouched and the fresh stem is unreserved.
    assert_eq!(
        std::fs::read(dest.join("Clip.video.mp4")).unwrap(),
        b"foreign"
    );
    assert!(!crate::video_staging::stem_reserved_in(
        &crate::video_staging::dir_file_names(&dest),
        "Clip (1)"
    ));
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_dir(item.id()));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn delete_download_trashes_video_sidecars() {
    // Trashing a video row takes its collected subtitle sidecars along;
    // plain rows keep a same-named srt (never Grab's).
    let _lock = QUEUE_FILE_LOCK.lock().unwrap();
    let _qf = test_queue_file("video-del-sidecar");
    let settings = test_settings();
    // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
    let dir = glib::user_data_dir().join(format!("grab-video-del-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = gio::ListStore::new::<DownloadItem>();
    let manager = DownloadManager::new(store.clone(), settings);
    let video = DownloadItem::new(
        11,
        "https://x.com/u/status/1",
        "Clip.mp4",
        &dir.to_string_lossy(),
    );
    video.set_status(DownloadStatus::Done);
    store.append(&video);
    manager.video_sources.borrow_mut().insert(
        11,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    let plain = DownloadItem::new(
        12,
        "https://example.com/other.mp4",
        "Other.mp4",
        &dir.to_string_lossy(),
    );
    plain.set_status(DownloadStatus::Done);
    store.append(&plain);
    for n in [
        "Clip.mp4",
        "Clip.en.srt",
        "Clip.fr.srt",
        "Other.mp4",
        "Other.en.srt",
    ] {
        std::fs::write(dir.join(n), b"x").unwrap();
    }
    assert!(manager.delete_download(11).is_ok());
    assert!(manager.delete_download(12).is_ok());
    // Video + its sidecars trashed; plain row's same-named srt stays.
    for n in ["Clip.mp4", "Clip.en.srt", "Clip.fr.srt", "Other.mp4"] {
        assert!(!dir.join(n).exists(), "{n} must be trashed");
    }
    assert!(dir.join("Other.en.srt").exists());
    assert_eq!(store.n_items(), 0);
    // Undo the test's own Trash litter.
    let trash = glib::user_data_dir().join("Trash");
    for n in ["Clip.mp4", "Clip.en.srt", "Clip.fr.srt", "Other.mp4"] {
        let _ = std::fs::remove_file(trash.join(format!("files/{n}")));
        let _ = std::fs::remove_file(trash.join(format!("info/{n}.trashinfo")));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enqueue_video_reserves_subtitle_sidecar_stems() {
    // A pre-existing foreign sidecar reserves the stem just like a
    // part file: intake dedupes onward so a later row delete (which
    // trashes every offered-language sidecar) can never take a file
    // Grab didn't write.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video-reserve-srt");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-video-reserve-srt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("Clip.en.srt"), b"foreign").unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(item.filename(), "Clip (1).mp4");
    assert_eq!(std::fs::read(dest.join("Clip.en.srt")).unwrap(), b"foreign");
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_dir(item.id()));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn enqueue_video_accepts_unlisted_url() {
    // No domain gate: any normalizable URL queues as a video row (the
    // dialog owns routing; misuse fails loudly at resolve instead of
    // silently saving HTML). Garbage still rejected by normalization.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("enqueue-video-probed");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let choices = crate::media_types::VideoChoices {
        quality: "1080p".to_string(),
        audio_only: false,
        video_format_id: None,
        is_live: false,
        playlist_item_id: None,
    };
    let item = manager
        .enqueue_video(
            "https://example.com/f.iso",
            Some("/tmp/dl"),
            None,
            choices.clone(),
        )
        .expect("probed intake");
    assert_eq!(item.filename(), "f.iso");
    assert!(
        manager
            .enqueue_video("not a url", Some("/tmp/dl"), None, choices)
            .is_err()
    );
    // Drain the spawned worker (fails fast without tools) so no woken
    // pump tail trips the next test's thread guard.
    drain_engine(&manager, item.id());
    manager.cancel_all();
    crate::video::clean_staging(&crate::video::staging_dir(item.id()));
}

#[test]
fn a_video_attempt_gets_a_gate_and_a_plain_row_does_not() {
    // The gate is the only thing that can arbitrate delivery, so a video
    // attempt must have one and a plain row must not: a plain row is
    // aborted outright and never arbitrates anything.
    let (_q, _l) = test_locks();
    let qf = test_queue_file("gate-spawn");
    let settings = test_settings();
    // Fail validation after gate creation but before the worker/pump starts,
    // keeping this ownership test synchronous and free of thread-affine
    // futures.
    settings
        .set_string(
            crate::settings::key::PROXY_MODE,
            crate::download_net::PROXY_MODE_MANUAL,
        )
        .unwrap();
    settings
        .set_string(crate::settings::key::PROXY_HOST, "")
        .unwrap();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);

    let plain = DownloadItem::new(700_001, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    manager.store().append(&plain);
    manager.epoch.borrow_mut().insert(plain.id(), 1);
    assert!(
        manager.gate_for(plain.id()).is_none(),
        "a plain row must not be given a gate to arbitrate"
    );

    let mut row = stored_row(
        "https://example.com/v.mp4",
        "/tmp/dl",
        "v.mp4",
        DownloadStatus::Queued,
    );
    row.video_source = Some(crate::media_types::VideoSource::Page {
        page_url: "https://example.com/v.mp4".to_string(),
        media_url: None,
        expires_at: None,
        quality: "1080p".to_string(),
        audio_only: false,
        is_live: false,
        video_format_id: None,
        playlist_item_id: None,
    });
    let item = manager.restore_existing(&row).expect("video row");
    assert!(
        manager.gate_for(item.id()).is_some(),
        "a video row with no gate cannot arbitrate delivery, so a removal \
         could not stop it placing a file"
    );

    manager
        .settings()
        .set_string(crate::settings::key::PROXY_MODE, PROXY_MODE_DIRECT)
        .unwrap();
    let _ = std::fs::remove_file(qf);
}

#[test]
fn a_removal_claims_the_gate_before_the_worker_can_commit() {
    // The decision must be claimed at `remove` time, not discovered later:
    // a worker that has not reached its rename yet must find the row gone.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-claims-gate");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 924_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-gateclaim-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    let part = dest_dir.join("v.live.mp4.part");
    std::fs::write(&part, b"recorded").unwrap();

    manager.remove(id);

    assert!(
        !gate.try_commit(),
        "remove returned without claiming the gate, so a worker that had not \
         reached its rename could still deliver"
    );
    assert!(!gate.was_delivered());
    assert!(
        !part.exists(),
        "remove claimed the gate but left the row's part shell behind: the \
         inline reclaim must sweep it"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn a_finalizer_removes_the_orphan_a_lost_commit_left_behind() {
    // When the commit wins, the file *is* placed, and the row is gone, so
    // the finalizer has to remove it. This is the only sanctioned exception
    // to `clean_dest_parts` never touching a finished file.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-orphan");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 925_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-orphan-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    // The commit already won, so the attempt will place the file.
    assert!(gate.try_commit());
    let worker_dest = dest.clone();
    let handle = crate::runtime::tokio_rt().spawn(async move {
        // Stand in for a worker that was already inside its rename.
        std::fs::write(&worker_dest, b"orphan").expect("stand-in worker must place the orphan");
        gate.mark_delivered();
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while dest.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !dest.exists(),
        "the commit won, so the attempt placed a file, and the row is gone: \
         the orphan outlived it"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn removing_a_settled_video_row_keeps_the_users_finished_file() {
    // A delivered attempt leaves its gate COMMITTING + delivered in the
    // map (the pump tail clears the worker handle but never the gate),
    // with no worker in flight. Removing that settled row must keep the
    // file the user owns: only a worker the finalizer actually awaited
    // can have left an orphan behind.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-settled-keeps");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 926_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-settled-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    std::fs::write(&dest, b"owned").unwrap();
    let staging = crate::video::staging_dir(id);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("manifest.json"), b"{}").unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    assert!(gate.try_commit());
    gate.mark_delivered();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    // No running handle: the attempt settled long ago.

    manager.remove(id);

    assert!(
        dest.exists(),
        "no worker was in flight, so nothing could have orphaned this file: \
         the finished file belongs to the user"
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"owned",
        "the settled row's file must survive removal byte-for-byte"
    );
    assert!(
        !staging.exists(),
        "the settled row's scratch must still go with the row"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn a_pending_discard_reserves_the_destination_against_intake() {
    // Between remove and its finalizer, the stem must not be claimable:
    // otherwise the finalizer's stem-wide sweep deletes the *new* row's
    // part files.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-intake");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    assert!(!manager.dest_reserved(&dest));
    manager.reserve_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "intake could claim a destination whose row is still tearing down"
    );
    manager.release_dest(&dest);
    assert!(
        !manager.dest_reserved(&dest),
        "the reservation outlived the cleanup"
    );
}

#[test]
fn an_undo_does_not_reclaim_a_destination_with_a_pending_discard() {
    // Undo bypasses intake dedupe and starts a row with the same filename
    // immediately, so it has to consult the reservation too.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-undo");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    manager.reserve_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "Undo was allowed to reclaim a destination with a pending discard"
    );
}

#[test]
fn a_reserved_destination_forces_video_intake_to_dedupe() {
    // Between remove and its finalizer the destination must count as taken
    // at intake, or the finalizer's stem-wide sweep deletes the new row's
    // part files.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-video-intake");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-intake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    manager.reserve_dest(&dest.join("Clip.mp4"));
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.mp4"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(
        item.filename(),
        "Clip (1).mp4",
        "intake claimed a destination whose row is still tearing down"
    );
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_dir(item.id()));
    manager.release_dest(&dest.join("Clip.mp4"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_pending_discard_reserves_the_stem_against_a_different_extension() {
    // The finalizer's sweep is stem-wide, so the reservation must be too:
    // a different extension on the same stem is still the old row's.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-stem");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dir = std::path::PathBuf::from("/tmp/dl");
    manager.reserve_dest(&dir.join("v.mp4"));
    assert!(
        manager.dest_reserved(&dir.join("v.m4a")),
        "same stem, different extension was not reserved"
    );
    assert!(
        !manager.dest_reserved(&dir.join("w.mp4")),
        "a different stem in the same dir read as reserved"
    );
    assert!(
        !manager.dest_reserved(&std::path::PathBuf::from("/tmp/other/v.m4a")),
        "the same stem in a different dir read as reserved"
    );
    manager.release_dest(&dir.join("v.mp4"));
    assert!(
        !manager.dest_reserved(&dir.join("v.m4a")),
        "the stem reservation outlived the cleanup"
    );
}

#[test]
fn a_reserved_stem_forces_video_intake_to_dedupe() {
    // Same stem, different extension: without the stem-wide reservation
    // intake would claim it and the old finalizer's sweep would delete
    // the new row's part files.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-stem-intake");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-stem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    manager.reserve_dest(&dest.join("Clip.mp4"));
    let item = manager
        .enqueue_video(
            "https://www.youtube.com/watch?v=gXtp6C-3JKo",
            Some(&dest_s),
            Some("Clip.m4a"),
            crate::media_types::VideoChoices {
                quality: "1080p".to_string(),
                audio_only: false,
                video_format_id: None,
                is_live: false,
                playlist_item_id: None,
            },
        )
        .expect("video enqueue");
    assert_eq!(
        item.filename(),
        "Clip (1).m4a",
        "intake claimed a stem whose row is still tearing down"
    );
    drain_engine(&manager, item.id());
    crate::video::clean_staging(&crate::video::staging_dir(item.id()));
    manager.release_dest(&dest.join("Clip.mp4"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_second_discard_keeps_the_reservation_after_the_first_releases() {
    // Remove, Undo, remove again before the first finalizer lands: the
    // same destination is reserved twice, and the first release must not
    // reopen the window while the second teardown is still in flight.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-refcount");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    manager.reserve_dest(&dest);
    manager.reserve_dest(&dest);
    manager.release_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "the first release dropped a reservation a second discard still holds"
    );
    assert!(
        manager.dest_reserved(&std::path::PathBuf::from("/tmp/dl/v.m4a")),
        "the stem half of the reservation was dropped early too"
    );
    manager.release_dest(&dest);
    assert!(
        !manager.dest_reserved(&dest),
        "the reservation outlived both cleanups"
    );
}

#[test]
fn a_reserved_destination_forces_plain_intake_to_dedupe() {
    // The exact-path reservation applies to plain intake too: a plain row
    // claiming a tearing-down destination meets the finalizer's sweep.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-plain-intake");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-plain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    manager.reserve_dest(&dest.join("a.bin"));
    let item = manager
        .enqueue("https://example.com/a.bin", Some(&dest_s), Some("a.bin"))
        .expect("plain enqueue");
    assert_eq!(
        item.filename(),
        "a (1).bin",
        "plain intake claimed a destination whose row is still tearing down"
    );
    manager.cancel(item.id());
    // Let the aborted engine's pump tail settle here, so no woken future
    // outlives this test's queue file.
    quiesce(&glib::MainContext::default());
    manager.release_dest(&dest.join("a.bin"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_finished_name_claim_honours_a_pending_discard() {
    // The Finished claim loop and the DEST_EXISTS requeue share
    // `is_name_taken`: a reserved destination must read as taken there too.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-claim");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-claim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let existing = crate::video_staging::dir_file_names(&dest);
    assert!(!manager.is_name_taken(&dest_s, &existing, "Clip.mp4"));
    manager.reserve_dest(&dest.join("Clip.mp4"));
    assert!(
        manager.is_name_taken(&dest_s, &existing, "Clip.mp4"),
        "a finished-name claim could take a destination still tearing down"
    );
    manager.release_dest(&dest.join("Clip.mp4"));
    assert!(!manager.is_name_taken(&dest_s, &existing, "Clip.mp4"));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_reserved_destination_parks_an_unremoved_row_until_start_next() {
    // Undo bypasses intake dedupe: a reserved destination must requeue the
    // row without starting it, later triggers must not start it while
    // reserved, and releasing alone must not wake it (no wakeup exists yet:
    // the next `start_next` trigger starts it).
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-unremove-park");
    let _notools = NoVideoTools::apply();
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-park-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let reserved = dest.join("Clip.mp4");
    manager.reserve_dest(&reserved);
    let snap = RemovedSnapshot {
        url: "https://vimeo.com/123456".to_string(),
        dest_dir: dest_s,
        filename: "Clip.mp4".to_string(),
        status: DownloadStatus::Downloading,
        progress: 0.3,
        detail: String::new(),
        output_dir: String::new(),
        segments: None,
        video_source: Some(crate::media_types::VideoSource::Page {
            page_url: "https://vimeo.com/123456".to_string(),
            media_url: None,
            expires_at: None,
            quality: "720p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        }),
    };
    let revived = manager.unremove(snap);
    let id = revived.id();
    assert_eq!(revived.status(), DownloadStatus::Queued);
    assert!(
        !manager.running.borrow().contains_key(&id),
        "unremove started a row whose destination is still tearing down"
    );
    manager.start_next();
    assert_eq!(
        revived.status(),
        DownloadStatus::Queued,
        "a later trigger started a row whose destination is still reserved"
    );
    assert!(!manager.running.borrow().contains_key(&id));
    manager.release_dest(&reserved);
    assert_eq!(
        revived.status(),
        DownloadStatus::Queued,
        "releasing the reservation must not start the row on its own"
    );
    assert!(!manager.running.borrow().contains_key(&id));
    manager.start_next();
    assert_eq!(revived.status(), DownloadStatus::Downloading);
    assert!(manager.running.borrow().contains_key(&id));
    drain_engine(&manager, id);
    crate::video::clean_staging(&crate::video::staging_dir(id));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn an_unremove_of_a_settled_row_stays_settled_while_reserved() {
    // Only rows that would actually start divert to the requeue path: a
    // Done snapshot (e.g. clear-finished Undo) over a reserved destination
    // restores as Done and never starts on its own.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-unremove-done");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::env::temp_dir().join(format!("grab-reserve-done-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let dest_s = dest.to_string_lossy().into_owned();
    let reserved = dest.join("Clip.mp4");
    manager.reserve_dest(&reserved);
    let snap = RemovedSnapshot {
        url: "https://vimeo.com/123456".to_string(),
        dest_dir: dest_s,
        filename: "Clip.mp4".to_string(),
        status: DownloadStatus::Done,
        progress: 1.0,
        detail: String::new(),
        output_dir: String::new(),
        segments: None,
        video_source: Some(crate::media_types::VideoSource::Page {
            page_url: "https://vimeo.com/123456".to_string(),
            media_url: None,
            expires_at: None,
            quality: "720p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        }),
    };
    let revived = manager.unremove(snap);
    assert_eq!(
        revived.status(),
        DownloadStatus::Done,
        "a settled restore must not be diverted into a re-download"
    );
    assert!(!manager.running.borrow().contains_key(&revived.id()));
    manager.release_dest(&reserved);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_finalizer_releases_the_reservation_after_the_worker() {
    // The awaited-worker arm holds the destination until the sweep is done,
    // then releases it.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-release-async");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 927_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-reserve-async-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));
    let handle = crate::runtime::tokio_rt().spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    assert!(
        manager.dest_reserved(&dest),
        "remove must hold the destination until the finalizer finishes"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while manager.dest_reserved(&dest) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !manager.dest_reserved(&dest),
        "the finalizer never released the destination"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn removing_a_never_spawned_video_row_releases_synchronously() {
    // With no worker in flight the reclaim runs inline, so the reservation
    // is already gone when remove returns.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-release-sync");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 928_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-reserve-sync-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("w.mp4");
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/2",
        "w.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/2".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.remove(id);
    assert!(
        !manager.dest_reserved(&dest),
        "the inline reclaim must release the destination before remove returns"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[cfg(target_os = "linux")]
#[test]
fn shutdown_during_a_pending_discard_stops_the_worker_rather_than_detaching_it() {
    // The finalizer owns the worker handle. Aborting the finalizer drops
    // that handle, which *detaches* the worker -- yt-dlp would keep running
    // with no supervisor, which is the regression #180 fixed.
    //
    // Driven through the real path: the long-running worker is inserted as
    // the row's running handle and `remove` hands it to `finish_discard`,
    // which retains the abort handle and tracks the finalizer. A
    // hand-built `PendingDiscard` would pass even if `finish_discard`
    // regressed, so this test must not construct one.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("shutdown-discard");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 926_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-shutdisc-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let item = DownloadItem::new(
        id,
        "https://x.com/u/status/1",
        "v.mp4",
        &dest_dir.to_string_lossy(),
    );
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            is_live: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = crate::video::AttemptGate::new();
    manager
        .gates
        .borrow_mut()
        .insert(id, std::sync::Arc::clone(&gate));

    // Scratch the real finalizer must reclaim once the worker stops: a
    // staging sidecar plus a dest-dir part in yt-dlp's split namespace.
    let staging = crate::video::staging_dir(id);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("manifest.json"), b"{}").unwrap();
    let part = dest_dir.join("v.video.mp4");
    std::fs::write(&part, b"recorded").unwrap();

    // A worker that runs long and records that it was dropped.
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let flag = std::sync::Arc::clone(&dropped);
    // Owned by the worker future from construction: aborting the task drops
    // the future (and this guard) whether the abort lands before or after
    // the first poll, so the flag fires in both orderings. Constructing the
    // guard inside the future instead would miss an abort that wins the race
    // with the first poll.
    let guard = DropFlag(flag);
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    manager.running.borrow_mut().insert(id, handle);

    // The real path: `remove` claims the gate and hands the running worker
    // to `finish_discard`, which retains its abort handle beside the
    // finalizer it spawns.
    manager.remove(id);

    manager.shutdown();

    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "shutdown left the worker running: the finalizer's handle was dropped, \
         which detaches the task instead of aborting it"
    );
    assert!(
        !staging.exists(),
        "shutdown never ran the finalizer's staging sweep: aborting finalizers \
         instead of awaiting them would leave this behind"
    );
    assert!(
        !part.exists(),
        "shutdown never ran the finalizer's dest-parts sweep: aborting finalizers \
         instead of awaiting them would leave this behind"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
    let _ = std::fs::remove_dir_all(&staging);
}
