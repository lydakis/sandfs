use std::collections::{HashMap, hash_map};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// How often finished transactions older than the retention period are dropped.
/// Checking them on every call made each call cost as much as the number of calls in
/// the retention period, so a busy client spent most of the server's time here.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(1);

/// `TransactionTracker` tracks the state of transactions to detect retransmissions.
pub struct TransactionTracker {
    retention_period: Duration,
    transactions: Mutex<Transactions>,
}

struct Transactions {
    states: HashMap<(u32, String), TransactionState>,
    last_housekeeping: SystemTime,
}

impl TransactionTracker {
    pub fn new(retention_period: Duration) -> Self {
        Self {
            retention_period,
            transactions: Mutex::new(Transactions { states: HashMap::new(), last_housekeeping: SystemTime::now() }),
        }
    }

    /// Checks if the transaction is a retransmission.
    /// If it's a new transaction, it is marked as `InProgress`.
    ///
    /// Returns `true` if the transaction is a retransmission, `false` otherwise.
    pub fn is_retransmission(&self, xid: u32, client_addr: &str) -> bool {
        let key = (xid, client_addr.to_string());
        let mut transactions = self.transactions.lock().expect("unable to unlock transactions mutex");
        let now = SystemTime::now();
        if now.duration_since(transactions.last_housekeeping).map_or(true, |since| since >= HOUSEKEEPING_INTERVAL) {
            housekeeping(&mut transactions.states, now, self.retention_period);
            transactions.last_housekeeping = now;
        }
        if let hash_map::Entry::Vacant(e) = transactions.states.entry(key) {
            e.insert(TransactionState::InProgress);
            false
        } else {
            true
        }
    }

    /// Marks the transaction as processed.
    pub fn mark_processed(&self, xid: u32, client_addr: &str) {
        let key = (xid, client_addr.to_string());
        let completion_time = SystemTime::now();
        let mut transactions = self.transactions.lock().expect("unable to unlock transactions mutex");
        if let Some(tx) = transactions.states.get_mut(&key) {
            *tx = TransactionState::Completed(completion_time);
        }
    }
}

fn housekeeping(transactions: &mut HashMap<(u32, String), TransactionState>, now: SystemTime, max_age: Duration) {
    let cutoff = now - max_age;
    transactions.retain(|_, v| match v {
        TransactionState::InProgress => true,
        TransactionState::Completed(completion_time) => *completion_time >= cutoff,
    });
}

pub enum TransactionState {
    InProgress,
    Completed(SystemTime),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retransmissions_are_caught_and_old_ones_dropped() {
        let tracker = TransactionTracker::new(Duration::from_millis(50));
        assert!(!tracker.is_retransmission(1, "client"));
        assert!(tracker.is_retransmission(1, "client"), "still in progress");
        tracker.mark_processed(1, "client");
        assert!(tracker.is_retransmission(1, "client"), "answered within the retention period");
        assert!(!tracker.is_retransmission(1, "other"), "another client's");
        // Past the retention period and the next housekeeping, it is new again.
        std::thread::sleep(HOUSEKEEPING_INTERVAL + Duration::from_millis(100));
        assert!(!tracker.is_retransmission(1, "client"));
    }
}
