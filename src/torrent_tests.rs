use super::*;
use pretty_assertions::assert_eq;

const MAGNET: &str = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=test+file";
const BARE_HASH: &str = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3";

#[test]
fn classifies_magnets() {
    assert!(is_magnet("magnet:?xt=urn:btih:abc"));
    assert!(is_magnet("MAGNET:?xt=urn:btih:abc"));
    assert!(is_magnet("  magnet:?xt=urn:btih:abc"));
    assert!(!is_magnet("https://example.com/f.iso"));
    assert!(!is_magnet("magnetfoo"));
    assert!(!is_magnet(""));
}

#[test]
fn parses_valid_magnets() {
    let m = parse_magnet(MAGNET).unwrap();
    assert_eq!(m.name.as_deref(), Some("test file"));
    assert!(parse_magnet(BARE_HASH).is_ok());
    assert!(parse_magnet("magnet:?xt=urn:btih:xyz").is_err());
    assert!(parse_magnet("https://example.com/f.iso").is_err());
    assert!(parse_magnet("").is_err());
}

#[test]
fn classifier_never_panics_on_unicode() {
    assert!(!is_magnet("éééééééééé"));
    assert!(!is_magnet("é"));
    assert!(!is_magnet("magnet"));
}

#[test]
fn stub_prefers_name_then_hash() {
    assert_eq!(stub_name(MAGNET).as_deref(), Some("test file"));
    assert_eq!(
        stub_name(BARE_HASH).as_deref(),
        Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
    );
    assert_eq!(stub_name("magnet:?xt=urn:btih:xyz"), None);
}

#[test]
fn drop_selection_clears_finished_filter() {
    let url = "torrent:/tmp/grab-drop-test.torrent";
    stage_selection(url, vec![0, 2]);
    assert_eq!(get_selection(url), Some(vec![0, 2]));
    drop_selection(url);
    assert_eq!(get_selection(url), None);
    // Dropping a missing key is a no-op, never a panic.
    drop_selection(url);
}

#[test]
fn multifile_torrents_get_name_subfolder() {
    let dest = std::path::PathBuf::from("/tmp/dl");
    // Multi-file torrents land in a subfolder named after the torrent…
    let out = output_folder_for(&dest, Some("Cosmos Laundromat".to_string()), true, "abc123");
    assert_eq!(out, std::path::PathBuf::from("/tmp/dl/Cosmos Laundromat"));
    // …single-file torrents sit flat even when a name is known…
    assert_eq!(
        output_folder_for(&dest, Some("movie.mp4".to_string()), false, "abc123"),
        dest
    );
    // …and hostile names fall back to the info-hash, never the parent.
    let out = output_folder_for(&dest, Some("../evil".to_string()), true, "abc123");
    assert_eq!(out, std::path::PathBuf::from("/tmp/dl/abc123"));
    assert!(out.starts_with(&dest));
    assert_eq!(
        output_folder_for(&dest, None, true, "abc123"),
        std::path::PathBuf::from("/tmp/dl/abc123")
    );
}

#[test]
fn intake_plan_mirrors_engine_layout() {
    fn torrent_bytes(info: &str) -> Vec<u8> {
        // `info` leaves both dicts open; the two `e` close info then outer.
        format!("d8:announce32:https://tracker.example.com:80/a4:info{info}ee").into_bytes()
    }
    // Single file, sane name: flat layout.
    let single = torrent_bytes(
        "d6:lengthi5e4:name9:movie.mp412:piece lengthi16384e6:pieces20:01234567890123456789",
    );
    assert_eq!(intake_plan(&single), Some(("movie.mp4".to_string(), false)));
    // Multi-file: folder base is the torrent name.
    let multi = torrent_bytes(
        "d5:filesld6:lengthi5e4:pathl5:a.txteed6:lengthi5e4:pathl5:b.txteee4:name4:pack12:piece lengthi16384e6:pieces20:01234567890123456789",
    );
    assert_eq!(intake_plan(&multi), Some(("pack".to_string(), true)));
    // Hostile name: info-hash hex fallback, never a path.
    let evil = torrent_bytes("d6:lengthi0e4:name7:../evil12:piece lengthi16384e6:pieces0:");
    let (base, multi) = intake_plan(&evil).expect("parses");
    assert!(!multi);
    assert_eq!(base.len(), 40);
    assert!(base.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!base.contains('/'));
    // Garbage is not a torrent.
    assert_eq!(intake_plan(b"not a torrent"), None);
}

#[test]
fn trackers_split_and_schemeless_dropped() {
    assert_eq!(parse_trackers(""), None);
    assert_eq!(parse_trackers("  ,  "), None);
    assert_eq!(parse_trackers("not a url"), None);
    assert_eq!(
        parse_trackers("udp://t.one:1337/announce, https://t.two/announce"),
        Some(vec![
            "udp://t.one:1337/announce".to_string(),
            "https://t.two/announce".to_string()
        ])
    );
    // Newlines/spaces split too; schemeless typos never fail a download.
    assert_eq!(
        parse_trackers("udp://t.one/a\ntypo.example.com https://t.two/b"),
        Some(vec![
            "udp://t.one/a".to_string(),
            "https://t.two/b".to_string()
        ])
    );
}
