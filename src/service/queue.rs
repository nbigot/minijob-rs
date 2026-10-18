//! Ordering structures of the scheduler.

use crate::model::Job;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Scheduling order of pending and queued jobs: priority descending, then creation time,
/// then id (UUIDv7 ids are time ordered; Go's unstable sort had no tie-break).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey {
    neg_priority: i16,
    created: i64,
    pub id: Uuid,
}

impl OrderKey {
    pub fn of(job: &Job) -> OrderKey {
        OrderKey {
            neg_priority: -i16::from(job.priority),
            created: job.creation_timestamp(),
            id: job.id,
        }
    }
}

/// Jobs ordered by a due time, FIFO for equal times; replaces Go's linked-list priority queue
/// (`pq/pq.go`, priority = −time) with O(log n) operations and removal by id.
#[derive(Debug, Default)]
pub struct TimeQueue {
    by_time: BTreeMap<(i64, u64), Uuid>,
    by_id: HashMap<Uuid, (i64, u64)>,
    seq: u64,
}

impl TimeQueue {
    /// Adds (or moves) a job, due at `time` (Unix ms).
    pub fn push(&mut self, id: Uuid, time: i64) {
        self.remove(id);
        let k = (time, self.seq);
        self.seq += 1;
        self.by_time.insert(k, id);
        self.by_id.insert(id, k);
    }

    /// Removes and returns the first job due at or before `now` (`DequeueWithPriority`).
    pub fn pop_due(&mut self, now: i64) -> Option<(Uuid, i64)> {
        let (&(time, seq), &id) = self.by_time.iter().next()?;
        if time > now {
            return None;
        }
        self.by_time.remove(&(time, seq));
        self.by_id.remove(&id);
        Some((id, time))
    }

    pub fn remove(&mut self, id: Uuid) -> bool {
        match self.by_id.remove(&id) {
            Some(k) => {
                self.by_time.remove(&k);
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.by_time.clear();
        self.by_id.clear();
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Ids in due order.
    pub fn ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.by_time.values().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    // Port of pq_test.go. Go priorities are negated due times: priority 2 → time -2.
    #[test]
    fn enqueue_order_is_fifo_for_ties() {
        let mut q = TimeQueue::default();
        q.push(ids(10), -1);
        q.push(ids(20), -2);
        q.push(ids(15), -1);
        q.push(ids(4), 0);
        assert_eq!(q.len(), 4);
        assert_eq!(
            q.ids().collect::<Vec<_>>(),
            [ids(20), ids(10), ids(15), ids(4)]
        );
    }

    #[test]
    fn dequeue_with_priority() {
        let mut q = TimeQueue::default();
        assert_eq!(q.pop_due(-1), None);
        q.push(ids(10), -1);
        q.push(ids(20), -2);
        assert_eq!(q.pop_due(-2), Some((ids(20), -2)));
        assert_eq!(q.pop_due(-2), None);
        assert_eq!(q.pop_due(-1), Some((ids(10), -1)));
        assert_eq!(q.pop_due(-1), None);
        assert!(q.is_empty());
    }

    #[test]
    fn remove_and_clear() {
        let mut q = TimeQueue::default();
        assert!(!q.remove(ids(10)));
        for n in [10, 20, 30, 40] {
            q.push(ids(n), 0);
        }
        assert!(q.remove(ids(20)));
        assert_eq!(q.ids().collect::<Vec<_>>(), [ids(10), ids(30), ids(40)]);
        q.push(ids(10), 5);
        assert_eq!(q.len(), 3);
        q.clear();
        assert!(q.is_empty());
    }

    #[test]
    fn order_key() {
        let mut a = Job {
            priority: 1,
            ..Default::default()
        };
        a.init(ids(2), 100);
        let mut b = Job::default();
        b.init(ids(1), 50);
        let mut c = Job {
            priority: 1,
            ..Default::default()
        };
        c.init(ids(3), 100);
        let mut keys = [OrderKey::of(&b), OrderKey::of(&c), OrderKey::of(&a)];
        keys.sort();
        assert_eq!(keys.map(|k| k.id), [ids(2), ids(3), ids(1)]);
    }
}
