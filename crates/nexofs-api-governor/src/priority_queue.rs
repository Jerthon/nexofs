//! Fila de prioridade determinística (SPEC §7.4 "fila de prioridade").
//!
//! Menor `Priority` é despachada primeiro; em empate, ordem de chegada
//! (FIFO) é preservada — importante para não inanir operações da mesma
//! classe quando várias chegam juntas.

use crate::scope::Priority;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

struct Entry<T> {
    priority: Priority,
    sequence: u64,
    item: T,
}

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl<T> Eq for Entry<T> {}

impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

pub struct PriorityQueue<T> {
    heap: BinaryHeap<Reverse<Entry<T>>>,
    next_sequence: u64,
}

impl<T> Default for PriorityQueue<T> {
    fn default() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_sequence: 0,
        }
    }
}

impl<T> PriorityQueue<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, priority: Priority, item: T) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.heap.push(Reverse(Entry {
            priority,
            sequence,
            item,
        }));
    }

    pub fn pop(&mut self) -> Option<T> {
        self.heap.pop().map(|Reverse(entry)| entry.item)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::OperationClass;

    #[test]
    fn interactive_download_dispatched_before_background_index() {
        let mut queue = PriorityQueue::new();
        queue.push(OperationClass::BackgroundIndex.default_priority(), "background");
        queue.push(OperationClass::InteractiveDownload.default_priority(), "interactive");

        assert_eq!(queue.pop(), Some("interactive"));
        assert_eq!(queue.pop(), Some("background"));
    }

    #[test]
    fn equal_priority_preserves_arrival_order() {
        let mut queue = PriorityQueue::new();
        let p = OperationClass::Upload.default_priority();
        queue.push(p, "first");
        queue.push(p, "second");
        queue.push(p, "third");

        assert_eq!(queue.pop(), Some("first"));
        assert_eq!(queue.pop(), Some("second"));
        assert_eq!(queue.pop(), Some("third"));
    }

    #[test]
    fn manual_refresh_priority_overrides_class_default() {
        let mut queue = PriorityQueue::new();
        // Rastreamento de pasta ativa (ChangeTracking, prioridade 40) chega
        // primeiro, mas atualização manual (prioridade 30 explícita) deve
        // furar a fila mesmo sendo da mesma classe de operação.
        queue.push(OperationClass::ChangeTracking.default_priority(), "active-folder");
        queue.push(Priority::MANUAL_REFRESH, "manual-refresh");

        assert_eq!(queue.pop(), Some("manual-refresh"));
        assert_eq!(queue.pop(), Some("active-folder"));
    }
}
