//! VM-global active network level, shared between the dispatch server (which
//! moves it via `set_mode`) and the DNS proxy thread (which reads it to decide
//! whether to filter or pass through). The nftables ruleset and the proxy are
//! both per-VM, not per-connection, so the active network level lives here as
//! shared atomic state rather than as per-connection local state (unlike the
//! command level, which is genuinely per-connection).

use std::sync::atomic::{AtomicU8, Ordering};

use crate::policy::NetworkLevel;

/// An atomically-updatable [`NetworkLevel`]. Cheap to share across threads
/// behind an `Arc`.
#[derive(Debug)]
pub struct ActiveNetwork(AtomicU8);

impl ActiveNetwork {
    pub fn new(level: NetworkLevel) -> Self {
        Self(AtomicU8::new(to_u8(level)))
    }

    pub fn get(&self) -> NetworkLevel {
        from_u8(self.0.load(Ordering::SeqCst))
    }

    pub fn set(&self, level: NetworkLevel) {
        self.0.store(to_u8(level), Ordering::SeqCst);
    }
}

fn to_u8(level: NetworkLevel) -> u8 {
    match level {
        NetworkLevel::None => 0,
        NetworkLevel::Allowlist => 1,
        NetworkLevel::Full => 2,
    }
}

fn from_u8(value: u8) -> NetworkLevel {
    match value {
        0 => NetworkLevel::None,
        1 => NetworkLevel::Allowlist,
        // The only writer is `set`, so any other value is unreachable; treat it
        // as the most-restrictive level rather than panicking on the read path.
        2 => NetworkLevel::Full,
        _ => NetworkLevel::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_level() {
        for level in [
            NetworkLevel::None,
            NetworkLevel::Allowlist,
            NetworkLevel::Full,
        ] {
            let active = ActiveNetwork::new(level);
            assert_eq!(active.get(), level);
        }
    }

    #[test]
    fn set_updates_the_level() {
        let active = ActiveNetwork::new(NetworkLevel::None);
        active.set(NetworkLevel::Full);
        assert_eq!(active.get(), NetworkLevel::Full);
    }
}
