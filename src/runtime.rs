//! Shared async runtime and lock recovery for every engine. Leaf module
//! (std + tokio only) breaking the import fan-out toward `download`: the
//! HTTP, video and torrent engines all spawn on this runtime, and worker
//! threads share poison-recovering mutexes through it.

use std::sync::{Mutex, OnceLock};

pub(crate) fn tokio_rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("grab-download")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Lock a worker-shared mutex, recovering the guarded value when a
/// previous worker panic poisoned it. The item then fails with an error
/// instead of the panic cascading through every worker into the app.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
