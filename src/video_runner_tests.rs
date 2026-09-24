//! Tests for the live-capture runner's private cleanup policy.
//!
//! Reaches items `crate::video`'s facade cannot: the sweep helper and
//! the exit/staging enums are private to `video_runner`, so their
//! retention contract is only observable from in here.

use super::*;

/// `Exit::RenameFailed` is the one exit where the recorded media is the
/// user's only copy of a finished capture: the row just fails, nothing
/// re-records, and the rename could not place the remux.
///
/// Pinned directly because the end-to-end fixture that reaches this exit
/// has to destroy the destination directory in order to fail the rename,
/// which takes the raw shell with it. This drives the same `matches!` the
/// runner uses, so adding `RenameFailed` to the drop set fails here.
#[test]
fn rename_failed_exit_keeps_media_and_staging_but_sweeps_state() {
    let dir = std::env::temp_dir().join(format!("grab-sweep-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    // Both dest-side media shapes: yt-dlp renames `.part` to the plain
    // path on a clean exit and leaves the shell when killed.
    let out = dir.join("v.live.mp4");
    let part = dir.join("v.live.mp4.part");
    let state = dir.join("v.live.mp4.ytdl");
    let final_tmp = staging.join("final.mp4");
    std::fs::write(&out, b"finalized").unwrap();
    std::fs::write(&part, b"shell").unwrap();
    std::fs::write(&state, b"fragment-3").unwrap();
    std::fs::write(&final_tmp, b"recorded").unwrap();

    crate::runtime::tokio_rt().block_on(async {
        sweep_live_capture(
            &out,
            &part,
            &state,
            &staging,
            Staging::Keep,
            Exit::RenameFailed,
        )
        .await;
    });

    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"finalized",
        "the finalized capture shell is the user's only copy and must survive"
    );
    assert_eq!(
        std::fs::read(&part).unwrap(),
        b"shell",
        "the raw .part shell must survive for salvage"
    );
    assert!(
        !state.exists(),
        "the .ytdl state file is scratch on every exit and must be swept"
    );
    assert_eq!(
        std::fs::read(&final_tmp).unwrap(),
        b"recorded",
        "the completed remux must survive: Staging::Keep means no directory sweep"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scratch sweep must not begin until the recorder has been reaped.
/// Sweeping first could delete a file the recorder is still writing.
///
/// A real `Child` cannot demonstrate this: both orders leave the same end
/// state whenever the reap is quick, and there is no hook in between. So
/// the sequence is driven here by controlled futures instead.
#[test]
fn the_sweep_follows_the_reap() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let (reap_log, sweep_log) = (log.clone(), log.clone());
    crate::runtime::tokio_rt().block_on(reap_then_sweep(
        async move {
            reap_log.borrow_mut().push("reap");
        },
        move || async move {
            sweep_log.borrow_mut().push("sweep");
        },
    ));
    assert_eq!(
        *log.borrow(),
        ["reap", "sweep"],
        "the recorder must be reaped before its scratch is reclaimed"
    );
}

/// The non-vacuity guard for the ordering: a reap that never completes
/// must stall the sweep indefinitely. Any implementation that ran the two
/// concurrently, or swept first, would reclaim scratch out from under a
/// recorder that is demonstrably still running.
#[test]
fn a_stalled_reap_blocks_the_sweep() {
    let swept = std::rc::Rc::new(std::cell::Cell::new(false));
    let sweep_flag = swept.clone();
    crate::runtime::tokio_rt().block_on(async {
        tokio::select! {
            _ = reap_then_sweep(
                std::future::pending::<()>(),
                move || async move { sweep_flag.set(true); },
            ) => {}
            // Nothing can satisfy the reap arm, so this is the only way
            // out: give the sequence a real window to misbehave.
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    });
    assert!(
        !swept.get(),
        "the sweep ran while the recorder had not been reaped: scratch would be \
         deleted under a live writer"
    );
}
