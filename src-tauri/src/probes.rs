use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Tracks the stop flag for each running probe task so targets can be added, restarted,
/// and removed at runtime. (History itself lives in the SQLite `Db`, not here.)
#[derive(Default)]
pub struct Probes {
    inner: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Probes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a target and return a fresh stop flag for its probe loop.
    pub fn start(&self, id: &str) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        self.inner.lock().unwrap().insert(id.to_string(), stop.clone());
        stop
    }

    /// Signal the target's probe loop to exit (if running).
    pub fn stop(&self, id: &str) {
        if let Some(flag) = self.inner.lock().unwrap().remove(id) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}
