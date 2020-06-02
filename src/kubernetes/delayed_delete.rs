//! A delayed delete logic.

use super::state;
use k8s_openapi::{apimachinery::pkg::apis::meta::v1::ObjectMeta, Metadata};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub struct DelayedDelete<T> {
    queue: VecDeque<(T, Instant)>,
}

impl<T> DelayedDelete<T> {
    /// Create a new [`DelayedDelete`] state.
    pub fn new() -> Self {
        let queue = VecDeque::new();
        Self { queue }
    }

    /// Schedules the delayed deletion of the item at the future.
    pub fn schedule_delete(&mut self, item: T, delete_in: Duration) {
        let deadline = Instant::now() + delete_in;
        self.queue.push_back((item, deadline));
    }

    /// Clear the delayed deletion requests.
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// Reset the queue.
    pub fn perform(&mut self, state_writer: &mut impl state::Write<Item = T>)
    where
        T: Metadata<Ty = ObjectMeta>,
    {
        let now = Instant::now();
        while let Some(deadline) = self.next_deadline() {
            if deadline > now {
                break;
            }
            let (item, _) = self.queue.pop_front().unwrap();
            state_writer.delete(item);
        }
    }

    /// Obtain the next deadline.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue.front().map(|(_, instant)| *instant)
    }
}
