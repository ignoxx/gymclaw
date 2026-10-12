//! Pure week planner: candidate slots, recovery, scoring (port of planning.py). Owner: lead.
use crate::time::Ts;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval {
    pub start: Ts,
    pub end: Ts,
}

/// Half-open intervals [start, end) overlap.
pub fn overlaps(start: Ts, end: Ts, other: &Interval) -> bool {
    start < other.end && other.start < end
}
