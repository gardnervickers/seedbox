//! Cooperative cancellation on SIGINT, SIGTERM and SIGHUP.

use seedbox::{execution::CancellationToken, Result};

pub struct Signals(Vec<signal_hook::SigId>);

impl Signals {
    pub fn register(token: &CancellationToken) -> Result<Self> {
        let mut handlers = Self(Vec::new());
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            let token = token.clone();
            // SAFETY: the callback performs only an atomic store, which is
            // async-signal-safe; cleanup happens in normal executor code
            // after the VMM is stopped and reaped.
            handlers
                .0
                .push(unsafe { signal_hook::low_level::register(signal, move || token.cancel()) }?);
        }
        Ok(handlers)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
