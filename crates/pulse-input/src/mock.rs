//! In-memory backend for tests and benchmarks. Records every submitted event
//! and can inject failures. Never touches the OS.

use std::sync::{Arc, Mutex};

use pulse_core::{BackendKind, HeldInputs, LogicalInput};

use crate::backend::{BackendHealth, InputBackend, InputError};

#[derive(Debug, Default)]
pub struct MockLog {
    /// `(batch index, event)` for every submitted event, releases included.
    pub events: Vec<(u64, LogicalInput)>,
    pub batches: u64,
    pub release_all_calls: u64,
    pub dropped: bool,
}

#[derive(Debug)]
pub struct MockBackend {
    kind: BackendKind,
    log: Arc<Mutex<MockLog>>,
    held: HeldInputs,
    /// Fail every `emit` once this many batches have succeeded.
    pub fail_after_batches: Option<u64>,
    pub fail_release: bool,
    dead: bool,
}

impl MockBackend {
    pub fn new(kind: BackendKind) -> (MockBackend, Arc<Mutex<MockLog>>) {
        let log = Arc::new(Mutex::new(MockLog::default()));
        (
            MockBackend {
                kind,
                log: Arc::clone(&log),
                held: HeldInputs::new(),
                fail_after_batches: None,
                fail_release: false,
                dead: false,
            },
            log,
        )
    }

    fn with_log<R>(&self, f: impl FnOnce(&mut MockLog) -> R) -> R {
        let mut guard = match self.log.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}

impl InputBackend for MockBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn emit(&mut self, events: &[LogicalInput]) -> Result<(), InputError> {
        if self.dead {
            return Err(InputError::Disconnected("mock backend failed".into()));
        }
        let fail = self.with_log(|l| self.fail_after_batches.is_some_and(|n| l.batches >= n));
        if fail {
            self.dead = true;
            return Err(InputError::Submit("injected mock failure".into()));
        }
        self.with_log(|l| {
            let b = l.batches;
            l.events.extend(events.iter().map(|e| (b, *e)));
            l.batches += 1;
        });
        for e in events {
            self.held.apply_lenient(e);
        }
        Ok(())
    }

    fn release_all(&mut self, extra: &HeldInputs) -> Result<(), InputError> {
        let all = self.held.union(extra);
        self.with_log(|l| l.release_all_calls += 1);
        if self.fail_release {
            return Err(InputError::Submit("injected release failure".into()));
        }
        self.with_log(|l| {
            let b = l.batches;
            l.events.extend(all.releases().map(|e| (b, e)));
            l.batches += 1;
        });
        self.held.clear();
        Ok(())
    }

    fn held(&self) -> HeldInputs {
        self.held
    }

    fn check(&mut self) -> Result<(), InputError> {
        if self.dead {
            Err(InputError::Disconnected("mock backend failed".into()))
        } else {
            Ok(())
        }
    }

    fn supports(&self, _set: &HeldInputs) -> Result<(), InputError> {
        Ok(())
    }

    fn health(&self) -> BackendHealth {
        BackendHealth {
            kind: self.kind,
            healthy: !self.dead,
            detail: "mock backend".into(),
            last_error: None,
        }
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.with_log(|l| l.dropped = true);
    }
}
