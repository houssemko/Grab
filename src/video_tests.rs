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
fn classify_top_up_domains() {
    // Every allowlisted video domain routes to the extractor.
    for url in [
        "https://www.instagram.com/reel/abc123/",
        "https://www.facebook.com/watch/?v=123",
        "https://fb.watch/abc123/",
        "https://www.threads.com/@user/post/abc",
        "https://bsky.app/profile/user/post/abc",
        "https://www.pinterest.com/pin/123/",
        "https://pin.it/abc123",
        "https://user.tumblr.com/post/123",
        "https://vk.com/video-123_456",
        "https://ok.ru/video/123",
        "https://www.coub.com/view/abc",
        "https://www.bitchute.com/video/abc/",
        "https://odysee.com/@channel:abc/video:def",
        "https://rutube.ru/video/abc/",
        "https://www.nicovideo.jp/watch/sm123",
        "https://www.ted.com/talks/speaker_title",
        "https://archive.org/details/some-item",
        "https://drive.google.com/file/d/abc/view",
        "https://www.dropbox.com/s/abc/file.mp4",
        "https://www.mediafire.com/file/abc/file.mp4",
        "https://www.loom.com/share/abc",
        "https://example.wistia.com/medias/abc",
        "https://fast.wistia.net/embed/abc",
        "https://soundcloud.com/artist/track",
        "https://bandcamp.com/track/name",
        "https://artist.bandcamp.com/track/name",
    ] {
        assert!(is_video_page(url), "should route to video: {url}");
    }
}

#[test]
fn classify_drm_walled_stays_direct() {
    // DRM services are deliberately NOT listed: routing them would only
    // promise what the pipeline refuses to fetch.
    for url in [
        "https://www.netflix.com/watch/123",
        "https://www.disneyplus.com/video/abc",
        "https://www.primevideo.com/detail/abc",
        "https://open.spotify.com/track/abc",
    ] {
        assert_eq!(
            classify(url),
            VideoSource::Direct,
            "must stay direct: {url}"
        );
    }
}

// ── format guards ────────────────────────────────────────────────────

fn test_format(overrides: serde_json::Value) -> yt_dlp::model::format::Format {
    let mut base = serde_json::json!({
        "format": "137 - 1920x1080",
        "format_id": "137",
        "protocol": "https",
        "ext": "mp4",
        "url": "https://cdn.example/v.mp4",
        "vcodec": "avc1.640028",
        "acodec": "none",
        "http_headers": {},
        "filesize": 100,
    });
    for (k, v) in overrides.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).expect("test format must parse")
}

#[test]
fn from_format_accepts_plain_https() {
    let sel = StreamSel::from_format(&test_format(serde_json::json!({}))).unwrap();
    assert_eq!(sel.format_id, "137");
    assert_eq!(sel.url, "https://cdn.example/v.mp4");
    assert_eq!(sel.size, Some(100));
}

#[test]
fn from_format_rejects_hls_manifest() {
    let f = test_format(serde_json::json!({"protocol": "m3u8_native"}));
    assert!(StreamSel::from_format(&f).is_err());
}

#[test]
fn from_format_rejects_unknown_protocol() {
    let f = test_format(serde_json::json!({"protocol": "weirdcast"}));
    assert!(StreamSel::from_format(&f).is_err());
}

#[test]
fn from_format_rejects_drm() {
    let f = test_format(serde_json::json!({"has_drm": true}));
    assert!(StreamSel::from_format(&f).is_err());
}

#[test]
fn from_format_rejects_missing_url() {
    let f = test_format(serde_json::json!({"url": null}));
    assert!(StreamSel::from_format(&f).is_err());
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

#[test]
fn ensure_staging_dir_roundtrip_and_clean() {
    let dir = staging_dir(u64::MAX - 8);
    let canon = ensure_staging_dir(&dir).expect("fresh dir verifies");
    assert!(
        canon.starts_with(std::fs::canonicalize(staging_root()).unwrap()),
        "{canon:?}"
    );
    clean_staging(&dir);
    assert!(!dir.exists());
    assert!(!canon.exists());
}

#[cfg(unix)]
#[test]
fn ensure_staging_dir_rejects_symlink_escape() {
    // Pre-planted symlink at the predicted per-item path: creation
    // follows it, but the canonical check must refuse the escape and
    // write nothing through the link.
    std::fs::create_dir_all(staging_root()).unwrap();
    let outside = std::env::temp_dir().join("grab-video-escape-target");
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_file(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    let link = staging_dir(u64::MAX - 7);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let err = ensure_staging_dir(&link).expect_err("symlink escape must fail");
    assert!(err.to_string().contains("escaped"), "{err}");
    assert!(outside.read_dir().unwrap().next().is_none());
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&outside);
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
        is_live: false,
        video_format_id: Some("137".into()),
    };
    let json = serde_json::to_string(&src).unwrap();
    let back: VideoSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back, src);
}

#[test]
fn serde_page_old_json_gets_defaults() {
    // Queue files written before quality/format existed must still
    // parse (quality falls back to 1080p, no pin) — and files that
    // still carry the removed audio_only key must parse too, ignoring it.
    for json in [
        r#"{"Page":{"page_url":"https://vimeo.com/99","media_url":null,"expires_at":null}}"#,
        r#"{"Page":{"page_url":"https://vimeo.com/99","media_url":null,"expires_at":null,"audio_only":true}}"#,
    ] {
        let back: VideoSource = serde_json::from_str(json).unwrap();
        assert_eq!(
            back,
            VideoSource::Page {
                page_url: "https://vimeo.com/99".into(),
                media_url: None,
                expires_at: None,
                quality: "1080p".into(),
                is_live: false,
                video_format_id: None,
            }
        );
    }
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
    let VideoSource::Page { quality, .. } = classify("https://www.youtube.com/watch?v=x") else {
        panic!("expected Page");
    };
    assert_eq!(quality, "1080p");
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
    dest: &'a std::path::Path,
) -> ResumeQuery<'a> {
    ResumeQuery {
        manifest,
        dest,
        page_url: "https://vimeo.com/99",
        quality: "720p",
        video: Some(("137", "mp4")),
        audio: ("251", "webm"),
        video_total: Some(100),
        audio_total: Some(50),
    }
}

#[test]
fn resume_plan_fresh_without_manifest() {
    let dir = test_manifest_dir("fresh");
    let dest = dir.join("Clip.mp4");
    let q = test_query(None, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_combine_only_with_verified_parts() {
    let dir = test_manifest_dir("combine");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 50]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::CombineOnly);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_resumes_truncated_parts() {
    let dir = test_manifest_dir("truncated");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 100]).unwrap();
    // Audio part truncated mid-download: resume it, never re-download.
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 49]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_resumes_pending_manifest() {
    // Pause before any part completed bookkeeping: the pending sidecar
    // (zero bytes recorded) plus partial files on disk means resume.
    let dir = test_manifest_dir("pending");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 60]).unwrap();
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 30]).unwrap();
    let mut m = test_manifest();
    m.video_bytes = 0;
    m.audio_bytes = 0;
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_without_manifest_despite_parts() {
    // Bytes without a matching sidecar are unverifiable (pre-sidecar
    // upgrades, foreign files): wipe and start clean.
    let dir = test_manifest_dir("unverified");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 60]).unwrap();
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 30]).unwrap();
    let q = test_query(None, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_overlong_part() {
    // A part larger than its total cannot be resumed into: wipe it.
    let dir = test_manifest_dir("overlong");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 60]).unwrap();
    let m = test_manifest();
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_sparse_shell() {
    // The killed-attempt shape: full apparent size, nothing on disk
    // (pre-allocated by the engine, sidecar gone with the task). The
    // engine equates size with completeness, so this must never reach
    // it — wipe and start over instead.
    let dir = test_manifest_dir("sparse");
    let dest = dir.join("Clip.mp4");
    let vpart = dest_part_path(&dest, "video", "mp4");
    let apart = dest_part_path(&dest, "audio", "webm");
    std::fs::File::create(&vpart).unwrap().set_len(100).unwrap();
    std::fs::File::create(&apart).unwrap().set_len(50).unwrap();
    assert!(is_sparse_shell(&vpart));
    assert!(is_sparse_shell(&apart));
    let m = test_manifest();
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_when_nothing_on_disk() {
    let dir = test_manifest_dir("empty");
    let dest = dir.join("Clip.mp4");
    let mut m = test_manifest();
    m.video_bytes = 0;
    m.audio_bytes = 0;
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_selection_change() {
    let dir = test_manifest_dir("reselect");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 50]).unwrap();
    let m = test_manifest();
    // Same parts, but the row now wants 1080p: re-download.
    let q = ResumeQuery {
        quality: "1080p",
        ..test_query(Some(&m), &dest)
    };
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    // Same prefs, but the extractor picked another audio format: re-download.
    let q = ResumeQuery {
        audio: ("250", "webm"),
        ..test_query(Some(&m), &dest)
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
    let q = test_query(Some(&m), &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Finished);
    // Same manifest, foreign file at dest: fall through to parts check
    // (absent here) instead of adopting someone else's bytes.
    std::fs::write(&dest, vec![0u8; 999]).unwrap();
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_adopted_single() {
    // Adopted single file (muxed direct, no video leg): the audio part
    // present with matching ids combines without re-downloading.
    let dir = test_manifest_dir("adopted-single");
    let dest = dir.join("Clip.m4a");
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 50]).unwrap();
    let mut m = test_manifest();
    m.video_format_id = None;
    m.video_ext = String::new();
    let q = ResumeQuery {
        video: None,
        ..test_query(Some(&m), &dest)
    };
    assert_eq!(resume_plan(&q), ResumePlan::CombineOnly);
    // A stale video expectation against a videoless manifest: re-download.
    let q = test_query(Some(&m), &dest);
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
        dest: std::env::temp_dir().join("grab-pipeline-probe.mp4"),
        tries: 1,
        timeout_secs: 5,
        user_agent: "test".into(),
        video_format_id: None,
        is_live: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        proxy: None,
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

// ── preview freshness (dialog kick/submit gate) ──────────────────────

fn test_video_info(page_url: &str) -> VideoInfo {
    VideoInfo {
        id: "x".into(),
        title: "T".into(),
        duration: None,
        duration_string: None,
        page_url: page_url.into(),
        expires_at: None,
        formats: vec![],
        is_live: false,
    }
}

#[test]
fn preview_fresh_matches_round_trip() {
    let info = Some(test_video_info("https://www.youtube.com/watch?v=x"));
    assert!(preview_fresh(
        &info,
        "https://www.youtube.com/watch?v=x",
        "https://www.youtube.com/watch?v=x",
    ));
}

#[test]
fn preview_fresh_accepts_canonical_drift() {
    // Extractor canonicalized youtu.be → youtube.com/watch: the stored
    // page URL differs from the typed text, but the round-trip key
    // matches, so Add must proceed instead of re-resolving forever.
    let info = Some(test_video_info("https://www.youtube.com/watch?v=x"));
    assert!(preview_fresh(
        &info,
        "https://youtu.be/x",
        "https://youtu.be/x"
    ));
}

#[test]
fn preview_fresh_rejects_stale_and_empty() {
    let info = Some(test_video_info("https://vimeo.com/1"));
    // User edited the URL after resolving: not fresh.
    assert!(!preview_fresh(
        &info,
        "https://vimeo.com/1",
        "https://vimeo.com/2"
    ));
    // Nothing resolved yet.
    assert!(!preview_fresh(
        &None,
        "https://vimeo.com/1",
        "https://vimeo.com/1"
    ));
    // Empty text never matches, even with a coincidental empty key.
    assert!(!preview_fresh(&info, "", ""));
}

// ── tool versions ────────────────────────────────────────────────────

#[test]
fn parse_yt_dlp_version_matrix() {
    assert_eq!(parse_yt_dlp_version("2026.08.19"), Some([2026, 8, 19]));
    assert_eq!(parse_yt_dlp_version("  2025.01.01\n"), Some([2025, 1, 1]));
    assert_eq!(parse_yt_dlp_version("nightly"), None);
    assert_eq!(parse_yt_dlp_version(""), None);
    assert_eq!(parse_yt_dlp_version("2026.08"), None);
    assert_eq!(parse_yt_dlp_version("not.a.version"), None);
}

#[test]
fn version_floor_accepts_new_rejects_old() {
    assert!([2026, 8, 19] >= MIN_YTDLP_VERSION);
    assert!([2027, 1, 1] >= MIN_YTDLP_VERSION);
    assert!([2026, 1, 1] >= MIN_YTDLP_VERSION);
    assert!([2025, 12, 31] < MIN_YTDLP_VERSION);
}

fn fake_tool(dir: &std::path::Path, name: &str, first_line: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\necho '{first_line}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Fake ffmpeg that behaves like the real one: only single-dash
/// `-version` works, `--version` exits 8. Guards the flag plumbing that
/// once broke every video attempt while the fakes stayed green.
fn fake_ffmpeg_strict(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("ffmpeg");
    std::fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"-version\" ]; then echo 'ffmpeg version n9.0.1'; else echo \"Unrecognized option '$1'.\" >&2; exit 8; fi\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn ensure_tool_versions_accepts_fresh_pair() {
    let dir = std::env::temp_dir().join(format!("grab-versions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yt = fake_tool(&dir, "yt-dlp", "2026.08.19");
    let ff = fake_ffmpeg_strict(&dir);
    let libs = yt_dlp::client::deps::Libraries::new(yt, ff);
    let (yt_v, ff_v) = crate::download::tokio_rt()
        .block_on(ensure_tool_versions(&libs))
        .expect("fresh pair passes");
    assert_eq!(yt_v, "2026.08.19");
    assert!(ff_v.starts_with("ffmpeg version"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_tool_versions_refuses_stale_yt_dlp() {
    let dir = std::env::temp_dir().join(format!("grab-versions-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yt = fake_tool(&dir, "yt-dlp", "2024.10.07");
    let ff = fake_ffmpeg_strict(&dir);
    let libs = yt_dlp::client::deps::Libraries::new(yt, ff);
    let res = crate::download::tokio_rt().block_on(ensure_tool_versions(&libs));
    assert!(
        matches!(res, Err(VideoError::Message(_))),
        "stale binary must fail with the actionable message, got {res:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_tool_versions_refuses_missing_binary() {
    let libs = yt_dlp::client::deps::Libraries::new(
        "/nonexistent-grab-test/yt-dlp".into(),
        "/nonexistent-grab-test/ffmpeg".into(),
    );
    let res = crate::download::tokio_rt().block_on(ensure_tool_versions(&libs));
    assert!(matches!(res, Err(VideoError::MissingLibraries(_))));
}

// ── default video filename ───────────────────────────────────────────

#[test]
fn default_video_filename_appends_mp4() {
    assert_eq!(default_video_filename("Clip"), "Clip.mp4");
    // Untouched otherwise: sanitizing is the intake's job.
    assert_eq!(default_video_filename("a/b"), "a/b.mp4");
}

// ── video format options ─────────────────────────────────────────────

fn test_video(formats: serde_json::Value) -> yt_dlp::model::Video {
    serde_json::from_value(serde_json::json!({
        "id": "x",
        "title": "T",
        "age_limit": 0,
        "live_status": "not_live",
        "playable_in_embed": true,
        "extractor": "generic",
        "extractor_key": "Generic",
        "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
        "formats": formats,
    }))
    .expect("test video must parse")
}

fn test_format_full(
    id: &str,
    vcodec: &str,
    acodec: &str,
    height: Option<u32>,
    filesize: Option<i64>,
    protocol: &str,
    drm: bool,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "format": id,
        "format_id": id,
        "protocol": protocol,
        "ext": "mp4",
        "url": format!("https://cdn.example/{id}"),
        "vcodec": vcodec,
        "acodec": acodec,
        "http_headers": {},
    });
    if drm {
        v["has_drm"] = serde_json::json!(true);
    }
    match (height, filesize) {
        (Some(h), Some(n)) => {
            v["height"] = h.into();
            v["filesize"] = n.into();
        }
        (Some(h), None) => {
            v["height"] = h.into();
        }
        _ => {}
    }
    v
}

#[test]
fn video_format_options_lists_best_per_height() {
    let video = test_video(serde_json::json!([
        test_format_full(
            "v360-vp9",
            "vp9",
            "none",
            Some(360),
            Some(10_000_000),
            "https",
            false
        ),
        test_format_full(
            "v1080-vp9",
            "vp9",
            "none",
            Some(1080),
            Some(200_000_000),
            "https",
            false
        ),
        test_format_full(
            "v1080-avc",
            "avc1.640028",
            "none",
            Some(1080),
            Some(180_000_000),
            "https",
            false
        ),
        test_format_full(
            "v720",
            "avc1.64001f",
            "none",
            Some(720),
            Some(90_000_000),
            "https",
            false
        ),
        test_format_full(
            "v720-av01",
            "av01.0.08M.08",
            "none",
            Some(720),
            Some(60_000_000),
            "https",
            false
        ),
        test_format_full(
            "a-only",
            "none",
            "opus",
            None,
            Some(8_000_000),
            "https",
            false
        ),
        test_format_full(
            "hls",
            "avc1.640028",
            "none",
            Some(1080),
            Some(180_000_000),
            "m3u8_native",
            false
        ),
        test_format_full(
            "drm",
            "avc1.640028",
            "none",
            Some(480),
            Some(40_000_000),
            "https",
            true
        ),
        test_format_full(
            "muxed",
            "avc1.640028",
            "mp4a.40.2",
            Some(720),
            Some(95_000_000),
            "https",
            false
        ),
    ]));
    let opts = video_format_options(&video, true);
    // Newest codec wins each height (AV1 over AVC1 at 720p despite the
    // smaller file, VP9 over AVC1 at 1080p); audio-only, HLS, DRM and
    // muxed never list; tallest first.
    assert_eq!(
        opts.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["v1080-vp9", "v720-av01", "v360-vp9"]
    );
    assert_eq!(opts[0].label, "1080p · vp9 · 200.0 MB");
    assert_eq!(opts[0].height, 1080);
    assert_eq!(opts[1].label, "720p · av01 · 60.0 MB");
}

#[test]
fn unavailable_detail_counts_rejections() {
    let mut no_link = test_format_full("n", "avc1", "none", Some(720), None, "https", false);
    no_link.as_object_mut().unwrap().remove("url");
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full("v", "avc1", "none", Some(720), None, "https", false),
        test_format_full("h", "avc1", "none", Some(720), None, "m3u8_native", false),
        test_format_full("d", "avc1", "none", Some(720), None, "https", true),
        no_link,
    ]))
    .unwrap();
    let message = match VideoError::unavailable_detail(&formats) {
        VideoError::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    };
    assert!(message.contains("4 listed"), "{message}");
    assert!(message.contains("manifest: 1"), "{message}");
    assert!(message.contains("DRM: 1"), "{message}");
    assert!(message.contains("no link: 1"), "{message}");
    assert!(message.contains("video-only: 1"), "{message}");
    let empty = match VideoError::unavailable_detail(&[]) {
        VideoError::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    };
    assert!(empty.contains("listed none"), "{empty}");
}

#[test]
fn hls_selection_prefers_capped_height() {
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
        test_format_full("https", "avc1", "none", Some(720), None, "https", false),
    ]))
    .unwrap();
    // Closest at or above the cap; tallest when capped above all.
    // Selections carry the format id so runners pin the exact variant
    // instead of re-delegating to yt-dlp's sort.
    let sel = select_hls_format(&formats, Some(720)).expect("hls");
    assert_eq!(sel.format_id, "h1080");
    assert_eq!(
        select_hls_format(&formats, Some(2160))
            .expect("hls")
            .format_id,
        "h1080"
    );
    // Best takes the tallest; https entries never select as HLS.
    assert_eq!(
        select_hls_format(&formats, None).expect("hls").format_id,
        "h1080"
    );
    assert!(select_hls_format(&[], Some(720)).is_none());
    assert_eq!(
        find_hls_format(&formats, "h480").expect("pin").format_id,
        "h480"
    );
    assert!(find_hls_format(&formats, "https").is_none());
    assert!(find_hls_format(&formats, "gone").is_none());
}

#[test]
fn explicit_nulls_parse_to_defaults() {
    // TikTok shape: explicit nulls where the model wants maps/arrays.
    let mut nulls = serde_json::json!({
        "id": "tk",
        "title": "T",
        "thumbnails": serde_json::Value::Null,
        "subtitles": serde_json::Value::Null,
        "formats": serde_json::Value::Null,
    });
    sanitize_video_json(&mut nulls);
    let nulls_video: Video = serde_json::from_value(nulls).expect("nulls parse");
    assert!(nulls_video.formats.is_empty());
    assert!(nulls_video.thumbnails.is_empty());
}

#[test]
fn hls_format_spec_names_height_and_pin() {
    // bv* leads so direct muxed files win over lower splits; the
    // trailing /b still catches audio-only pages.
    assert_eq!(hls_format_spec("best", None), "bv*+ba/b");
    assert_eq!(hls_format_spec("1080p", None), "bv*[height<=1080]+ba/b");
    assert_eq!(hls_format_spec("mystery", None), "bv*[height<=1080]+ba/b");
    assert_eq!(hls_format_spec("1080p", Some("hls-99")), "hls-99+ba/b");
    assert_eq!(
        hls_format_spec("1080p", Some("   ")),
        "bv*[height<=1080]+ba/b"
    );
}

#[test]
fn ytdlp_template_parses_absolute_counts() {
    let p = parse_ytdlp_template("[Grab];downloading;47448064;52428800;52428800;1293945;4")
        .expect("progress");
    assert_eq!(p.downloaded, Some(47448064));
    assert_eq!(p.total, Some(52428800));
    assert!((p.speed.expect("speed") - 1293945.0).abs() < 1e-6);
    assert_eq!(p.eta, Some(4));
    // Estimate folds in when the total is unknown; NA means unknown.
    let p = parse_ytdlp_template("[Grab];downloading;100;NA;200;NA;Unknown").expect("progress");
    assert_eq!(p.downloaded, Some(100));
    assert_eq!(p.total, Some(200));
    assert_eq!(p.speed, None);
    assert_eq!(p.eta, None);
    // Finished lines still carry final counts; error lines carry none.
    let p = parse_ytdlp_template("[Grab];finished;52428800;52428800;52428800;NA;0").expect("done");
    assert_eq!(p.downloaded, Some(52428800));
    assert_eq!(parse_ytdlp_template("[Grab];error;NA;NA;NA;NA;NA"), None);
    // Foreign lines never parse — including bare paths, so the
    // after_move sniffer keeps working.
    assert_eq!(parse_ytdlp_template("[download] 45.2% of 50MiB"), None);
    assert_eq!(parse_ytdlp_template("[Merger] Merging"), None);
    assert_eq!(parse_ytdlp_template("[info] x"), None);
    assert_eq!(parse_ytdlp_template("garbage"), None);
    assert_eq!(parse_ytdlp_template("/tmp/grab/abc.mp4"), None);
    assert!(is_ytdlp_merge_line("[Merger] Merging formats"));
    assert!(is_ytdlp_merge_line("[ExtractAudio] Destination"));
    assert!(!is_ytdlp_merge_line("[download] 10% of 1MiB"));
    assert_eq!(
        parse_ytdlp_after_move("/tmp/grab/abc.mp4"),
        Some("/tmp/grab/abc.mp4")
    );
    assert_eq!(parse_ytdlp_after_move("[download] 10%"), None);
    assert_eq!(parse_ytdlp_after_move(""), None);
    // Absolute template counts need no unit table: exact bytes in/out.
    let p = parse_ytdlp_template("[Grab];downloading;805306368;1610612736;1610612736;1048576;768")
        .expect("progress");
    assert_eq!(p.downloaded, Some(805306368));
    assert_eq!(p.total, Some(1610612736));
    assert_eq!(p.eta, Some(768));
}

#[test]
fn piece_marks_cover_prefix_once() {
    let mut marked = 0u64;
    assert!(piece_marks(100, &mut marked, 50).is_empty());
    assert_eq!(piece_marks(100, &mut marked, 250), vec![0, 1]);
    assert!(piece_marks(100, &mut marked, 250).is_empty());
    assert_eq!(
        piece_marks(100, &mut marked, 1000),
        vec![2, 3, 4, 5, 6, 7, 8, 9]
    );
    assert!(piece_marks(0, &mut marked, 1000).is_empty());
}

#[test]
fn picker_lists_hls_gap_heights() {
    let video = test_video(serde_json::json!([
        test_format_full("v720", "avc1", "none", Some(720), None, "https", false),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let opts = video_format_options(&video, true);
    assert_eq!(
        opts.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["h1080", "v720"]
    );
    assert_eq!(opts[0].label, "1080p · HLS");
}

#[test]
fn video_info_carries_live_flag() {
    let mut video = test_video(serde_json::json!([]));
    assert!(!VideoInfo::from(&video, "https://x.com/u/status/1", true).is_live);
    video.is_live = Some(true);
    assert!(VideoInfo::from(&video, "https://x.com/u/status/1", true).is_live);
}

#[cfg(unix)]
#[test]
fn codec_rank_orders_newest_first() {
    assert!(codec_rank("av01.0.08M.08", true) < codec_rank("vp9", true));
    assert!(codec_rank("VP9", true) < codec_rank("hev1.1.6.L93", true));
    assert!(codec_rank("hvc1", true) < codec_rank("avc1.640028", true));
    assert!(codec_rank("avc1.640028", true) < codec_rank("theora", true));
    assert_eq!(codec_rank("av1", true), codec_rank("av01.0.05M.08", true));
}

#[test]
fn codec_rank_compatible_prefers_h264() {
    use yt_dlp::model::selector::VideoCodecPreference;
    assert!(codec_rank("avc1.640028", false) < codec_rank("vp9", false));
    assert!(codec_rank("VP9", false) < codec_rank("hev1.1.6.L93", false));
    assert!(codec_rank("hvc1", false) < codec_rank("av01.0.08M.08", false));
    assert!(codec_rank("av01.0.08M.08", false) < codec_rank("theora", false));
    assert_eq!(
        codec_priority_index("compatible"),
        1,
        "unknown values fall back to newest, not compatible"
    );
    assert_eq!(codec_priority_index("mystery"), 0);
    assert_eq!(codec_priority_value(9), CODEC_PRIORITY_NEWEST);
    assert!(matches!(
        codec_preference(false),
        VideoCodecPreference::AVC1
    ));
    assert!(matches!(codec_preference(true), VideoCodecPreference::AV1));
}

#[test]
fn video_format_options_empty_without_fetchable_video() {
    let video = test_video(serde_json::json!([test_format_full(
        "a-only",
        "none",
        "opus",
        None,
        Some(8_000_000),
        "https",
        false
    ),]));
    assert!(video_format_options(&video, true).is_empty());
}

#[test]
fn find_usable_format_matches() {
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full(
            "137",
            "avc1.640028",
            "none",
            Some(1080),
            Some(100),
            "https",
            false
        ),
        test_format_full(
            "hls",
            "avc1.640028",
            "none",
            Some(1080),
            Some(100),
            "m3u8_native",
            false
        ),
    ]))
    .unwrap();
    assert_eq!(
        find_usable_format(&formats, "137").map(|s| s.format_id),
        Some("137".to_string())
    );
    assert!(find_usable_format(&formats, "nope").is_none());
    assert!(find_usable_format(&formats, "hls").is_none());
}

#[test]
fn video_source_page_carries_format_pin() {
    let src = VideoSource::Page {
        page_url: "https://vimeo.com/99".into(),
        media_url: None,
        expires_at: None,
        quality: "1080p".into(),
        is_live: false,
        video_format_id: Some("137".into()),
    };
    let json = serde_json::to_string(&src).unwrap();
    assert!(json.contains("\"video_format_id\":\"137\""));
    let back: VideoSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back, src);
}

// ── distro package managers ──────────────────────────────────────────

#[test]
fn distro_packages_known_ids() {
    let fedora = "ID=fedora\nNAME=Fedora\n";
    assert_eq!(
        distro_packages(fedora).map(|d| (d.distro, d.yt_dlp, d.ffmpeg)),
        Some((
            "Fedora".to_string(),
            "sudo dnf install yt-dlp".to_string(),
            "sudo dnf install ffmpeg".to_string(),
        ))
    );
    let ubuntu = "ID=ubuntu\nID_LIKE=debian\nNAME=Ubuntu\n";
    assert_eq!(
        distro_packages(ubuntu).map(|d| d.yt_dlp),
        Some("sudo apt install yt-dlp".to_string())
    );
    let arch = "ID=arch\nNAME=Arch\n";
    assert_eq!(
        distro_packages(arch).map(|d| d.ffmpeg),
        Some("sudo pacman -S ffmpeg".to_string())
    );
}

#[test]
fn distro_packages_id_like_fallback() {
    // Unknown derivative riding a known family (e.g. a Ubuntu respin
    // with its own ID) still resolves through ID_LIKE.
    let neon = "ID=neon\nID_LIKE=\"ubuntu debian\"\nNAME=KDE neon\n";
    let found = distro_packages(neon).expect("ID_LIKE fallback");
    assert_eq!(found.distro, "KDE neon");
    assert_eq!(found.yt_dlp, "sudo apt install yt-dlp");
}

#[test]
fn distro_packages_unknown_is_none() {
    assert_eq!(distro_packages(""), None);
    assert_eq!(distro_packages("NAME=No ID here\n"), None);
    assert_eq!(distro_packages("ID=mysteryos\nNAME=Mystery\n"), None);
}

// ── browser cookies ──────────────────────────────────────────────────

#[test]
fn cookies_browser_index_round_trip() {
    assert_eq!(cookies_browser_index("none"), 0);
    assert_eq!(cookies_browser_index("firefox"), 5);
    assert_eq!(cookies_browser_value(5), "firefox");
    assert_eq!(cookies_browser_value(99), "none");
    // Unknown values fall back to off, never to a browser.
    assert_eq!(cookies_browser_index("chromium-beta"), 0);
    assert_eq!(cookies_browser_index(""), 0);
    assert_eq!(cookies_browser_labels().len(), COOKIES_BROWSERS.len());
}

#[test]
fn ytdlp_update_available_compares_releases() {
    assert!(ytdlp_update_available("2026.08.19", "2026.09.01"));
    assert!(!ytdlp_update_available("2026.09.01", "2026.09.01"));
    assert!(!ytdlp_update_available("2026.09.02", "2026.09.01"));
    // Unparseable tags never nag.
    assert!(!ytdlp_update_available("2026.09.01", "nightly"));
    assert!(!ytdlp_update_available("nightly", "2026.09.01"));
    assert!(!ytdlp_update_available("", ""));
}

#[test]
fn sparse_x_com_json_parses_to_video() {
    // x.com omits top-level scalars (live_status, …) and ships sparse
    // nested objects; every one of those used to abort the preview with
    // a missing-field JSON error. Unread arrays are dropped, formats get
    // neutral defaults, and the full model parse succeeds.
    let mut v = serde_json::json!({
        "id": "abc",
        "title": "T",
        "webpage_url": "https://x.com/u/status/abc",
        "duration": 7.66599888973779,
        "view_count": 1200.0,
        "thumbnails": [{"url": "http://e/1.jpg"}],
        "chapters": [{"title": "c"}],
        "tags": ["t"],
        "subtitles": {"en": [{"url": "http://e/s"}]},
        "heatmap": [{"start_time": 0.0}],
        "formats": [
            {
                "url": "https://e/v.mp4",
                "http_headers": {},
                "fragments": [{"url": "http://e/f"}],
                "filesize": 180000000.0,
            },
            {"format_id": "hls-1", "protocol": "m3u8_native", "url": "https://e/m.m3u8"},
        ],
    });
    sanitize_video_json(&mut v);
    let video: Video = serde_json::from_value(v).expect("sparse page parses");
    assert_eq!(video.live_status, "");
    assert_eq!(video.age_limit, 0);
    assert_eq!(video.duration, Some(7));
    assert!(!video.playable_in_embed);
    assert!(video.thumbnails.is_empty());
    assert!(video.chapters.is_empty());
    assert_eq!(video.formats.len(), 2);
    assert_eq!(video.formats[0].format_id, "");
    assert_eq!(video.formats[1].format_id, "hls-1");
    // Missing top-level scalars get defaults too.
    let mut bare = serde_json::json!({"formats": []});
    sanitize_video_json(&mut bare);
    let bare: Video = serde_json::from_value(bare).expect("bare page parses");
    assert_eq!(bare.id, "");
    assert_eq!(bare.extractor_info.extractor, "");
}

#[test]
fn cookies_browser_spec_falls_back_to_bare_name() {
    // Hermetic: point the host config lookup at an empty dir so no real
    // browser profile on the dev machine leaks into the assertion. With
    // nothing on disk the spec stays the bare name and yt-dlp falls back
    // to its own $HOME-relative lookup (correct outside Flatpak).
    let dir = std::env::temp_dir().join(format!("grab-nocookies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".config")).unwrap();
    let _env = ScopedHostConfig::apply(&dir.join(".config"));
    assert_eq!(cookies_browser_spec("chrome"), Some("chrome".to_string()));
    assert_eq!(cookies_browser_spec("firefox"), Some("firefox".to_string()));
    assert_eq!(cookies_browser_spec("brave"), Some("brave".to_string()));
    assert_eq!(cookies_browser_spec("none"), None);
    assert_eq!(cookies_browser_spec(""), None);
    assert_eq!(cookies_browser_spec("mystery"), None);
    drop(_env);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scoped HOST_XDG_CONFIG_HOME override, restored on drop. Serial suite
/// only: the environment is process-global (same precedent as ScopedEnv).
/// Setting it also pins the derived home dir (its parent), so profile
/// resolution stays hermetic without touching the real home.
struct ScopedHostConfig {
    saved: Option<std::ffi::OsString>,
}

impl ScopedHostConfig {
    fn apply(config_home: &std::path::Path) -> Self {
        let saved = std::env::var_os("HOST_XDG_CONFIG_HOME");
        // SAFETY: serial suite; restored on drop below.
        unsafe {
            std::env::set_var("HOST_XDG_CONFIG_HOME", config_home);
        }
        Self { saved }
    }
}

impl Drop for ScopedHostConfig {
    fn drop(&mut self) {
        // SAFETY: same serial-suite context as apply().
        unsafe {
            match &self.saved {
                Some(v) => std::env::set_var("HOST_XDG_CONFIG_HOME", v),
                None => std::env::remove_var("HOST_XDG_CONFIG_HOME"),
            }
        }
    }
}

#[test]
fn browser_profile_prefers_default_over_numbered() {
    let dir = std::env::temp_dir().join(format!("grab-chromium-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for profile in ["Default", "Profile 1"] {
        let p = dir.join("BraveSoftware/Brave-Browser").join(profile);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("Cookies"), b"sqlite").unwrap();
    }
    let found = browser_profile_dir_in(&dir, &dir, "brave").expect("brave resolves");
    assert_eq!(found, dir.join("BraveSoftware/Brave-Browser/Default"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn browser_profile_unknown_browser_resolves_nothing() {
    let dir = std::env::temp_dir().join(format!("grab-nobrowser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(browser_profile_dir_in(&dir, &dir, "mystery"), None);
    assert_eq!(browser_profile_dir_in(&dir, &dir, "none"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn firefox_profile_prefers_default_section() {
    let dir = std::env::temp_dir().join(format!("grab-firefox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let base = dir.join(".mozilla/firefox");
    for profile in ["aaaa1111.other", "bbbb2222.main"] {
        let p = base.join(profile);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cookies.sqlite"), b"sqlite").unwrap();
    }
    std::fs::write(
        base.join("profiles.ini"),
        "[Profile0]\nName=other\nIsRelative=1\nPath=aaaa1111.other\n\n\
         [Profile1]\nName=main\nIsRelative=1\nPath=bbbb2222.main\nDefault=1\n",
    )
    .unwrap();
    let found = browser_profile_dir_in(&dir.join(".config"), &dir, "firefox").expect("ff resolves");
    assert_eq!(found, base.join("bbbb2222.main"));
    let spec = {
        let _env = ScopedHostConfig::apply(&dir.join(".config"));
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        cookies_browser_spec("firefox")
    };
    assert_eq!(spec, Some(format!("firefox:{}", found.display())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cookies_browser_spec_pins_chromium_profile_path() {
    let dir = std::env::temp_dir().join(format!("grab-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = dir.join(".config/BraveSoftware/Brave-Browser/Default");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("Cookies"), b"sqlite").unwrap();
    let _env = ScopedHostConfig::apply(&dir.join(".config"));
    assert_eq!(
        cookies_browser_spec("brave"),
        Some(format!("brave:{}", profile.display()))
    );
    // A browser with no profile on disk keeps the bare-name fallback.
    assert_eq!(cookies_browser_spec("chrome"), Some("chrome".to_string()));
    drop(_env);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── tool search order ────────────────────────────────────────────────

/// Scoped PATH + XDG_DATA_HOME override, restored on drop. Serial suite
/// only: the environment is process-global (same precedent as the queue
/// file and NoVideoTools helpers).
struct ScopedEnv {
    path: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
}

impl ScopedEnv {
    fn apply(path: &str, xdg: &std::path::Path) -> Self {
        let saved = Self {
            path: std::env::var_os("PATH"),
            xdg: std::env::var_os("XDG_DATA_HOME"),
        };
        // SAFETY: serial suite; restored on drop below.
        unsafe {
            std::env::set_var("PATH", path);
            std::env::set_var("XDG_DATA_HOME", xdg);
        }
        saved
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        // SAFETY: same serial-suite context as apply().
        unsafe {
            match &self.path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            match &self.xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }
}

fn fake_executable(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn user_installed_tools_win_over_bundle() {
    // User dir holds our own binaries, PATH leads nowhere (in particular
    // the Flatpak bundle dir is absent on a dev host): resolution must
    // find the user copies. On a Flatpak system this same order lets a
    // user Update override the bundle.
    let dir = std::env::temp_dir().join(format!("grab-userlibs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let libs = dir.join("xdg").join("grab").join("libs");
    std::fs::create_dir_all(&libs).unwrap();
    fake_executable(&libs, "yt-dlp");
    fake_executable(&libs, "ffmpeg");
    let _env = ScopedEnv::apply("/nonexistent-grab-test", &dir.join("xdg"));
    let found = resolve_libraries().expect("user tools resolve");
    assert_eq!(found.youtube, libs.join("yt-dlp"));
    assert_eq!(found.ffmpeg, libs.join("ffmpeg"));
    drop(_env);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── stream planning (selection gating) ───────────────────────────────

/// TikTok shape: sparse mp4 with neither codec field (typed Unknown,
/// invisible to every crate selector) beside a separate music track.
fn tiktok_like_video() -> yt_dlp::model::Video {
    let sparse_mp4 = serde_json::json!({
        "format": "dl",
        "format_id": "dl",
        "protocol": "https",
        "ext": "mp4",
        "url": "https://cdn.example/dl.mp4",
        "http_headers": {},
    });
    test_video(serde_json::json!([
        sparse_mp4,
        test_format_full("music", "none", "mp4a.40.2", None, None, "https", false),
    ]))
}

/// x.com shape: muxed direct mp4s (AudioVideo type, invisible to the
/// video-only selector) beside HLS variants.
fn x_like_video() -> yt_dlp::model::Video {
    test_video(serde_json::json!([
        test_format_full(
            "http-320",
            "avc1.64001f",
            "mp4a.40.2",
            Some(320),
            None,
            "https",
            false
        ),
        test_format_full(
            "http-720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
        test_format_full(
            "hls-720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "m3u8_native",
            false
        ),
    ]))
}

#[test]
fn plan_adopts_unknown_video_despite_separate_audio() {
    // The TikTok gating bug: both fallbacks keyed off audio absence, so
    // the music track suppressed them and only the music survived.
    let video = tiktok_like_video();
    let plan = plan_streams(&video, "1080p", None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "dl");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_pinned_hls_wins_over_muxed_adoption() {
    // The x.com shadowing bug: the pin was dropped by the HTTPS-only
    // lookup and the muxed adoption then vetoed the HLS path.
    let video = x_like_video();
    let plan = plan_streams(&video, "720p", Some("hls-720"), true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    let hls = plan.hls_sel.expect("pinned hls");
    assert_eq!(hls.height, Some(720));
}

#[test]
fn plan_muxed_only_still_adopts_without_pin() {
    // No pin, no splits: the muxed file adopts as before (precedence
    // over the HLS preset is unchanged) — but at the requested height,
    // not first-in-extractor-order.
    let video = x_like_video();
    let plan = plan_streams(&video, "1080p", None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_muxed_adoption_honors_height_cap() {
    // First-in-order used to win regardless of quality (x.com lists
    // ascending, so Best match downloaded the lowest). Now the cap
    // picks smallest-at-or-above, tallest when nothing qualifies.
    let video = test_video(serde_json::json!([
        test_format_full(
            "m320",
            "avc1.64001f",
            "mp4a.40.2",
            Some(320),
            None,
            "https",
            false
        ),
        test_format_full(
            "m720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
        test_format_full(
            "m1080",
            "avc1.64001f",
            "mp4a.40.2",
            Some(1080),
            None,
            "https",
            false
        ),
    ]));
    for (quality, want) in [
        ("best", "m1080"),
        ("1080p", "m1080"),
        ("720p", "m720"),
        ("480p", "m720"),
    ] {
        let plan = plan_streams(&video, quality, None, true, 1);
        assert_eq!(
            plan.audio_sel.expect("adopted").format_id,
            want,
            "quality {quality}"
        );
    }
}

#[test]
fn plan_stale_hls_pin_degrades_to_muxed_adoption() {
    // A vanished HLS pin behaves like no pin: preset, then adoption
    // (at the dialog-picked height, not the lowest listing).
    let video = x_like_video();
    let plan = plan_streams(&video, "1080p", Some("gone"), true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_splits_stay_split() {
    let video = test_video(serde_json::json!([
        test_format_full("v", "avc1.640028", "none", Some(1080), None, "https", false),
        test_format_full("a", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let plan = plan_streams(&video, "1080p", None, true, 1);
    assert_eq!(plan.video_sel.expect("video").format_id, "v");
    assert_eq!(plan.audio_sel.expect("audio").format_id, "a");
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_hls_preset_still_serves_hls_only_pages() {
    let video = test_video(serde_json::json!([
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]));
    let plan = plan_streams(&video, "720p", None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    assert_eq!(plan.hls_sel.expect("preset hls").height, Some(1080));
}

// ── quality for height ───────────────────────────────────────────────

#[test]
fn quality_for_height_buckets() {
    assert_eq!(quality_for_height(2160), "2160p");
    assert_eq!(quality_for_height(1440), "1440p");
    assert_eq!(quality_for_height(1080), "1080p");
    assert_eq!(quality_for_height(720), "720p");
    assert_eq!(quality_for_height(480), "480p");
    // Odd extractor heights round to the closest bucket (ties up), and
    // everything outside clamps — so the result is always a recognized
    // stored value, never a silent 1080p fallback.
    assert_eq!(quality_for_height(632), "720p");
    assert_eq!(quality_for_height(900), "1080p");
    assert_eq!(quality_for_height(100), "480p");
    assert_eq!(quality_for_height(5000), "2160p");
    for h in [240, 360, 480, 720, 1080, 1440, 2160] {
        assert!(
            VIDEO_QUALITY_VALUES.contains(&quality_for_height(h)),
            "bucket for {h}"
        );
    }
}

// ── HLS candidate ordering ───────────────────────────────────────────

#[test]
fn hls_selection_ignores_extractor_order() {
    // Tallest first (the order some extractors emit): the cap must still
    // resolve to the smallest height at or above it, not the first
    // qualifying entry.
    let formats: Vec<yt_dlp::model::format::Format> = serde_json::from_value(serde_json::json!([
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h720",
            "avc1",
            "mp4a.40.2",
            Some(720),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
    ]))
    .unwrap();
    assert_eq!(
        select_hls_format(&formats, Some(720))
            .expect("hls")
            .format_id,
        "h720"
    );
    assert_eq!(
        select_hls_format(&formats, Some(480))
            .expect("hls")
            .format_id,
        "h480"
    );
    // Above every variant still takes the tallest.
    assert_eq!(
        select_hls_format(&formats, Some(2160))
            .expect("hls")
            .format_id,
        "h1080"
    );
}

// ── direct ffmpeg merge ──────────────────────────────────────────────

#[test]
fn merge_audio_codec_matrix() {
    // Mirrors the crate's audio_codec_for_mux without its builder.
    assert_eq!(merge_audio_codec("m4a", "mp4"), "copy");
    assert_eq!(merge_audio_codec("aac", "mp4"), "copy");
    assert_eq!(merge_audio_codec("webm", "webm"), "copy");
    assert_eq!(merge_audio_codec("opus", "webm"), "copy");
    assert_eq!(merge_audio_codec("mka", "mka"), "copy");
    assert_eq!(merge_audio_codec("webm", "mkv"), "copy");
    // Opus into MP4 (the YouTube split case) re-encodes to AAC.
    assert_eq!(merge_audio_codec("webm", "mp4"), "aac");
    assert_eq!(merge_audio_codec("ogg", "mp4"), "aac");
}

#[test]
fn merge_argv_maps_audio_first() {
    let argv = merge_argv(
        std::path::Path::new("/tmp/st/audio.webm"),
        std::path::Path::new("/tmp/st/video.mp4"),
        std::path::Path::new("/tmp/st/muxed.mp4"),
        "Some Title",
    );
    let inputs: Vec<&str> = argv
        .windows(2)
        .filter(|w| w[0] == "-i")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(inputs, ["/tmp/st/audio.webm", "/tmp/st/video.mp4"]);
    assert!(argv.windows(2).any(|w| w == ["-map", "0:a"]));
    assert!(argv.windows(2).any(|w| w == ["-map", "1:v"]));
    assert!(argv.windows(2).any(|w| w == ["-c:v", "copy"]));
    assert!(argv.windows(2).any(|w| w == ["-c:a", "aac"]));
    assert!(argv.contains(&"title=Some Title".to_string()));
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "/tmp/st/muxed.mp4");
}

fn fake_ffmpeg_merge(dir: &std::path::Path, fail: bool) -> std::path::PathBuf {
    let bin = dir.join("fake-ffmpeg");
    std::fs::write(
        &bin,
        if fail {
            r#"#!/bin/sh
echo "[out#0/mp4] Invalid data found when processing input" >&2
exit 1
"#
        } else {
            r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--" ]; then out="$a"; fi
    prev="$a"
done
echo "$@" >> "$out.argv.log"
printf 'merged' > "$out"
exit 0
"#
        },
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}
// ── binary part downloads ────────────────────────────────────────────

fn part_test_job() -> VideoJob {
    VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        quality: "720p".into(),
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
        tries: 3,
        timeout_secs: 60,
        user_agent: "Grab-test/1.0".into(),
        video_format_id: None,
        is_live: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        proxy: None,
    }
}

#[test]
fn part_argv_pins_format_output_and_page() {
    let job = part_test_job();
    let out = std::path::Path::new("/tmp/staging/video.mp4");
    let argv = part_download_argv(
        &job,
        "hls-720",
        out,
        false,
        std::path::Path::new("/usr/bin/ffmpeg"),
    );
    // Exact id, exact output, page URL last behind `--`.
    let f = argv.iter().position(|a| a == "-f").expect("has -f");
    assert_eq!(argv[f + 1], "hls-720");
    let o = argv.iter().position(|a| a == "-o").expect("has -o");
    assert_eq!(argv[o + 1], "/tmp/staging/video.mp4");
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "https://x.com/u/status/1");
    assert!(argv.contains(&"--newline".to_string()));
    assert!(argv.contains(&"--no-playlist".to_string()));
    let r = argv.iter().position(|a| a == "--retries").expect("retries");
    assert_eq!(argv[r + 1], "3");
    // User agent passes through; no browser cookies configured.
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--user-agent" && w[1] == "Grab-test/1.0")
    );
    assert!(!argv.iter().any(|a| a.starts_with("--cookies-from-browser")));
}

#[test]
fn part_argv_forwards_browser_cookies() {
    let mut job = part_test_job();
    job.cookies_browser = "firefox".into();
    let argv = part_download_argv(
        &job,
        "dl",
        std::path::Path::new("/tmp/staging/dl.mp4"),
        false,
        std::path::Path::new("/usr/bin/ffmpeg"),
    );
    assert!(
        argv.iter()
            .any(|a| a.starts_with("--cookies-from-browser=firefox"))
    );
}

#[test]
fn part_fallback_specs() {
    assert_eq!(part_fallback_spec("720p", true, false), "bv*[height<=720]");
    assert_eq!(part_fallback_spec("best", true, false), "bv*");
    assert_eq!(part_fallback_spec("720p", false, true), "ba/b");
    // Adopted single files degrade to best-single, never a bare audio
    // track; genuine audio legs prefer audio.
    assert_eq!(part_fallback_spec("720p", false, false), "b");
}

#[test]
fn format_unavailable_detection() {
    assert!(is_format_unavailable(
        "ERROR: [Video] 1: Requested format is not available"
    ));
    assert!(!is_format_unavailable(
        "ERROR: [Video] 1: Unable to download"
    ));
    assert!(!is_format_unavailable("HTTP Error 403: Forbidden"));
}

// ── binary part plumbing (fake yt-dlp) ───────────────────────────────

/// Fake yt-dlp: logs argv beside the output, then either fails stale
/// ids with the real unavailability message or emits one `--newline`
/// progress line and writes 4 bytes to the `-o` path.
fn fake_ytdlp(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
spec=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    if [ "$prev" = "-f" ]; then spec="$a"; fi
    prev="$a"
done
echo "$@" >> "$out.argv.log"
if [ "$spec" = "gone-id" ]; then
    echo "ERROR: [Video] 1: Requested format is not available" >&2
    exit 1
fi
echo "[Grab];downloading;4;4;4;1000;0"
echo "[Grab];finished;4;4;4;NA;0"
printf 'data' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn merge_binary_combines_and_reports_failure() {
    let dir = std::env::temp_dir().join(format!("grab-fakemerge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ffmpeg_merge(&dir, false);
    let (apart, vpart, out) = (dir.join("a.webm"), dir.join("v.mp4"), dir.join("muxed.mp4"));
    std::fs::write(&apart, b"a").unwrap();
    std::fs::write(&vpart, b"v").unwrap();
    let res = crate::download::tokio_rt().block_on(run_merge_ffmpeg(
        &fake,
        &apart,
        &vpart,
        &out,
        "T",
        std::time::Duration::from_secs(30),
    ));
    assert!(matches!(res, Ok(())), "got {res:?}");
    assert_eq!(std::fs::read(&out).unwrap(), b"merged");

    let fail = fake_ffmpeg_merge(&dir, true);
    let res = crate::download::tokio_rt().block_on(run_merge_ffmpeg(
        &fail,
        &apart,
        &vpart,
        &dir.join("muxed2.mp4"),
        "T",
        std::time::Duration::from_secs(30),
    ));
    match res {
        Err(e) => assert!(
            e.to_string().contains("Invalid data found"),
            "real ffmpeg message surfaces: {e}"
        ),
        ok => panic!("expected combine error, got {ok:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ── extraction spawn (no crate executor) ─────────────────────────────

#[test]
fn fetch_video_page_parses_dump_json() {
    // Fake yt-dlp emitting --dump-single-json bytes: proves the direct
    // spawn, concurrent drain and parse path without network.
    let dir = std::env::temp_dir().join(format!("grab-fakeextract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("info.json"),
        serde_json::json!({
            "id": "abc",
            "title": "T",
            "age_limit": 0,
            "live_status": "not_live",
            "playable_in_embed": true,
            "extractor": "generic",
            "extractor_key": "Generic",
            "_version": {"version": "2026.08.19", "repository": "yt-dlp"},
            "formats": [],
        })
        .to_string(),
    )
    .unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\ncat \"$(dirname \"$0\")/info.json\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let video = crate::download::tokio_rt()
        .block_on(fetch_video_page(
            &bin,
            "https://example.com/v",
            "none",
            std::time::Duration::from_secs(30),
            None,
        ))
        .expect("fake extract parses");
    assert_eq!(video.id, "abc");
    assert!(video.formats.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn part_binary_download_reports_progress() {
    let dir = std::env::temp_dir().join(format!("grab-fakeyt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let out = dir.join("video.mp4");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let report = {
        let seen = std::sync::Arc::clone(&seen);
        std::sync::Arc::new(move |d: u64, t: u64| {
            seen.lock().unwrap().push((d, t));
        }) as std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>
    };
    let job = part_test_job();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_part_ytdlp(
        &fake,
        &job,
        "v123",
        "bv*",
        &out,
        false,
        std::path::Path::new("/usr/bin/ffmpeg"),
        report,
        &mut abort_rx,
        std::time::Duration::from_secs(30),
    ));
    assert!(matches!(res, Ok(Some(()))), "got {res:?}");
    assert_eq!(std::fs::read(&out).unwrap(), b"data");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|(d, t)| *d == 4 && *t == 4),
        "progress reported: {seen:?}"
    );
    let logged = std::fs::read_to_string(dir.join("video.mp4.argv.log")).unwrap();
    assert!(logged.contains("-f v123"), "{logged}");
    assert!(logged.contains("-- https://x.com/u/status/1"), "{logged}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn part_binary_retries_stale_id_with_fallback_spec() {
    let dir = std::env::temp_dir().join(format!("grab-fakeyt-fb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let out = dir.join("audio.m4a");
    let report =
        std::sync::Arc::new(|_: u64, _: u64| {}) as std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>;
    let job = part_test_job();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_part_ytdlp(
        &fake,
        &job,
        "gone-id",
        "ba/b",
        &out,
        false,
        std::path::Path::new("/usr/bin/ffmpeg"),
        report,
        &mut abort_rx,
        std::time::Duration::from_secs(30),
    ));
    assert!(matches!(res, Ok(Some(()))), "got {res:?}");
    assert_eq!(std::fs::read(&out).unwrap(), b"data");
    let logged = std::fs::read_to_string(dir.join("audio.m4a.argv.log")).unwrap();
    let attempts: Vec<&str> = logged.lines().collect();
    assert_eq!(attempts.len(), 2, "{logged}");
    assert!(attempts[0].contains("-f gone-id"), "{logged}");
    assert!(attempts[1].contains("-f ba/b"), "{logged}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── shared yt-dlp identity argv ──────────────────────────────────────

#[test]
fn identity_args_order_and_trim() {
    // Cookies, trimmed UA, then `--` + page: identical for every spawn.
    let argv = ytdlp_identity_args("none", Some("  Grab/1  "), "https://x.com/u/status/1");
    assert_eq!(
        argv,
        vec![
            "--user-agent".to_string(),
            "Grab/1".to_string(),
            "--".to_string(),
            "https://x.com/u/status/1".to_string(),
        ]
    );
    // Blank UA is omitted, never sent empty.
    let argv = ytdlp_identity_args("none", None, "https://x.com/u/status/1");
    assert_eq!(
        argv,
        vec!["--".to_string(), "https://x.com/u/status/1".to_string()]
    );
}

// ── best-overall muxed vs HLS ────────────────────────────────────────

/// Direct muxed files top out below the tallest HLS variant (the
/// x.com shape that Best match undershot before the override).
fn muxed_below_hls_video() -> yt_dlp::model::Video {
    test_video(serde_json::json!([
        test_format_full(
            "m320",
            "avc1.64001f",
            "mp4a.40.2",
            Some(320),
            None,
            "https",
            false
        ),
        test_format_full(
            "m720",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
        test_format_full(
            "h480",
            "avc1",
            "mp4a.40.2",
            Some(480),
            None,
            "m3u8_native",
            false
        ),
        test_format_full(
            "h1080",
            "avc1",
            "mp4a.40.2",
            Some(1080),
            None,
            "m3u8_native",
            false
        ),
    ]))
}

#[test]
fn plan_best_prefers_taller_hls_over_muxed() {
    let video = muxed_below_hls_video();
    let plan = plan_streams(&video, "best", None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none(), "adoption yields to the variant");
    assert_eq!(plan.hls_sel.expect("hls wins").height, Some(1080));
}

#[test]
fn plan_cap_blocks_taller_hls() {
    // Capped 720p: the 1080p variant exceeds the cap, so the direct
    // 720p file stands.
    let video = muxed_below_hls_video();
    let plan = plan_streams(&video, "720p", None, true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "m720");
    assert!(plan.hls_sel.is_none());
    // Capped 1080p: the variant is within cap and taller, so it wins.
    let plan = plan_streams(&video, "1080p", None, true, 1);
    assert!(plan.audio_sel.is_none());
    assert_eq!(plan.hls_sel.expect("hls wins").height, Some(1080));
}

#[test]
fn plan_tie_keeps_direct_muxed() {
    // Equal heights: direct-file precedence is unchanged.
    let video = x_like_video();
    let plan = plan_streams(&video, "best", None, true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.hls_sel.is_none());
}

// ── default combo selection ──────────────────────────────────────────

fn test_options() -> Vec<VideoFormatOption> {
    [1080u32, 720, 480]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            height: *h,
        })
        .collect()
}

#[test]
fn default_quality_index_preselects() {
    let opts = test_options();
    // Best (and empty listings) stay on the first row, which lists
    // tallest first.
    assert_eq!(default_quality_index(&opts, "best"), 0);
    assert_eq!(default_quality_index(&[], "720p"), 0);
    // Otherwise the closest listed height wins, 0-based.
    assert_eq!(default_quality_index(&opts, "1080p"), 0);
    assert_eq!(default_quality_index(&opts, "720p"), 1);
    assert_eq!(default_quality_index(&opts, "480p"), 2);
    // Between buckets the nearer height wins, ties go taller.
    assert_eq!(default_quality_index(&opts, "2160p"), 0);
    // Unknown stored values degrade like the extractor selector (1080p).
    assert_eq!(default_quality_index(&opts, "mystery"), 0);
    // Exact ties (odd extractor heights equidistant from the cap) go taller.
    let odd = [840u32, 600]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            height: *h,
        })
        .collect::<Vec<_>>();
    assert_eq!(default_quality_index(&odd, "720p"), 0);
}

// ── yt-dlp live capture ──────────────────────────────────────────────

fn live_test_job() -> VideoJob {
    VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        quality: "720p".into(),
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
        tries: 3,
        timeout_secs: 60,
        user_agent: "Grab-test/1.0".into(),
        video_format_id: None,
        is_live: true,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        proxy: None,
    }
}

#[test]
fn live_argv_pins_planner_id_in_mpegts() {
    let job = live_test_job();
    let out = std::path::Path::new("/tmp/staging/live.mp4");
    // The planner-resolved id rides along verbatim: yt-dlp's sort never
    // gets a second vote (its ie_pref/quality/source tiebreaks can
    // shadow height).
    let argv = live_capture_argv(&job, "h1080", out);
    let f = argv.iter().position(|a| a == "-f").expect("has -f");
    assert_eq!(argv[f + 1], "h1080+ba/b");
    assert!(argv.contains(&"--hls-use-mpegts".to_string()));
    assert!(
        argv.windows(2)
            .any(|w| w == ["--fragment-retries", "infinite"])
    );
    // No live-from-start (record-now means the live edge) and no
    // unbounded wait loop.
    assert!(!argv.iter().any(|a| a == "--live-from-start"));
    assert!(!argv.iter().any(|a| a == "--wait-for-video"));
    let o = argv.iter().position(|a| a == "-o").expect("has -o");
    assert_eq!(argv[o + 1], "/tmp/staging/live.mp4");
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "https://x.com/u/status/1");
    // Pins ride along in the capture spec.
    let mut pinned = live_test_job();
    pinned.video_format_id = Some("h720".into());
    let argv = live_capture_argv(&pinned, "h720", out);
    let f = argv.iter().position(|a| a == "-f").expect("has -f");
    assert_eq!(argv[f + 1], "h720+ba/b");
}

#[test]
fn live_remux_argv_copies_with_fixup() {
    // Map everything, stream-copy, ADTS fixup, faststart.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.mp4.part"),
        std::path::Path::new("/tmp/st/final.mp4"),
        true,
    );
    assert!(argv.windows(2).any(|w| w == ["-map", "0"]));
    assert!(argv.windows(2).any(|w| w == ["-c", "copy"]));
    assert!(argv.windows(2).any(|w| w == ["-bsf:a", "aac_adtstoasc"]));
    assert!(argv.windows(2).any(|w| w == ["-movflags", "+faststart"]));
    assert!(!argv.iter().any(|a| a == "-vn"));
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "/tmp/st/final.mp4");
    // The bare retry drops the fixup.
    let argv = live_remux_argv(
        std::path::Path::new("/tmp/st/live.mp4.part"),
        std::path::Path::new("/tmp/st/final.mp4"),
        false,
    );
    assert!(!argv.iter().any(|a| a == "-bsf:a"));
}

/// Fake yt-dlp for live: emits one progress line, writes the `.part`
/// shell (or fails barren for the stale-URL case).
fn fake_ytdlp_live(dir: &std::path::Path, fail: bool) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live");
    std::fs::write(
        &bin,
        if fail {
            "#!/bin/sh\necho \"ERROR: [Video] 1: Got error 404\" >&2\nexit 1\n"
        } else {
            r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "[Grab];downloading;3;100;100;1000;5"
printf 'tsbytes' > "$out.part"
exit 0
"#
        },
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Fake ffmpeg remux: copies its `-i` input to the trailing output.
fn fake_ffmpeg_copy(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ffmpeg-copy");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
input=""
prev=""
last=""
for a in "$@"; do
    if [ "$prev" = "-i" ]; then input="$a"; fi
    prev="$a"
    last="$a"
done
cat "$input" > "$last"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_adopts_part_and_remuxes() {
    let dir = std::env::temp_dir().join(format!("grab-fakelive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    // Natural end (fake exits 0) with a `.part` shell: adopted,
    // remuxed, delivered.
    let res = crate::download::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"tsbytes");
    assert!(!staging.exists(), "staging cleaned");
    // The row must leave Resolving the moment capture starts, even
    // before any bytes flow.
    let phases: Vec<String> = {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::download::EngineMsg::Phase(p) = msg {
                out.push(p);
            }
        }
        out
    };
    assert!(
        phases.iter().any(|p| p.contains("Recording")),
        "phases seen: {phases:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_empty_fails_with_detail() {
    let dir = std::env::temp_dir().join(format!("grab-fakelive-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, true);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert!(e.to_string().contains("404"), "yt-dlp line surfaces: {e}"),
        ok => panic!("expected failure, got {ok:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for an abortable capture: records a partial immediately,
/// then sleeps (simulating an ongoing live edge) until killed.
fn fake_ytdlp_slow(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-slow");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "[Grab];downloading;1;100;100;1000;5"
printf 'partial' > "$out.part"
sleep 60
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_capture_stale_staging_never_adopts() {
    // A crashed run's leftover must not pose as a fresh capture: the
    // attempt wipes staging first, so a barren run fails instead of
    // delivering stale bytes.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("live.mp4"), b"stale").unwrap();
    let fake_yt = fake_ytdlp_live(&dir, true);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(res.is_err(), "barren run must fail, got {res:?}");
    assert!(!job.dest.exists(), "stale bytes must not deliver");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_refuses_existing_dest() {
    // Overwrite pre-flight (Parabolic parity): a finished file already
    // at dest makes the final rename claim impossible, so the capture
    // must refuse before recording — no spawn, no part shell, and the
    // pump's DEST_EXISTS path requeues under a fresh name.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-ow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    std::fs::write(&job.dest, b"already").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::download::DEST_EXISTS, "{e}"),
        ok => panic!("expected pre-flight refusal, got {ok:?}"),
    }
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "no capture shell: the fake must never have run"
    );
    assert!(!staging.exists(), "staging untouched by the refusal");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_capture_abort_adopts_partial() {
    // User stop mid-capture: the kill lands, the recorded partial is
    // adopted and remuxed, the row completes Done.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_slow(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let res = crate::download::tokio_rt().block_on(async {
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = abort_tx.send(());
        });
        run_live_ytdlp(
            &fake_yt,
            &fake_ff,
            &staging,
            &job,
            "h1080",
            abort_rx,
            std::time::Duration::from_secs(30),
            tx,
        )
        .await
    });
    assert!(
        matches!(res, Ok(Some(_))),
        "abort must complete Done, got {res:?}"
    );
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"partial");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for VOD HLS: logs argv, expands the `-o` template's
/// `%(ext)s`, writes bytes there (what discover adopts).
fn fake_ytdlp_hls(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
echo "$@" >> "$(dirname "$out").argv.log"
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn vod_hls_pins_planner_variant_id() {
    // Best-match with no dialog pin must still download the planner's
    // pick verbatim — never yt-dlp's sort order.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls(&dir);
    let staging = dir.join("staging");
    let job = VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        quality: "best".into(),
        dest: dir.join("v.mp4"),
        tries: 3,
        timeout_secs: 60,
        user_agent: "Grab-test/1.0".into(),
        video_format_id: None,
        is_live: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        proxy: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"hlsbytes");
    // argv log lands next to the dest-dir template (`v.hls.%(ext)s`):
    // parts download beside the finished file, never into staging.
    // (The fake logs to dirname(-o) + ".argv.log", i.e. beside `dir`.)
    let logged = std::fs::read_to_string(dir.with_extension("argv.log")).unwrap();
    assert!(logged.contains("v.hls."), "{logged}");
    let f = logged
        .split_whitespace()
        .position(|a| a == "-f")
        .expect("has -f");
    assert_eq!(
        logged.split_whitespace().nth(f + 1).expect("spec"),
        "h1080+ba/b",
        "{logged}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vod_hls_refuses_existing_dest() {
    // Overwrite pre-flight for the VOD HLS path: an existing finished
    // file must be refused before yt-dlp transfers anything, so the
    // pump requeues under a fresh name instead of downloading the whole
    // stream into a doomed claim.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-ow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls(&dir);
    let staging = dir.join("staging");
    let job = VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        quality: "best".into(),
        dest: dir.join("v.mp4"),
        tries: 3,
        timeout_secs: 60,
        user_agent: "Grab-test/1.0".into(),
        video_format_id: None,
        is_live: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: None,
        proxy: None,
    };
    std::fs::write(&job.dest, b"already").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::download::DEST_EXISTS, "{e}"),
        ok => panic!("expected pre-flight refusal, got {ok:?}"),
    }
    assert!(
        !dir.with_extension("argv.log").exists(),
        "no argv log: the fake must never have run"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn format_selection_line_detection() {
    assert!(is_format_selection_line(
        "[info] 1234567890: Downloading 1 format(s): h1080+ba/b"
    ));
    assert!(is_format_selection_line(
        "[info] x: Downloading 2 format(s): 399+251"
    ));
    assert!(!is_format_selection_line(
        "[download] 100% of 10MiB in 00:01"
    ));
    assert!(!is_format_selection_line("[info] Downloading video info"));
    assert!(!is_format_selection_line(""));
}

#[test]
fn format_lines_survive_split_reads() {
    // A selection line split across 4 KiB reads still traces (no
    // panic, no loss): drive the scanner the way the pumps do.
    let mut pending = String::new();
    trace_format_lines(&mut pending, b"[info] abc: Download");
    assert_eq!(pending, "[info] abc: Download");
    trace_format_lines(&mut pending, b"ing 1 format(s): h1\n[download] x\n");
    assert!(pending.is_empty(), "complete lines drain: {pending:?}");
    trace_format_lines(&mut pending, b"");
    assert!(pending.is_empty());
}

#[test]
fn hls_format_spec_rejects_hostile_pins() {
    // A hostile pinned id must not widen the yt-dlp format set: `,`,
    // `[]` and `()` all change set semantics. Rejected pins fall
    // through to the height rule instead of failing the row.
    assert_eq!(
        hls_format_spec("1080p", Some("hls-99,hls-720")),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(
        hls_format_spec("1080p", Some("best[height=1080]")),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(
        hls_format_spec("1080p", Some("(hls-99)")),
        "bv*[height<=1080]+ba/b"
    );
    // Dashes, underscores and colons are legitimate extractor id chars.
    assert_eq!(
        hls_format_spec("1080p", Some("hls-720_p:1")),
        "hls-720_p:1+ba/b"
    );
}

#[test]
fn picker_label_sanitizes_remote_codec() {
    // Extractor-controlled codec strings render as plain text but must
    // not spoof rows: bidi overrides, newlines and oversized values
    // are stripped to label-safe chars (max 16).
    let video = test_video(serde_json::json!([test_format_full(
        "evil",
        "avc1\u{202e}gnp8001\u{000a}FREE",
        "none",
        Some(720),
        None,
        "https",
        false
    ),]));
    let opts = video_format_options(&video, true);
    assert_eq!(opts.len(), 1);
    assert_eq!(opts[0].label, "720p · avc1gnp8001FREE");
    // Empty-after-filter degrades to the placeholder, never an empty tag.
    let video = test_video(serde_json::json!([test_format_full(
        "weird",
        "...",
        "none",
        Some(720),
        None,
        "https",
        false
    ),]));
    let opts = video_format_options(&video, true);
    assert_eq!(opts[0].label, "720p · ?");
}

#[test]
fn plan_unknown_adoption_is_first_match() {
    // Restored semantics: the Unknown fallback takes the first
    // fetchable video container in extractor order, not the tallest.
    // Two sparse candidates pin that ordering down.
    let video = test_video(serde_json::json!([
        serde_json::json!({
            "format": "low",
            "format_id": "low",
            "protocol": "https",
            "ext": "mp4",
            "url": "https://cdn.example/low.mp4",
            "http_headers": {},
        }),
        serde_json::json!({
            "format": "high",
            "format_id": "high",
            "protocol": "https",
            "ext": "mp4",
            "url": "https://cdn.example/high.mp4",
            "http_headers": {},
        }),
        test_format_full("music", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let plan = plan_streams(&video, "1080p", None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "low");
}

#[test]
fn plan_stale_pin_to_unlisted_id_resolves_as_split() {
    // Pins are not restricted to listed ids: a stale persisted pin to
    // a fetchable-but-unlisted split (here v1080-avc loses the 1080
    // slot to v1080-vp9 on the codec tie-break, so it never lists)
    // resolves through find_usable_format as a split paired with
    // audio — never an adoption, never a failure.
    let video = test_video(serde_json::json!([
        test_format_full("v1080-vp9", "vp9", "none", Some(1080), None, "https", false),
        test_format_full(
            "v1080-avc",
            "avc1.640028",
            "none",
            Some(1080),
            None,
            "https",
            false
        ),
        test_format_full("music", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let listed: Vec<String> = video_format_options(&video, true)
        .iter()
        .map(|o| o.id.clone())
        .collect();
    assert!(
        !listed.contains(&"v1080-avc".to_string()),
        "fixture must keep the pin unlisted: {listed:?}"
    );
    let plan = plan_streams(&video, "1080p", Some("v1080-avc"), true, 1);
    let v = plan.video_sel.expect("stale pin resolves");
    assert_eq!(v.format_id, "v1080-avc");
    assert!(plan.audio_sel.is_some(), "split pairs with audio");
}

#[test]
fn finish_merge_conflicting_dest_reports_exists() {
    // A foreign file appearing after intake dedupe must requeue with a
    // fresh name: finish_merge reports exactly DEST_EXISTS, the part
    // stays on disk (retryable), and the foreign dest is untouched.
    let dir = std::env::temp_dir().join(format!("grab-fakeexists-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let apart = staging.join("a.m4a");
    std::fs::write(&apart, b"audio-part").unwrap();
    let dest = dir.join("song.m4a");
    std::fs::write(&dest, b"foreign").unwrap();
    let res = crate::download::tokio_rt().block_on(finish_merge(
        std::path::Path::new("/nonexistent-ffmpeg"),
        &staging,
        None,
        &apart,
        &dest,
        None,
        "T",
        None,
        std::time::Duration::from_secs(30),
    ));
    match res {
        Err(e) => assert_eq!(e.to_string(), crate::download::DEST_EXISTS),
        ok => panic!("expected DEST_EXISTS, got {ok:?}"),
    }
    assert_eq!(std::fs::read(&apart).unwrap(), b"audio-part");
    assert_eq!(std::fs::read(&dest).unwrap(), b"foreign");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_video_page_surfaces_stderr_tail() {
    // Nonzero exit: the last non-blank stderr line becomes the detail,
    // not a generic wrapper.
    let dir = std::env::temp_dir().join(format!("grab-fakefail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(
        &bin,
        "#!/bin/sh\necho 'noise line' >&2\necho 'boom detail' >&2\necho '' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let res = crate::download::tokio_rt().block_on(fetch_video_page(
        &bin,
        "https://example.com/v",
        "none",
        std::time::Duration::from_secs(30),
        None,
    ));
    match res {
        Err(e) => assert!(e.to_string().contains("boom detail"), "{e}"),
        ok => panic!("expected fetch error, got {ok:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_video_page_rejects_garbage_stdout() {
    // Zero exit with non-JSON stdout: parse error, not a phantom video.
    let dir = std::env::temp_dir().join(format!("grab-fakegarbage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\necho 'not json'\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let res = crate::download::tokio_rt().block_on(fetch_video_page(
        &bin,
        "https://example.com/v",
        "none",
        std::time::Duration::from_secs(30),
        None,
    ));
    assert!(res.is_err(), "garbage stdout must not parse");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn part_binary_does_not_retry_ordinary_errors() {
    // Only stale-id ("not available") earns the fallback second spawn;
    // a 403-style failure returns after exactly one attempt.
    let dir = std::env::temp_dir().join(format!("grab-fakeyt-403-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
spec=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    if [ "$prev" = "-f" ]; then spec="$a"; fi
    prev="$a"
done
echo "$@" >> "$out.argv.log"
echo "ERROR: [Video] 1: Unable to download: 403 Forbidden" >&2
exit 1
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = dir.join("audio.m4a");
    let report =
        std::sync::Arc::new(|_: u64, _: u64| {}) as std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>;
    let job = part_test_job();
    let (_abort_tx, mut abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_part_ytdlp(
        &bin,
        &job,
        "v123",
        "ba/b",
        &out,
        false,
        std::path::Path::new("/usr/bin/ffmpeg"),
        report,
        &mut abort_rx,
        std::time::Duration::from_secs(30),
    ));
    assert!(res.is_err(), "got {res:?}");
    let logged = std::fs::read_to_string(dir.join("audio.m4a.argv.log")).unwrap();
    assert_eq!(logged.lines().count(), 1, "no fallback retry: {logged}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn part_binary_abort_stays_quiet() {
    // Dropping the abort sender mid-attempt resolves Ok(None) — the
    // caller (pauser/canceller) owns the row state, so no error.
    let dir = std::env::temp_dir().join(format!("grab-fakeyt-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\nsleep 60\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = dir.join("audio.m4a");
    let report =
        std::sync::Arc::new(|_: u64, _: u64| {}) as std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>;
    let job = part_test_job();
    let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel();
    let handle = std::thread::spawn(move || {
        crate::download::tokio_rt().block_on(run_part_ytdlp(
            &bin,
            &job,
            "v123",
            "ba/b",
            &out,
            false,
            std::path::Path::new("/usr/bin/ffmpeg"),
            report,
            &mut abort_rx,
            std::time::Duration::from_secs(30),
        ))
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(abort_tx);
    let res = handle.join().expect("thread joins");
    assert!(matches!(res, Ok(None)), "abort is quiet, got {res:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sanitize_missing_codec_fields_still_parse() {
    // A new sparse extractor omitting codec/container/height keys must
    // parse like TikTok's: Unknown-typed, adoptable, never a row killer.
    let mut value = serde_json::json!({
        "id": "sparse1",
        "title": "S",
        "formats": [
            {"format_id": "dl", "url": "https://cdn.example/dl.mp4"},
        ],
    });
    sanitize_video_json(&mut value);
    let video: yt_dlp::model::Video = serde_json::from_value(value).expect("sparse parses");
    assert_eq!(video.formats.len(), 1);
    let plan = plan_streams(&video, "best", None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "dl");
}

#[test]
fn resume_plan_unknown_total_resumes_bytes_on_disk() {
    // No total to judge overlong against (live-adjacent/single-connection
    // flows): bytes on disk mean resume, never a Fresh wipe.
    let dir = test_manifest_dir("unknown-total");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dest_part_path(&dest, "audio", "webm"), vec![0u8; 49]).unwrap();
    let m = test_manifest();
    let mut q = test_query(Some(&m), &dest);
    q.video_total = None;
    q.audio_total = None;
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_proxy_env_sets_nothing_when_direct() {
    // A direct spawn must not inherit NO_PROXY from anywhere: only an
    // explicit proxy sets it.
    // Deterministic regardless of the ambient environment: a direct
    // spawn must neither set nor inherit proxy routing. The spawn runs
    // inside the runtime (tokio process needs a reactor context).
    let out = crate::download::tokio_rt().block_on(async {
        let mut cmd = tokio::process::Command::new("env");
        cmd.env_remove("NO_PROXY").env_remove("no_proxy");
        cmd.env_remove("HTTP_PROXY").env_remove("http_proxy");
        cmd.env_remove("HTTPS_PROXY").env_remove("https_proxy");
        cmd.env_remove("ALL_PROXY").env_remove("all_proxy");
        apply_proxy_env(&mut cmd, None);
        cmd.output().await.expect("env runs")
    });
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("NO_PROXY=") || l.starts_with("no_proxy=")),
        "direct spawn leaks proxy env"
    );
}

#[test]
fn apply_proxy_env_stamps_no_proxy_when_proxied() {
    let proxy = crate::download::DownloadOptions {
        tries: 3,
        timeout: 30,
        limit_rate: String::new(),
        user_agent: String::new(),
        connections: 4,
        proxy_mode: "manual".into(),
        proxy_type: "socks5".into(),
        proxy_host: "127.0.0.1".into(),
        proxy_port: 9050,
        cookies_browser: String::new(),
    }
    .proxy_config()
    .expect("well-formed")
    .expect("proxied");
    let out = crate::download::tokio_rt().block_on(async {
        let mut cmd = tokio::process::Command::new("env");
        cmd.env_remove("NO_PROXY").env_remove("no_proxy");
        apply_proxy_env(&mut cmd, Some(&proxy));
        cmd.output().await.expect("env runs")
    });
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.starts_with("NO_PROXY=")),
        "proxied spawn missing NO_PROXY"
    );
}

#[test]
fn dest_part_paths_sit_beside_finished_file() {
    // yt-dlp defaults: `<stem>.<kind>.<ext>` in the dest dir, so `.part`
    // shells and fragments show up in the user's folder while moving
    // nothing else. Deterministic across attempts for crash-resume.
    let dest = std::path::Path::new("/tmp/dl/Clip.mp4");
    assert_eq!(
        dest_part_path(dest, "video", "mp4"),
        std::path::Path::new("/tmp/dl/Clip.video.mp4")
    );
    assert_eq!(
        dest_part_path(dest, "audio", "webm"),
        std::path::Path::new("/tmp/dl/Clip.audio.webm")
    );
    assert_eq!(
        dest_part_path(dest, "hls", "%(ext)s"),
        std::path::Path::new("/tmp/dl/Clip.hls.%(ext)s")
    );
    assert_eq!(
        dest_part_path(dest, "live", "mp4"),
        std::path::Path::new("/tmp/dl/Clip.live.mp4")
    );
}

#[test]
fn clean_dest_parts_keeps_finished_and_foreign_files() {
    let dir = std::env::temp_dir().join(format!("grab-cleanparts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("Clip.mp4");
    let keep = [
        "Clip.mp4",
        "Clip.srt",
        // Collected subtitle sidecars live outside the part namespace.
        "Clip.en.srt",
        "Other.video.mp4",
        "Clip.video-notes.txt",
    ];
    let drop = [
        "Clip.video.mp4",
        "Clip.video.mp4.part",
        "Clip.audio.webm",
        "Clip.audio.webm.part",
        "Clip.hls.mp4",
        "Clip.live.mp4.part",
        // Stale sidecars beside part files are part-namespace litter.
        "Clip.video.en.srt",
        "Clip.hls.en.srt",
    ];
    for n in keep.iter().chain(drop.iter()) {
        std::fs::write(dir.join(n), b"x").unwrap();
    }
    clean_dest_parts(&dest);
    for n in keep {
        assert!(dir.join(n).exists(), "{n} must survive");
    }
    for n in drop {
        assert!(!dir.join(n).exists(), "{n} must go");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_builders_use_machine_progress_and_ignore_config() {
    // Every yt-dlp spawn parses `--progress-template` lines (never human
    // prose) and ignores ambient user configs that could reshape argv.
    let out = std::path::Path::new("/tmp/staging/video.mp4");
    let job = part_test_job();
    for argv in [
        part_download_argv(
            &job,
            "v123",
            out,
            false,
            std::path::Path::new("/usr/bin/ffmpeg"),
        ),
        live_capture_argv(&job, "h720", out),
        hls_download_argv(&job, "h720", std::path::Path::new("/usr/bin/ffmpeg"), out),
    ] {
        assert!(argv.contains(&"--ignore-config".to_string()), "{argv:?}");
        let t = argv
            .iter()
            .position(|a| a == "--progress-template")
            .expect("template flag");
        assert_eq!(argv[t + 1], YTDLP_PROGRESS_TEMPLATE, "{argv:?}");
    }
}
// ── subtitle sidecars ────────────────────────────────────────────────

#[test]
fn part_argv_adds_subtitle_flags_when_requested() {
    let mut job = part_test_job();
    job.subtitles = Some("en".into());
    let out = std::path::Path::new("/tmp/staging/video.mp4");
    let argv = part_download_argv(
        &job,
        "hls-720",
        out,
        true,
        std::path::Path::new("/usr/bin/ffmpeg"),
    );
    let sub = argv
        .iter()
        .position(|a| a == "--sub-langs")
        .expect("--sub-langs");
    assert_eq!(argv[sub + 1], "en");
    assert!(argv.contains(&"--write-subs".to_string()));
    assert!(argv.contains(&"--write-auto-subs".to_string()));
    let conv = argv
        .iter()
        .position(|a| a == "--convert-subs")
        .expect("--convert-subs");
    assert_eq!(argv[conv + 1], "srt");
    // Conversion points at Grab's resolved ffmpeg, not PATH (Flatpak).
    let loc = argv
        .iter()
        .position(|a| a == "--ffmpeg-location")
        .expect("--ffmpeg-location");
    assert_eq!(argv[loc + 1], "/usr/bin");
    // Flags must stay ahead of the `--` URL separator: anything after it
    // is consumed by yt-dlp as an extra URL, never read as an option.
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(sub < sep && conv < sep && loc < sep, "{argv:?}");
    assert_eq!(argv[argv.len() - 1], "https://x.com/u/status/1");
}

#[test]
fn part_argv_omits_subtitle_flags_when_not_requested() {
    let mut job = part_test_job();
    job.subtitles = Some("fr".into());
    let out = std::path::Path::new("/tmp/staging/video.mp4");
    let argv = part_download_argv(
        &job,
        "hls-720",
        out,
        false,
        std::path::Path::new("/usr/bin/ffmpeg"),
    );
    assert_no_subtitle_tokens(&argv);
    assert!(
        !argv.iter().any(|a| a == "--ffmpeg-location"),
        "no ffmpeg location without subs: {argv:?}"
    );
    let mut job = part_test_job();
    job.subtitles = None;
    let argv = part_download_argv(
        &job,
        "hls-720",
        out,
        true,
        std::path::Path::new("/usr/bin/ffmpeg"),
    );
    assert_no_subtitle_tokens(&argv);
}

#[test]
fn leg_downloads_subs_matrix() {
    // Only legs carrying video content take subtitles: the split video
    // part, or an adopted single file. Audio parts and rows without a
    // configured language never do.
    let mut job = part_test_job();
    job.subtitles = Some("en".into());
    // Split download: video leg yes, audio leg no.
    assert!(leg_downloads_subs(&job, true, false));
    assert!(!leg_downloads_subs(&job, false, false));
    // Adopted single file (no split video part): the audio leg carries
    // the whole video.
    assert!(leg_downloads_subs(&job, false, true));
    // No configured language: nothing anywhere.
    job.subtitles = None;
    assert!(!leg_downloads_subs(&job, true, false));
    assert!(!leg_downloads_subs(&job, false, true));
}

#[test]
fn hls_argv_takes_subtitles() {
    let mut job = part_test_job();
    job.quality = "best".into();
    job.subtitles = Some("ar".into());
    let dest = std::path::Path::new("/tmp/dl/v.mp4");
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    let sub = argv
        .iter()
        .position(|a| a == "--sub-langs")
        .expect("--sub-langs");
    assert_eq!(argv[sub + 1], "ar");
    assert!(argv.contains(&"--write-auto-subs".to_string()));
    let sep = argv.iter().position(|a| a == "--").expect("separator");
    assert!(sub < sep, "subtitle flags must precede the URL separator");
    // Without a configured language the merge flags stay, subtitles go.
    job.subtitles = None;
    let argv = hls_download_argv(&job, "h1080", std::path::Path::new("/usr/bin/ffmpeg"), dest);
    assert_no_subtitle_tokens(&argv);
    assert!(argv.contains(&"--merge-output-format".to_string()));
}

#[test]
fn live_capture_argv_never_takes_subtitles() {
    // Captioning a live edge is not a download: even with a language
    // configured, live captures must not pass subtitle flags.
    let mut job = live_test_job();
    job.subtitles = Some("en".into());
    let argv = live_capture_argv(&job, "h720", std::path::Path::new("/tmp/dl/v.live.ts"));
    assert_no_subtitle_tokens(&argv);
}

#[test]
fn subtitle_language_index_value_round_trip() {
    assert_eq!(subtitle_language_index("off"), 0);
    assert_eq!(subtitle_language_value(0), "off");
    assert_eq!(subtitle_language_index("en"), 1);
    assert_eq!(subtitle_language_value(1), "en");
    assert_eq!(subtitle_language_index("zh"), 17);
    assert_eq!(subtitle_language_value(17), "zh");
    // Unknown settings fall back to English (the schema default); bad
    // indexes fall back to English too.
    assert_eq!(subtitle_language_index("xx"), 1);
    assert_eq!(subtitle_language_index(""), 1);
    assert_eq!(subtitle_language_value(99), "en");
    assert_eq!(
        subtitle_language_labels().len(),
        SUBTITLE_LANGUAGE_VALUES.len()
    );
}

/// Subtitle tokens that must never leak onto non-video legs. (HLS always
/// carries `--ffmpeg-location` for its merge step, so it is excluded
/// here; the part/live builders must not emit it without subs either.)
const SUBTITLE_TOKENS: &[&str] = &[
    "--write-subs",
    "--sub-langs",
    "--write-auto-subs",
    "--convert-subs",
];

fn assert_no_subtitle_tokens(argv: &[String]) {
    for t in SUBTITLE_TOKENS {
        assert!(!argv.iter().any(|a| a == *t), "{t} leaked: {argv:?}");
    }
}

#[test]
fn subtitle_lang_active_allowlist() {
    // The configured code resolves verbatim; `off`, empty and unknown
    // codes (hand-edited dconf) resolve to off — never passed through
    // to `--sub-langs` or the sidecar filename.
    assert_eq!(subtitle_lang_active("en"), Some("en".to_string()));
    assert_eq!(subtitle_lang_active("zh"), Some("zh".to_string()));
    assert_eq!(subtitle_lang_active("off"), None);
    assert_eq!(subtitle_lang_active(""), None);
    assert_eq!(subtitle_lang_active("xx"), None);
    assert_eq!(subtitle_lang_active(" "), None);
    // Normalization: surrounding whitespace and case fold onto the list.
    assert_eq!(subtitle_lang_active(" EN "), Some("en".to_string()));
    // Regex-shaped values never reach yt-dlp (`--sub-langs` accepts
    // patterns, so only exact allowlist hits pass).
    assert_eq!(subtitle_lang_active("en,fr"), None);
    assert_eq!(subtitle_lang_active("en.*"), None);
}

#[test]
fn sidecar_path_for_cases() {
    // yt-dlp drops `--write-subs` sidecars as `<out-stem>.<lang>.srt`
    // beside the `-o` path; Grab keeps the same shape beside the dest.
    assert_eq!(
        sidecar_path_for(std::path::Path::new("/tmp/dl/Clip.video.mp4"), "en"),
        std::path::Path::new("/tmp/dl/Clip.video.en.srt")
    );
    assert_eq!(
        sidecar_path_for(std::path::Path::new("/tmp/dl/Clip.mp4"), "ar"),
        std::path::Path::new("/tmp/dl/Clip.ar.srt")
    );
}

#[test]
fn finished_sidecars_escape_part_namespace() {
    // Structural: a collected `<stem>.<lang>.srt` (allowlisted lang, no
    // dots) can never match Grab's part infixes, so `clean_dest_parts`
    // leaves finished sidecars alone for every offered language.
    for lang in SUBTITLE_LANGUAGE_VALUES {
        if *lang == "off" {
            continue;
        }
        let name = format!("Clip.{lang}.srt");
        assert!(!is_grab_part(&name, "Clip"), "{name} must not match");
    }
}

#[test]
fn collect_sidecar_moves_and_ignores_missing() {
    let dir = std::env::temp_dir().join(format!("grab-sidecar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("Clip.video.mp4");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dir.join("Clip.video.en.srt"), b"subs").unwrap();
    collect_sidecar(&sidecar_path_for(&out, "en"), &dest, "en");
    assert_eq!(std::fs::read(dir.join("Clip.en.srt")).unwrap(), b"subs");
    assert!(!dir.join("Clip.video.en.srt").exists());
    // A page that published no subtitles: missing source is a quiet no-op.
    collect_sidecar(&sidecar_path_for(&out, "fr"), &dest, "fr");
    assert!(!dir.join("Clip.fr.srt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for HLS that also drops an `en` sidecar beside the `-o`
/// template (what the real binary does for `--write-subs`).
fn fake_ytdlp_hls_subs(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-hls-subs");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
stem="$(basename "$out" | sed 's/%(ext)s/mp4/')"
side="$(printf '%s' "$stem" | sed 's/\.mp4$//').en.srt"
printf 'subbytes' > "$(dirname "$out")/$side"
out="$(printf '%s' "$out" | sed 's/%(ext)s/mp4/')"
printf 'hlsbytes' > "$out"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn hls_collects_sidecar_beside_finished_file() {
    // End to end on the Critical path: the sidecar the binary drops as
    // `<stem>.hls.en.srt` must be collected as `<stem>.en.srt` next to
    // the claimed file — never orphaned under the part name.
    let dir = std::env::temp_dir().join(format!("grab-fakehls-subs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls_subs(&dir);
    let staging = dir.join("staging");
    let job = VideoJob {
        item_id: 1,
        page_url: "https://x.com/u/status/1".into(),
        quality: "best".into(),
        dest: dir.join("v.mp4"),
        tries: 3,
        timeout_secs: 60,
        user_agent: "Grab-test/1.0".into(),
        video_format_id: None,
        is_live: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
        subtitles: Some("en".into()),
        proxy: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"hlsbytes");
    assert_eq!(std::fs::read(dir.join("v.en.srt")).unwrap(), b"subbytes");
    assert!(
        !dir.join("v.hls.en.srt").exists(),
        "part-namespaced sidecar must be collected, not orphaned"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake yt-dlp for live that behaves like a killed recorder: bytes land
/// in the `.part` shell, no progress line is ever printed, then it exits
/// cleanly after a beat (so the file watcher, not the log parser, must
/// announce recording).
fn fake_ytdlp_live_shell_only(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("fake-ytdlp-live-shell");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-o" ]; then out="$a"; fi
    prev="$a"
done
printf 'tsbytes' > "$out.part"
sleep 2
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn live_part_shell_announces_recording_and_is_swept() {
    // Two real-world live bugs in one: the file watcher polled the `-o`
    // path (empty until finalize) instead of the growing `.part` shell,
    // so the row sat on "Resolving media…" while bytes landed; and the
    // shell survived next to the finished file after stop.
    let dir = std::env::temp_dir().join(format!("grab-fakelive-shell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live_shell_only(&dir);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_abort_tx, abort_rx) = tokio::sync::oneshot::channel();
    let res = crate::download::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        "h1080",
        abort_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "got {res:?}");
    assert_eq!(std::fs::read(&job.dest).unwrap(), b"tsbytes");
    let phases: Vec<String> = {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::download::EngineMsg::Phase(p) = msg {
                out.push(p);
            }
        }
        out
    };
    assert!(
        phases.iter().any(|p| p.contains("Recording")),
        "shell growth must announce Recording, phases seen: {phases:?}"
    );
    assert!(
        !dir.join("v.live.mp4.part").exists(),
        "stopped capture must not leave its shell behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stem_reserved_in_matches_namespace_only() {
    let names: Vec<String> = [
        "Clip.video.mp4",
        "Clip.video.mp4.part",
        "Clip.audio.webm",
        "Clip.hls.mp4",
        "Clip.live.ts",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(stem_reserved_in(&names, "Clip"));
    // Finished files, foreign stems and lookalikes reserve nothing.
    // (Sidecars live in the `subs` list below — they reserve by design.)
    let clean: Vec<String> = [
        "Clip.mp4",
        "Other.video.mp4",
        "Clip.video-notes.txt",
        "Clip.mp4.part",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(!stem_reserved_in(&clean, "Clip"));
    assert!(!stem_reserved_in(&names, "Other"));
    // Dot boundary: a longer stem sharing the prefix does not match.
    assert!(!stem_reserved_in(&names, "Clippy"));
    // Empty stems never match, even against dotfiles shaped like parts.
    assert!(!stem_reserved_in(&[".video.mp4".to_string()], ""));
    assert!(!stem_reserved_in(&[], "Clip"));
    // Subtitle sidecars reserve the stem (any offered language — the
    // pref is global, and delete trashes every offered code); bare or
    // unknown-code names do not.
    let subs: Vec<String> = ["Clip.en.srt", "Clip.fr.srt"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(stem_reserved_in(&subs, "Clip"));
    let bare: Vec<String> = ["Clip.srt", "Clip.eng.srt", "Clip.EN.srt"]
        // "eng" is unknown *today*: if it ever joins the offered list,
        // this case flips to reserving (correctly) — update then.
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(!stem_reserved_in(&bare, "Clip"));
}

#[test]
fn dir_file_names_snapshots_dir() {
    let dir = std::env::temp_dir().join(format!("grab-dirnames-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.iso"), b"x").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let mut names = dir_file_names(&dir);
    names.sort();
    assert_eq!(names, vec!["a.iso".to_string(), "sub".to_string()]);
    assert!(dir_file_names(&dir.join("missing-dir")).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collect_sidecar_never_clobbers() {
    // A foreign sidecar arriving mid-download (after the intake
    // snapshot) survives collection: ours stays beside the part file
    // for row removal to sweep, and the download itself is unaffected.
    let dir = std::env::temp_dir().join(format!("grab-sidecar-noclobber-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("Clip.video.mp4");
    let dest = dir.join("Clip.mp4");
    std::fs::write(dir.join("Clip.video.en.srt"), b"ours").unwrap();
    std::fs::write(dir.join("Clip.en.srt"), b"foreign").unwrap();
    collect_sidecar(&sidecar_path_for(&out, "en"), &dest, "en");
    assert_eq!(std::fs::read(dir.join("Clip.en.srt")).unwrap(), b"foreign");
    assert_eq!(
        std::fs::read(dir.join("Clip.video.en.srt")).unwrap(),
        b"ours"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sweep_parts_removes_split_parts_only() {
    // Successful splits must leave no litter: both legs go, the
    // finished file and sidecars stay, missing files are quiet.
    let dir = std::env::temp_dir().join(format!("grab-sweepparts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Spaced name mirrors real titles (and file_stem edge cases).
    let vpart = dir.join("How Do LLMs Work?.video.mp4");
    let apart = dir.join("How Do LLMs Work?.audio.m4a");
    let dest = dir.join("How Do LLMs Work?.mp4");
    let sidecar = dir.join("How Do LLMs Work?.en.srt");
    for p in [&vpart, &apart, &dest, &sidecar] {
        std::fs::write(p, b"x").unwrap();
    }
    sweep_parts(Some(&vpart), &apart);
    assert!(!vpart.exists());
    assert!(!apart.exists());
    assert!(dest.exists(), "finished file must survive");
    assert!(sidecar.exists(), "collected sidecar must survive");
    // Adopted singles (and audio-only rows) sweep nothing: the part
    // was renamed to the destination, not copied.
    std::fs::write(&apart, b"x").unwrap();
    sweep_parts(None, &apart);
    assert!(apart.exists());
    // Missing files never panic.
    sweep_parts(
        Some(&dir.join("gone.video.mp4")),
        &dir.join("gone.audio.m4a"),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
