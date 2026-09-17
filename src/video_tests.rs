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

// ── audio-first ──────────────────────────────────────────────────────

#[test]
fn audio_first_domains() {
    assert!(is_audio_first("https://soundcloud.com/artist/track"));
    assert!(is_audio_first("https://m.soundcloud.com/artist/track"));
    assert!(is_audio_first("https://artist.bandcamp.com/track/name"));
    assert!(is_audio_first("https://bandcamp.com/track/name"));
}

#[test]
fn audio_first_negative() {
    assert!(!is_audio_first("https://www.youtube.com/watch?v=x"));
    assert!(!is_audio_first("https://vimeo.com/123"));
    assert!(!is_audio_first("https://example.com/f.mp3"));
    assert!(!is_audio_first("not a url"));
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
        video_format_id: Some("137".into()),
    };
    let json = serde_json::to_string(&src).unwrap();
    let back: VideoSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back, src);
}

#[test]
fn serde_page_old_json_gets_defaults() {
    // Queue files written before quality/audio_only/format existed must
    // still parse: quality falls back to 1080p, audio to off, no pin.
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
            video_format_id: None,
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
        video_total: Some(100),
        audio_total: Some(50),
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
fn resume_plan_resumes_truncated_parts() {
    let dir = test_manifest_dir("truncated");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 100]).unwrap();
    // Audio part truncated mid-download: resume it, never re-download.
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 49]).unwrap();
    let dest = dir.join("Clip.mp4");
    let m = test_manifest();
    let q = test_query(Some(&m), &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_resumes_pending_manifest() {
    // Pause before any part completed bookkeeping: the pending sidecar
    // (zero bytes recorded) plus partial files on disk means resume.
    let dir = test_manifest_dir("pending");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 60]).unwrap();
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 30]).unwrap();
    let dest = dir.join("Clip.mp4");
    let mut m = test_manifest();
    m.video_bytes = 0;
    m.audio_bytes = 0;
    let q = test_query(Some(&m), &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Resume);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_without_manifest_despite_parts() {
    // Bytes without a matching sidecar are unverifiable (pre-sidecar
    // upgrades, foreign files): wipe and start clean.
    let dir = test_manifest_dir("unverified");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 60]).unwrap();
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 30]).unwrap();
    let dest = dir.join("Clip.mp4");
    let q = test_query(None, &dir, &dest);
    assert_eq!(resume_plan(&q), ResumePlan::Fresh);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_plan_fresh_on_overlong_part() {
    // A part larger than its total cannot be resumed into: wipe it.
    let dir = test_manifest_dir("overlong");
    std::fs::write(part_path(&dir, "video", "mp4"), vec![0u8; 100]).unwrap();
    std::fs::write(part_path(&dir, "audio", "webm"), vec![0u8; 60]).unwrap();
    let dest = dir.join("Clip.mp4");
    let m = test_manifest();
    let q = test_query(Some(&m), &dir, &dest);
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
    let vpart = part_path(&dir, "video", "mp4");
    let apart = part_path(&dir, "audio", "webm");
    std::fs::File::create(&vpart).unwrap().set_len(100).unwrap();
    std::fs::File::create(&apart).unwrap().set_len(50).unwrap();
    assert!(is_sparse_shell(&vpart));
    assert!(is_sparse_shell(&apart));
    let dest = dir.join("Clip.mp4");
    let m = test_manifest();
    let q = test_query(Some(&m), &dir, &dest);
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
        video_format_id: None,
        cookies_browser: "none".into(),
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
fn default_video_filename_by_mode() {
    assert_eq!(default_video_filename("Clip", false), "Clip.mp4");
    assert_eq!(default_video_filename("Clip", true), "Clip.m4a");
    // Untouched otherwise: sanitizing is the intake's job.
    assert_eq!(default_video_filename("a/b", false), "a/b.mp4");
    assert_eq!(default_video_filename("", true), ".m4a");
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
    let opts = video_format_options(&video);
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
fn codec_rank_orders_newest_first() {
    assert!(codec_rank("av01.0.08M.08") < codec_rank("vp9"));
    assert!(codec_rank("VP9") < codec_rank("hev1.1.6.L93"));
    assert!(codec_rank("hvc1") < codec_rank("avc1.640028"));
    assert!(codec_rank("avc1.640028") < codec_rank("theora"));
    assert_eq!(codec_rank("av1"), codec_rank("av01.0.05M.08"));
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
    assert!(video_format_options(&video).is_empty());
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
        audio_only: false,
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
