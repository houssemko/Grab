use super::*;
use pretty_assertions::assert_eq;

// ── classify ───────────────────────────────────────────────────────────

#[test]
fn classify_youtube_watch() {
    let url = "https://www.youtube.com/watch?v=gXtp6C-3JKo";
    assert!(matches!(classify(url), VideoSource::Page { page_url, .. } if page_url == url));
}

#[test]
fn classify_youtu_be_short() {
    assert!(is_video_page("https://youtu.be/dQw4w9WgXcQ"));
}

#[test]
fn classify_music_youtube_subdomain() {
    assert!(is_video_page("https://music.youtube.com/watch?v=abc"));
}

#[test]
fn classify_vimeo() {
    assert!(is_video_page("https://vimeo.com/123456"));
}

#[test]
fn classify_tiktok() {
    assert!(is_video_page("https://www.tiktok.com/@user/video/123"));
}

#[test]
fn classify_dailymotion() {
    assert!(is_video_page("https://www.dailymotion.com/video/x5tmt"));
}

#[test]
fn classify_twitch() {
    assert!(is_video_page("https://www.twitch.tv/clip/abc123"));
}

#[test]
fn classify_bilibili() {
    assert!(is_video_page("https://bilibili.com/video/BV1xx411c7mD"));
}

#[test]
fn classify_reddit_video() {
    assert!(is_video_page(
        "https://www.reddit.com/r/pics/comments/abc/test/"
    ));
}

#[test]
fn classify_direct_file() {
    assert_eq!(
        classify("https://example.com/archive.tar.gz"),
        VideoSource::Direct
    );
}

#[test]
fn classify_ftp_is_direct() {
    assert_eq!(classify("ftp://server/file.bin"), VideoSource::Direct);
}

#[test]
fn classify_empty_is_direct() {
    assert_eq!(classify(""), VideoSource::Direct);
}

#[test]
fn classify_bare_host_no_scheme() {
    // Missing scheme → Url::parse fails → Direct (caller must normalize first).
    assert_eq!(classify("youtube.com/watch?v=x"), VideoSource::Direct);
}

#[test]
fn classify_garbage_string() {
    assert_eq!(classify("not a url at all"), VideoSource::Direct);
}

// ── video_domain suffix matching ───────────────────────────────────────

#[test]
fn domain_exact_match() {
    assert!(video_domain("youtube.com"));
}

#[test]
fn domain_subdomain_match() {
    assert!(video_domain("www.youtube.com"));
}

#[test]
fn domain_deep_subdomain() {
    assert!(video_domain("sub.sub.youtube.com"));
}

#[test]
fn domain_trailing_dot_stripped() {
    assert!(video_domain("youtube.com."));
}

#[test]
fn domain_case_insensitive() {
    assert!(video_domain("YouTube.COM"));
}

#[test]
fn domain_unknown_not_match() {
    assert!(!video_domain("google.com"));
}

// ── expiry ─────────────────────────────────────────────────────────────

#[test]
fn expired_none_is_expired() {
    assert!(is_expired(None));
}

#[test]
fn expired_past() {
    assert!(is_expired(Some(0)));
}

#[test]
fn expired_future() {
    let future = now_unix() + 3600;
    assert!(!is_expired(Some(future)));
}

// ── staging ────────────────────────────────────────────────────────────

#[test]
fn staging_dir_is_under_root() {
    let d = staging_dir(42);
    assert!(d.starts_with(staging_root()));
    assert!(d.ends_with("42"));
}

#[test]
fn clean_staging_refuses_outside_root() {
    // /tmp is NOT under our staging root (/tmp/grab-video), so clean_staging
    // must not delete anything.
    let fake = std::env::temp_dir().join("grab-video-PROMISE-I-WILL-NOT-DELETE");
    std::fs::create_dir_all(&fake).unwrap();
    clean_staging(&fake);
    assert!(fake.exists());
    let _ = std::fs::remove_dir_all(&fake);
}

#[test]
fn clean_staging_removes_our_dir() {
    let dir = staging_dir(999_999);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(dir.exists());
    clean_staging(&dir);
    assert!(!dir.exists());
}

// ── VideoSource serde round-trip ───────────────────────────────────────

#[test]
fn serde_direct() {
    let s = serde_json::to_string(&VideoSource::Direct).unwrap();
    assert_eq!(s, "\"Direct\"");
    let back: VideoSource = serde_json::from_str(&s).unwrap();
    assert_eq!(back, VideoSource::Direct);
}

#[test]
fn serde_page_round_trip() {
    let src = VideoSource::Page {
        page_url: "https://vimeo.com/99".into(),
        media_url: None,
        expires_at: Some(1_700_000_000),
        quality: "720p".into(),
        audio_only: true,
    };
    let json = serde_json::to_string(&src).unwrap();
    let back: VideoSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back, src);
}

#[test]
fn serde_page_old_json_gets_defaults() {
    // Queue files written before quality/audio_only existed must still
    // parse: quality falls back to 1080p, audio to off.
    let json = r#"{"Page":{"page_url":"https://vimeo.com/99","media_url":null,"expires_at":null}}"#;
    let back: VideoSource = serde_json::from_str(json).unwrap();
    assert_eq!(
        back,
        VideoSource::Page {
            page_url: "https://vimeo.com/99".into(),
            media_url: None,
            expires_at: None,
            quality: "1080p".into(),
            audio_only: false,
        }
    );
}

// ── quality mapping ──────────────────────────────────────────────────

#[test]
fn quality_round_trip() {
    for (i, v) in VIDEO_QUALITY_VALUES.iter().enumerate() {
        assert_eq!(quality_index(v), i);
        assert_eq!(quality_value(i), *v);
    }
}

#[test]
fn quality_unknown_falls_back_to_1080p() {
    assert_eq!(quality_index("8k"), 3);
    assert_eq!(quality_index(""), 3);
    assert_eq!(VIDEO_QUALITY_VALUES[quality_index("audio")], "1080p");
}

#[test]
fn quality_oob_index_falls_back() {
    assert_eq!(quality_value(99), "1080p");
}

#[test]
fn classify_page_carries_defaults() {
    let VideoSource::Page {
        quality,
        audio_only,
        ..
    } = classify("https://www.youtube.com/watch?v=x")
    else {
        panic!("expected Page");
    };
    assert_eq!(quality, "1080p");
    assert!(!audio_only);
}

// ── find_in_dirs (pure, no env races) ─────────────────────────────────

#[test]
fn find_in_dirs_picks_first_match() {
    let tmp = std::env::temp_dir().join("grab-test-find-in-dirs");
    let sub = tmp.join("a");
    std::fs::create_dir_all(&sub).unwrap();
    let bin = sub.join("tool");
    std::fs::write(&bin, b"").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let sub_b = tmp.join("b");
    std::fs::create_dir_all(&sub_b).unwrap();
    let bin_b = sub_b.join("tool");
    std::fs::write(&bin_b, b"").unwrap();
    std::fs::set_permissions(&bin_b, std::fs::Permissions::from_mode(0o755)).unwrap();

    let found = find_in_dirs("tool", &[sub_b, sub]);
    assert_eq!(found.as_deref(), Some(bin_b.as_path()));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn find_in_dirs_none_when_non_executable() {
    let tmp = std::env::temp_dir().join("grab-test-find-noexec");
    std::fs::create_dir_all(&tmp).unwrap();
    let bin = tmp.join("noexec");
    std::fs::write(&bin, b"").unwrap();
    // mode 0o644 — not executable
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(find_in_dirs("noexec", std::slice::from_ref(&tmp)).is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

// ── user_lib_dir (env-driven, no assertions on real system) ────────────

#[test]
fn user_lib_dir_ends_with_grab_libs() {
    let d = user_lib_dir();
    assert!(
        d.ends_with("grab/libs"),
        "expected …/grab/libs, got {}",
        d.display()
    );
}

// ── quality selector ─────────────────────────────────────────────────

#[test]
fn selector_for_quality_maps_all() {
    use yt_dlp::model::selector::VideoQuality;
    assert_eq!(selector_for_quality("best"), VideoQuality::Best);
    assert_eq!(
        selector_for_quality("2160p"),
        VideoQuality::CustomHeight(2160)
    );
    assert_eq!(
        selector_for_quality("1440p"),
        VideoQuality::CustomHeight(1440)
    );
    assert_eq!(
        selector_for_quality("1080p"),
        VideoQuality::CustomHeight(1080)
    );
    assert_eq!(
        selector_for_quality("720p"),
        VideoQuality::CustomHeight(720)
    );
    assert_eq!(
        selector_for_quality("480p"),
        VideoQuality::CustomHeight(480)
    );
    assert_eq!(selector_for_quality("8k"), VideoQuality::CustomHeight(1080));
    assert_eq!(selector_for_quality(""), VideoQuality::CustomHeight(1080));
}

// ── manifest + resume plan ───────────────────────────────────────────

fn test_manifest() -> VideoManifest {
    VideoManifest {
        page_url: "https://vimeo.com/99".into(),
        quality: "720p".into(),
        audio_only: false,
        video_format_id: Some("137".into()),
        video_ext: "mp4".into(),
        video_bytes: 100,
        audio_format_id: "251".into(),
        audio_ext: "webm".into(),
        audio_bytes: 50,
        final_bytes: None,
    }
}

fn test_manifest_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("grab-manifest-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_query<'a>(
    manifest: Option<&'a VideoManifest>,
    dir: &'a std::path::Path,
    dest: &'a std::path::Path,
) -> ResumeQuery<'a> {
    ResumeQuery {
        manifest,
        dir,
        dest,
        page_url: "https://vimeo.com/99",
        quality: "720p",
        audio_only: false,
        video: Some(("137", "mp4")),
        audio: ("251", "webm"),
    }
}

#[test]
fn resume_plan_fresh_without_manifest() {
    let dir = test_manifest_dir("fresh");
    let dest = dir.join("Clip.mp4");
    let q = test_query(None, &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_combine_only_with_verified_parts() {
    let dir = test_manifest_dir("combine");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 50]).unwrap();
    let dest = dir.join("Clip.mp4");
    let m = test_manifest();
    let q = test_query(Some(&m), &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::CombineOnly);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_size_mismatch() {
    let dir = test_manifest_dir("truncated");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 100]).unwrap();
    // Audio part truncated: re-download, never merge a partial.
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 49]).unwrap();
    let dest = dir.join("Clip.mp4");
    let m = test_manifest();
    let q = test_query(Some(&m), &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_selection_change() {
    let dir = test_manifest_dir("reselect");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 50]).unwrap();
    let dest = dir.join("Clip.mp4");
    let m = test_manifest();
    // Same parts, but the row now wants 1080p: re-download.
    let q = ResumeQuery {
        quality: "1080p",
        ..test_query(Some(&m), &dir, &dest)
    };
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    // Same prefs, but the extractor picked another audio format: re-download.
    let q = ResumeQuery {
        audio: ("250", "webm"),
        ..test_query(Some(&m), &dir, &dest)
    };
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_finished_when_dest_complete() {
    let dir = test_manifest_dir("finished");
    let dest = dir.join("Clip.mp4");
    std::fs::write(&dest, vec![0u8; 1000]).unwrap();
    let mut m = test_manifest();
    m.final_bytes = Some(1000);
    let q = test_query(Some(&m), &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Finished);
    // Same manifest, foreign file at dest: fall through to parts check
    // (absent here) instead of adopting someone else's bytes.
    std::fs::write(&dest, vec![0u8; 999]).unwrap();
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_audio_only() {
    let dir = test_manifest_dir("audio-only");
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 50]).unwrap();
    let dest = dir.join("Clip.m4a");
    let mut m = test_manifest();
    m.audio_only = true;
    m.video_format_id = None;
    m.video_ext = String::new();
    let q = ResumeQuery {
        audio_only: true,
        video: None,
        ..test_query(Some(&m), &dir, &dest)
    };
    assert_eq!(resume_plan(&q), ResumePlan::CombineOnly);
    // A stale video expectation against an audio-only manifest: re-download.
    let q = test_query(Some(&m), &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn manifest_serde_round_trip() {
    let dir = test_manifest_dir("serde");
    let m = test_manifest();
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&m).unwrap(),
    )
    .unwrap();
    assert_eq!(read_manifest(&dir).as_ref(), Some(&m));
    // Corrupt sidecars read as absent (fresh attempt), never fatal.
    std::fs::write(dir.join("manifest.json"), b"{nope").unwrap();
    assert_eq!(read_manifest(&dir), None);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── pipeline failure without tools (no network) ──────────────────────

#[test]
fn pipeline_reports_missing_tools() {
    use crate::video::test_support::NoVideoTools;

    let _guard = NoVideoTools::apply();
    let item_id = 800_000 + std::process::id() as u64;
    let job = VideoJob {
        item_id,
        page_url: "https://vimeo.com/123456".into(),
        quality: "1080p".into(),
        audio_only: false,
        dest: std::env::temp_dir().join("grab-pipeline-probe.mp4"),
        tries: 1,
        timeout_secs: 5,
        user_agent: "test".into(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_video_download(job, abort_rx, tx));
    assert!(matches!(res, Err(VideoError::MissingLibraries(_))));
    // Nothing else was sent: resolving never started without the tools.
    // The abandoned (empty) staging dir is the caller's to drop.
    assert!(rx.try_recv().is_err());
    drop(rx);
    clean_staging(&staging_dir(item_id));
    assert!(!staging_dir(item_id).exists());
}
