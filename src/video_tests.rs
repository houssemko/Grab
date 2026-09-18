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
        is_live: false,
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
            is_live: false,
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
        is_live: false,
        newest_codecs: true,
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
    assert_eq!(
        select_hls_format(&formats, Some(720)).expect("hls").url,
        "https://cdn.example/h1080"
    );
    assert_eq!(
        select_hls_format(&formats, Some(2160)).expect("hls").url,
        "https://cdn.example/h1080"
    );
    // Best takes the tallest; https entries never select as HLS.
    assert_eq!(
        select_hls_format(&formats, None).expect("hls").url,
        "https://cdn.example/h1080"
    );
    assert!(select_hls_format(&[], Some(720)).is_none());
    assert!(find_hls_format(&formats, "h480").is_some());
    assert!(find_hls_format(&formats, "https").is_none());
    assert!(find_hls_format(&formats, "gone").is_none());
}

#[test]
fn ffmpeg_headers_passthrough() {
    let headers = yt_dlp::model::format::HttpHeaders {
        user_agent: "Extractor/1".into(),
        accept: "*/*".into(),
        accept_language: "".into(),
        sec_fetch_mode: "no-cors".into(),
    };
    // Caller UA wins; empty values are skipped; lines are CRLF.
    assert_eq!(
        ffmpeg_headers(&headers, "Grab/1", None),
        "User-Agent: Grab/1\r\nAccept: */*\r\nSec-Fetch-Mode: no-cors\r\n"
    );
    assert_eq!(
        ffmpeg_headers(&headers, "", None),
        "User-Agent: Extractor/1\r\nAccept: */*\r\nSec-Fetch-Mode: no-cors\r\n"
    );
    // A resolved page Referer rides along for hotlink-guarded CDNs.
    assert_eq!(
        ffmpeg_headers(&headers, "Grab/1", Some("https://www.tiktok.com/")),
        "User-Agent: Grab/1\r\nAccept: */*\r\nSec-Fetch-Mode: no-cors\r\nReferer: https://www.tiktok.com/\r\n"
    );
    let bare = yt_dlp::model::format::HttpHeaders {
        user_agent: "".into(),
        accept: "".into(),
        accept_language: "".into(),
        sec_fetch_mode: "".into(),
    };
    assert_eq!(ffmpeg_headers(&bare, "", None), "");
    assert_eq!(
        ffmpeg_headers(&bare, "", Some("https://www.tiktok.com/")),
        "Referer: https://www.tiktok.com/\r\n"
    );
}

#[test]
fn ffmpeg_progress_reports_total_size() {
    assert_eq!(parse_progress_size("total_size=1048576"), Some(1048576));
    assert_eq!(parse_progress_size("total_size=0"), Some(0));
    assert_eq!(parse_progress_size("out_time_ms=123456"), None);
    assert_eq!(parse_progress_size("progress=end"), None);
    assert_eq!(parse_progress_size("total_size=abc"), None);
    assert_eq!(parse_progress_size("garbage"), None);
}

#[test]
fn hls_master_parses_variants_and_audio() {
    let base = url::Url::parse("https://cdn.example/vid/master.m3u8").unwrap();
    let text = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"en\",URI=\"audio.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"es\",DEFAULT=YES,URI=\"audio-es.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"aac\"\n\
        low.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"mp4a.40.2,avc1.640028\"\n\
        mid-muxed.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080,AUDIO=\"aac\"\n\
        /abs/hi.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160\n\
        ultra.m3u8\n";
    let (variants, audios) = parse_hls_master(text, &base).expect("master");
    assert_eq!(variants.len(), 4);
    assert_eq!(variants[0].height, Some(360));
    assert_eq!(variants[0].uri, "https://cdn.example/vid/low.m3u8");
    assert_eq!(variants[1].uri, "https://cdn.example/vid/mid-muxed.m3u8");
    assert!(
        variants[1]
            .codecs
            .as_deref()
            .is_some_and(|c| c.contains("mp4a"))
    );
    assert_eq!(variants[2].uri, "https://cdn.example/abs/hi.m3u8");
    assert_eq!(variants[3].audio_group, None);
    assert_eq!(audios.len(), 2);
    assert_eq!(
        audios[0].uri.as_deref(),
        Some("https://cdn.example/vid/audio.m3u8")
    );
    // Height cap takes the smallest sounding variant at or above, with
    // CODECS-muxed winning ties; the DEFAULT rendition is selected.
    let pick = pick_hls_variant(&variants, &audios, Some(720)).expect("pick");
    assert_eq!(pick.video, "https://cdn.example/vid/mid-muxed.m3u8");
    assert_eq!(pick.audio, None);
    let hi = pick_hls_variant(&variants, &audios, Some(1080)).expect("hi");
    assert_eq!(hi.video, "https://cdn.example/abs/hi.m3u8");
    assert_eq!(
        hi.audio.as_deref(),
        Some("https://cdn.example/vid/audio-es.m3u8")
    );
    let best = pick_hls_variant(&variants, &audios, None).expect("best");
    assert_eq!(best.video, "https://cdn.example/vid/ultra.m3u8");
    assert_eq!(best.audio, None);
    assert!(pick_hls_variant(&[], &audios, Some(720)).is_none());
    // Media playlists (segments, no variants) pass through untouched.
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
    assert!(parse_hls_master(media, &base).is_none());
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
fn hls_joins_carry_master_query() {
    // Tokenized masters (Twitter) authenticate every URL with the same
    // query: relative references inherit it, absolute ones keep theirs.
    let base = url::Url::parse("https://cdn.example/vid/master.m3u8?tag=12").unwrap();
    assert_eq!(
        join_hls_url(&base, "low.m3u8").unwrap().as_str(),
        "https://cdn.example/vid/low.m3u8?tag=12"
    );
    assert_eq!(
        join_hls_url(&base, "/abs/hi.m3u8?tag=34").unwrap().as_str(),
        "https://cdn.example/abs/hi.m3u8?tag=34"
    );
    let plain = url::Url::parse("https://cdn.example/vid/master.m3u8").unwrap();
    assert_eq!(
        join_hls_url(&plain, "low.m3u8").unwrap().as_str(),
        "https://cdn.example/vid/low.m3u8"
    );
}

#[test]
fn hls_format_spec_names_height_and_pin() {
    // bv* leads so direct muxed files win over lower splits; the
    // trailing /b still catches audio-only pages.
    assert_eq!(hls_format_spec("best", None, false), "bv*+ba/b");
    assert_eq!(
        hls_format_spec("1080p", None, false),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(
        hls_format_spec("mystery", None, false),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(
        hls_format_spec("1080p", Some("hls-99"), false),
        "hls-99+ba/b"
    );
    assert_eq!(
        hls_format_spec("1080p", Some("   "), false),
        "bv*[height<=1080]+ba/b"
    );
    assert_eq!(hls_format_spec("1080p", None, true), "ba/b");
    assert_eq!(hls_format_spec("1080p", Some("hls-99"), true), "ba/b");
}

#[test]
fn ytdlp_progress_parses_percent_and_size() {
    let (frac, total) =
        parse_ytdlp_progress("[download]  45.2% of ~50.00MiB at 1.23MiB/s ETA 00:20")
            .expect("progress");
    assert!((frac - 0.452).abs() < 1e-12);
    assert_eq!(total, Some(52_428_800));
    assert_eq!(
        parse_ytdlp_progress("[download] 100% of 10.00MiB in 5s"),
        Some((1.0, Some(10_485_760)))
    );
    assert_eq!(parse_ytdlp_progress("[download] Destination: x.mp4"), None);
    assert_eq!(
        parse_ytdlp_progress("[download] file already downloaded"),
        None
    );
    assert_eq!(parse_ytdlp_progress("[Merger] Merging"), None);
    assert_eq!(parse_ytdlp_progress("[info] x"), None);
    assert_eq!(parse_ytdlp_progress("garbage"), None);
    assert!(is_ytdlp_merge_line("[Merger] Merging formats"));
    assert!(is_ytdlp_merge_line("[ExtractAudio] Destination"));
    assert!(!is_ytdlp_merge_line("[download] 10% of 1MiB"));
    assert_eq!(
        parse_ytdlp_after_move("/tmp/grab/abc.mp4"),
        Some("/tmp/grab/abc.mp4")
    );
    assert_eq!(parse_ytdlp_after_move("[download] 10%"), None);
    assert_eq!(parse_ytdlp_after_move(""), None);
    // Size units, approximate marker included.
    assert_eq!(
        parse_ytdlp_progress("[download] 50% of ~1.50GiB at 1MiB/s"),
        Some((0.5, Some(1_610_612_736)))
    );
    assert_eq!(
        parse_ytdlp_progress("[download] 25% of 800K at 1MiB/s"),
        Some((0.25, Some(819_200)))
    );
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
fn terminate_ffmpeg_graceful_then_forced() {
    crate::download::tokio_rt().block_on(async {
        // A plain sleep dies on SIGTERM inside the grace period.
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(terminate_ffmpeg(&mut child, Duration::from_millis(500)).await);
        // A TERM-ignoring sleep survives grace: SIGKILL fallback still
        // reaps it, reporting not-graceful. Clean env (no BASH_ENV slow
        // startup racing the signal) and a READY handshake (trap
        // installed before we signal) keep this deterministic; exec
        // carries the ignored disposition into sleep itself.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "trap '' TERM; echo READY; exec sleep 30"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use tokio::io::AsyncBufReadExt as _;
            let mut line = String::new();
            tokio::io::BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .await
                .unwrap();
            assert_eq!(line.trim(), "READY");
        }
        assert!(!terminate_ffmpeg(&mut child, Duration::from_millis(200)).await);
    });
}

#[test]
fn adopt_hls_output_moves_or_rejects_empty() {
    crate::download::tokio_rt().block_on(async {
        let dir = std::env::temp_dir().join(format!("grab-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        // Empty capture fails instead of stranding an empty Done row.
        let empty = staging.join("hls.mp4");
        std::fs::write(&empty, b"").unwrap();
        let dest = dir.join("out.mp4");
        assert!(adopt_hls_output(&empty, &dest, &staging).await.is_err());
        assert!(!staging.exists());
        assert!(!dest.exists());
        // Real capture moves with content; source gone.
        std::fs::create_dir_all(&staging).unwrap();
        let full = staging.join("hls.mp4");
        std::fs::write(&full, b"0123456789").unwrap();
        let size = adopt_hls_output(&full, &dest, &staging)
            .await
            .expect("adopt");
        assert_eq!(size, Some(10));
        assert!(!full.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

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
        audio_only: false,
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
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "dl");
    assert!(plan.audio_only);
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_pinned_hls_wins_over_muxed_adoption() {
    // The x.com shadowing bug: the pin was dropped by the HTTPS-only
    // lookup and the muxed adoption then vetoed the HLS path.
    let video = x_like_video();
    let plan = plan_streams(&video, "720p", false, Some("hls-720"), true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    assert!(!plan.audio_only);
    let hls = plan.hls_sel.expect("pinned hls");
    assert_eq!(hls.height, Some(720));
}

#[test]
fn plan_muxed_only_still_adopts_without_pin() {
    // No pin, no splits: the muxed file adopts as before (precedence
    // over the HLS preset is unchanged) — but at the requested height,
    // not first-in-extractor-order.
    let video = x_like_video();
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.audio_only);
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
        let plan = plan_streams(&video, quality, false, None, true, 1);
        assert_eq!(
            plan.audio_sel.expect("adopted").format_id,
            want,
            "quality {quality}"
        );
        assert!(plan.audio_only, "quality {quality}");
    }
}

#[test]
fn plan_stale_hls_pin_degrades_to_muxed_adoption() {
    // A vanished HLS pin behaves like no pin: preset, then adoption
    // (at the dialog-picked height, not the lowest listing).
    let video = x_like_video();
    let plan = plan_streams(&video, "1080p", false, Some("gone"), true, 1);
    assert_eq!(plan.audio_sel.expect("adopted").format_id, "http-720");
    assert!(plan.audio_only);
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_splits_stay_split() {
    let video = test_video(serde_json::json!([
        test_format_full("v", "avc1.640028", "none", Some(1080), None, "https", false),
        test_format_full("a", "none", "mp4a.40.2", None, None, "https", false),
    ]));
    let plan = plan_streams(&video, "1080p", false, None, true, 1);
    assert_eq!(plan.video_sel.expect("video").format_id, "v");
    assert_eq!(plan.audio_sel.expect("audio").format_id, "a");
    assert!(!plan.audio_only);
    assert!(plan.hls_sel.is_none());
}

#[test]
fn plan_satisfied_audio_only_request_untouched() {
    // A fulfilled audio-only request keeps its track even when a muxed
    // file is also listed: adoption must not swap it out.
    let video = test_video(serde_json::json!([
        test_format_full("a", "none", "mp4a.40.2", None, None, "https", false),
        test_format_full(
            "m",
            "avc1.64001f",
            "mp4a.40.2",
            Some(720),
            None,
            "https",
            false
        ),
    ]));
    let plan = plan_streams(&video, "1080p", true, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert_eq!(plan.audio_sel.expect("audio").format_id, "a");
    assert!(plan.audio_only);
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
    let plan = plan_streams(&video, "720p", false, None, true, 1);
    assert!(plan.video_sel.is_none());
    assert!(plan.audio_sel.is_none());
    assert!(!plan.audio_only);
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
        select_hls_format(&formats, Some(720)).expect("hls").url,
        "https://cdn.example/h720"
    );
    assert_eq!(
        select_hls_format(&formats, Some(480)).expect("hls").url,
        "https://cdn.example/h480"
    );
    // Above every variant still takes the tallest.
    assert_eq!(
        select_hls_format(&formats, Some(2160)).expect("hls").url,
        "https://cdn.example/h1080"
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
        audio_only: false,
        dest: std::path::PathBuf::from("/tmp/dl/v.mp4"),
        tries: 3,
        timeout_secs: 60,
        user_agent: "Grab-test/1.0".into(),
        video_format_id: None,
        is_live: false,
        newest_codecs: true,
        cookies_browser: "none".into(),
    }
}

#[test]
fn part_argv_pins_format_output_and_page() {
    let job = part_test_job();
    let out = std::path::Path::new("/tmp/staging/video.mp4");
    let argv = part_download_argv(&job, "hls-720", out);
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
    let argv = part_download_argv(&job, "dl", std::path::Path::new("/tmp/staging/dl.mp4"));
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
    // Adopted single files degrade to best-single, never an audio-only
    // track; genuine audio-only legs prefer audio.
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
echo "[download] 100.0% of 4.00B in 00:00"
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

// ── live resolution via yt-dlp dump ──────────────────────────────────

/// Canned `--dump-single-json` over an HLS master: two variants, a
/// page-level HLS audio rendition, and a video-level Referer.
fn master_dump_json() -> serde_json::Value {
    serde_json::json!({
        "id": "live",
        "title": "Live",
        "http_headers": {"Referer": "https://www.tiktok.com/"},
        "formats": [
            {
                "format": "h480",
                "format_id": "h480",
                "protocol": "m3u8_native",
                "ext": "mp4",
                "url": "https://cdn.example/v480.m3u8",
                "vcodec": "avc1",
                "acodec": "mp4a.40.2",
                "height": 480,
                "http_headers": {},
            },
            {
                "format": "h1080",
                "format_id": "h1080",
                "protocol": "m3u8_native",
                "ext": "mp4",
                "url": "https://cdn.example/v1080.m3u8",
                "vcodec": "avc1",
                "acodec": "mp4a.40.2",
                "height": 1080,
                "http_headers": {},
            },
            {
                "format": "haudio",
                "format_id": "haudio",
                "protocol": "m3u8_native",
                "ext": "m4a",
                "url": "https://cdn.example/a.m3u8",
                "vcodec": "none",
                "acodec": "mp4a.40.2",
                "http_headers": {},
            },
        ],
    })
}

#[test]
fn live_input_from_dump_picks_height_and_referer() {
    let value = master_dump_json();
    let input = live_input_from_dump(&value, Some(720)).expect("resolves");
    assert_eq!(input.video, "https://cdn.example/v1080.m3u8");
    assert_eq!(input.audio.as_deref(), Some("https://cdn.example/a.m3u8"));
    assert_eq!(input.referer.as_deref(), Some("https://www.tiktok.com/"));
    // No cap takes the tallest; no variants at all stays unresolved.
    let tall = live_input_from_dump(&value, None).expect("resolves");
    assert_eq!(tall.video, "https://cdn.example/v1080.m3u8");
    let direct = test_video(serde_json::json!([test_format_full(
        "v",
        "avc1",
        "none",
        Some(720),
        None,
        "https",
        false
    ),]));
    let direct_value = serde_json::to_value(&direct).unwrap();
    assert!(live_input_from_dump(&direct_value, Some(720)).is_none());
}

#[test]
fn extract_referer_prefers_format_level() {
    let mut value = master_dump_json();
    assert_eq!(
        extract_referer(&value, "https://cdn.example/v480.m3u8").as_deref(),
        Some("https://www.tiktok.com/")
    );
    // Format-level wins over video-level.
    value["formats"][0]["http_headers"] = serde_json::json!({"Referer": "https://page.example/"});
    assert_eq!(
        extract_referer(&value, "https://cdn.example/v480.m3u8").as_deref(),
        Some("https://page.example/")
    );
    // Unknown URLs fall back to video-level; CR/LF values are rejected.
    assert_eq!(
        extract_referer(&value, "https://cdn.example/nope.m3u8").as_deref(),
        Some("https://www.tiktok.com/")
    );
    value["http_headers"] = serde_json::json!({"Referer": "https://evil.example/\r\nX: 1"});
    assert!(extract_referer(&value, "https://cdn.example/nope.m3u8").is_none());
    value.as_object_mut().unwrap().remove("http_headers");
    value["formats"][0]["http_headers"] = serde_json::json!({});
    assert!(extract_referer(&value, "https://cdn.example/v480.m3u8").is_none());
}

#[test]
fn resolve_hls_input_prefers_binary_dump() {
    // Fake yt-dlp emitting the canned dump: resolution, audio and
    // Referer all come from the binary, no playlist fetch involved.
    let dir = std::env::temp_dir().join(format!("grab-fakeresolve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("dump.json"), master_dump_json().to_string()).unwrap();
    let bin = dir.join("fake-ytdlp");
    std::fs::write(&bin, "#!/bin/sh\ncat \"$(dirname \"$0\")/dump.json\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let headers = yt_dlp::model::format::HttpHeaders {
        user_agent: "".into(),
        accept: "".into(),
        accept_language: "".into(),
        sec_fetch_mode: "".into(),
    };
    let input = crate::download::tokio_rt().block_on(resolve_hls_input(
        &bin,
        "https://cdn.example/master.m3u8",
        &headers,
        "Grab-test/1.0",
        "none",
        Some(720),
    ));
    assert_eq!(input.video, "https://cdn.example/v1080.m3u8");
    assert_eq!(input.audio.as_deref(), Some("https://cdn.example/a.m3u8"));
    assert_eq!(input.referer.as_deref(), Some("https://www.tiktok.com/"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── default combo selection ──────────────────────────────────────────

fn test_options() -> Vec<VideoFormatOption> {
    [480u32, 720, 1080]
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
    // Best (and empty listings) stay on "Best match".
    assert_eq!(default_quality_index(&opts, "best"), 0);
    assert_eq!(default_quality_index(&[], "720p"), 0);
    // Otherwise the closest listed height wins (combo index 1-based).
    assert_eq!(default_quality_index(&opts, "1080p"), 3);
    assert_eq!(default_quality_index(&opts, "720p"), 2);
    assert_eq!(default_quality_index(&opts, "480p"), 1);
    // Between buckets the nearer height wins, ties go taller.
    assert_eq!(default_quality_index(&opts, "2160p"), 3);
    // Unknown stored values degrade like the extractor selector (1080p).
    assert_eq!(default_quality_index(&opts, "mystery"), 3);
    // Exact ties (odd extractor heights equidistant from the cap) go taller.
    let odd = [600u32, 840]
        .iter()
        .map(|h| VideoFormatOption {
            id: format!("v{h}"),
            label: format!("{h}p"),
            height: *h,
        })
        .collect::<Vec<_>>();
    assert_eq!(default_quality_index(&odd, "720p"), 2);
}
