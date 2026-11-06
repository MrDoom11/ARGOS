use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) struct Cadence {
    deadline: Instant,
    interval: Duration,
}

impl Cadence {
    pub(crate) fn new(now: Instant, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "o intervalo de coleta deve ser positivo"
        );
        Self {
            deadline: now,
            interval,
        }
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn complete(&mut self, at: Instant) {
        self.deadline = at + self.interval;
    }
}

pub(crate) struct History<T> {
    points: VecDeque<(Instant, T)>,
    retention: Duration,
    capacity: usize,
}

impl<T> History<T> {
    pub(crate) fn new(retention: Duration, capacity: usize) -> Self {
        Self {
            points: VecDeque::with_capacity(capacity),
            retention,
            capacity,
        }
    }

    pub(crate) fn push(&mut self, at: Instant, value: T) {
        debug_assert!(
            self.points
                .back()
                .is_none_or(|(previous, _)| *previous <= at)
        );
        self.prune(at);
        if self.capacity == 0 || self.retention.is_zero() {
            return;
        }
        if self.points.len() == self.capacity {
            self.points.pop_front();
        }
        self.points.push_back((at, value));
    }

    pub(crate) fn prune(&mut self, at: Instant) {
        while self.points.front().is_some_and(|(timestamp, _)| {
            at.saturating_duration_since(*timestamp) >= self.retention
        }) {
            self.points.pop_front();
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.points.len()
    }

    pub(crate) fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Instant, &T)> + ExactSizeIterator + '_ {
        self.points
            .iter()
            .map(|(timestamp, value)| (*timestamp, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_collection_waits_a_full_interval_before_the_next() {
        let start = Instant::now();
        let interval = Duration::from_millis(101);
        let mut cadence = Cadence::new(start, interval);
        assert_eq!(cadence.deadline(), start);

        let completion = start + Duration::from_secs(5);
        cadence.complete(completion);
        assert_eq!(cadence.deadline(), completion + interval);
    }

    #[test]
    fn expiration_preserves_a_gap_and_a_valid_zero_inside_the_window() {
        let start = Instant::now();
        let mut history = History::new(Duration::from_secs(3), 10);
        history.push(start, Some(12));
        history.push(start + Duration::from_secs(1), None);
        history.push(start + Duration::from_secs(2), Some(0));

        history.prune(start + Duration::from_secs(3));
        let points: Vec<_> = history.iter().map(|(at, value)| (at, *value)).collect();
        assert_eq!(
            points,
            vec![
                (start + Duration::from_secs(1), None),
                (start + Duration::from_secs(2), Some(0)),
            ]
        );

        history.prune(start + Duration::from_secs(5));
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn full_history_replaces_the_oldest_point() {
        let start = Instant::now();
        let mut history = History::new(Duration::from_secs(60), 2);
        for value in 0..10 {
            history.push(start + Duration::from_secs(value), value);
        }

        assert_eq!(history.len(), 2);
        assert!(history.iter().map(|(_, value)| *value).eq([8, 9]));
    }
}
