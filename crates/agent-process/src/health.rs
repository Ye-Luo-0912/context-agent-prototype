//! Connection health, epochs, and a bounded restart circuit (PLAT-06).
//!
//! This is process-connection state. It is never task state and never Core
//! authority. A quarantined connection cannot be reused; a restart builds a
//! new [`crate::ProcessHost`] / MCP child and increments the adapter's
//! restart count. Single-inflight stays in force.
//!
//! Peer cancel-ACK frames and coalescible progress are PLAT-06 slice 2.
//! Multiplexing remains later. Host-side cancel settlement is kill-then-reap.

use std::sync::atomic::{AtomicU32, Ordering};

use agent_contracts::{AgentError, AgentResult};

/// How many times an adapter may replace a quarantined child. The first
/// connect is not a restart. Three replacements bound a crash loop without
/// pretending the peer is transactional.
pub const DEFAULT_MAX_CONNECTION_RESTARTS: u32 = 3;

/// Serving condition of one process connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionHealth {
    /// Handshake succeeded; frames may be exchanged.
    Ready,
    /// Still serving, but impaired (stderr capture saturated).
    Degraded,
    /// No live child (adapter slot empty, or the host was consumed).
    NotServing,
    /// Poisoned: the tree was killed and the connection must not be reused.
    Quarantined,
}

impl ConnectionHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::NotServing => "not_serving",
            Self::Quarantined => "quarantined",
        }
    }

    pub fn allows_call(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }
}

/// Host and peer generations for one connection object.
///
/// `host` is 1 for a given [`crate::ProcessHost`]: a reconnect constructs a
/// new host. Adapters count replacements with [`RestartCircuit`]. `peer` is
/// the child's advertised epoch from ping, or 0 if it sent none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionEpoch {
    pub host: u64,
    pub peer: u64,
}

/// Snapshot of one connection. Bounded; not an event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub health: ConnectionHealth,
    pub epoch: ConnectionEpoch,
    /// First poison reason, or a degraded-signal note. Never task text.
    pub reason: Option<String>,
}

/// Bounds how many times an adapter may spawn a replacement child.
pub struct RestartCircuit {
    max_restarts: u32,
    used: AtomicU32,
}

impl RestartCircuit {
    pub fn new(max_restarts: u32) -> Self {
        Self {
            max_restarts,
            used: AtomicU32::new(0),
        }
    }

    pub fn max_restarts(&self) -> u32 {
        self.max_restarts
    }

    pub fn restarts_used(&self) -> u32 {
        self.used.load(Ordering::Relaxed)
    }

    /// Reserve one replacement of a quarantined child. The first connect
    /// must not call this.
    pub fn try_acquire(&self) -> AgentResult<u32> {
        let used = self.used.fetch_add(1, Ordering::Relaxed) + 1;
        if used > self.max_restarts {
            return Err(AgentError::Context(format!(
                "connection quarantined: restart budget exhausted ({used}/{})",
                self.max_restarts
            )));
        }
        Ok(used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_connect_is_not_a_restart() {
        let circuit = RestartCircuit::new(3);
        assert_eq!(circuit.restarts_used(), 0);
        assert_eq!(circuit.try_acquire().unwrap(), 1);
        assert_eq!(circuit.try_acquire().unwrap(), 2);
        assert_eq!(circuit.try_acquire().unwrap(), 3);
        let error = circuit.try_acquire().unwrap_err();
        assert!(
            error.to_string().contains("restart budget exhausted"),
            "{error}"
        );
        assert!(
            error.to_string().contains("quarantined"),
            "exhausted restart is a quarantine, not a task failure: {error}"
        );
    }

    #[test]
    fn zero_budget_refuses_the_first_replacement() {
        let circuit = RestartCircuit::new(0);
        let error = circuit.try_acquire().unwrap_err();
        assert!(error.to_string().contains("1/0"), "{error}");
    }

    #[test]
    fn health_ready_and_degraded_may_call() {
        assert!(ConnectionHealth::Ready.allows_call());
        assert!(ConnectionHealth::Degraded.allows_call());
        assert!(!ConnectionHealth::NotServing.allows_call());
        assert!(!ConnectionHealth::Quarantined.allows_call());
    }
}
