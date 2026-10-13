//! The segment table: which byte ranges exist, how far each got, and which ones a
//! connection is working on. Pure bookkeeping, no I/O.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// A connection that wrote nothing for this long (not waiting on the speed limit), while
/// others sit idle, loses the rest of its segment to one of them. So does one that has been
/// at it this long at less than half the speed of the idle one. Each time the same range
/// is taken over, its next owner gets twice as long, so connections that all need longer
/// than this for their first write still finish instead of taking it from one another.
pub(crate) const TAKE_OVER_AFTER: Duration = Duration::from_secs(3);

/// Byte range `[start, end)` of the file; bytes `[start, pos)` are written to disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub pos: u64,
    pub end: u64,
}

impl Segment {
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
}

/// What a connection does next.
#[derive(Debug, PartialEq)]
pub(crate) enum Claim {
    Work(usize),
    /// Nothing to take yet, but other connections are still working: ask again shortly, as
    /// one of them may stall and lose its segment.
    Wait,
    Leave,
}

pub(crate) struct Table {
    segs: Vec<Segment>,
    owned: Vec<bool>,
    /// When each segment last got a write (or a new owner).
    touched: Vec<Instant>,
    /// When each segment got its current owner, and its position then (the owner's speed on
    /// it as a whole, for when it asks for more work).
    claimed: Vec<(Instant, u64)>,
    /// The owner's recent speed: an older and a newer mark (time, position) of its writes,
    /// at least TAKE_OVER_AFTER apart once it has written that long. None before its first
    /// write, so the wait for a server's first byte does not count as slowness. Waits on the
    /// speed limit count, as they do in the asking connection's speed (`since_claim`).
    marks: Vec<Option<[(Instant, u64); 2]>>,
    /// Since when each owner waits on the speed limit: not stalled meanwhile.
    held: Vec<Option<Instant>>,
    /// How many times each segment's range was taken over before (`patience`).
    taken: Vec<u32>,
    /// Smallest piece a split may leave on either side.
    min_split: u64,
    /// Connections still running. Kept under the same lock as the segments, so the last
    /// connection can never step back and leave a segment without anyone to finish it.
    workers: usize,
    stepped_back: usize,
}

impl Table {
    pub fn new(segs: Vec<Segment>, min_split: u64) -> Self {
        let now = Instant::now();
        let owned = vec![false; segs.len()];
        let touched = vec![now; segs.len()];
        let claimed = segs.iter().map(|s| (now, s.pos)).collect();
        let marks = vec![None; segs.len()];
        let held = vec![None; segs.len()];
        let taken = vec![0; segs.len()];
        Table { segs, owned, touched, claimed, marks, held, taken, min_split, workers: 0, stepped_back: 0 }
    }

    pub fn whole(size: u64, min_split: u64) -> Self {
        Self::new(vec![Segment { start: 0, pos: 0, end: size }], min_split)
    }

    /// A new connection's first segment; None (and no connection) when there is nothing to
    /// split off.
    pub fn start(&mut self, now: Instant) -> Option<usize> {
        let i = self.claim(now)?;
        self.workers += 1;
        Some(i)
    }

    /// Releases `prev` and claims new work: an unowned unfinished segment if there is one,
    /// otherwise the second half of the owned segment with the most bytes left (IDM's
    /// dynamic segmentation), otherwise the rest of a segment whose connection has stalled
    /// or is far slower than this one (`take_over`; `speed` is this connection's, in bytes
    /// per second on its last segment). The owner of a split or taken segment stops at the
    /// new end.
    pub fn next(&mut self, prev: Option<usize>, speed: f64, now: Instant) -> Claim {
        if let Some(p) = prev {
            self.owned[p] = false;
        }
        if let Some(i) = self.claim(now).or_else(|| self.take_over(speed, now)) {
            return Claim::Work(i);
        }
        if self.active() > 0 {
            return Claim::Wait;
        }
        self.workers -= 1;
        Claim::Leave
    }

    fn claim(&mut self, now: Instant) -> Option<usize> {
        if let Some(i) = (0..self.segs.len()).find(|&i| !self.owned[i] && self.segs[i].remaining() > 0) {
            self.owned[i] = true;
            self.touched[i] = now;
            self.claimed[i] = (now, self.segs[i].pos);
            self.marks[i] = None;
            self.held[i] = None;
            return Some(i);
        }
        let (i, left) = (0..self.segs.len())
            .filter(|&i| self.owned[i])
            .map(|i| (i, self.segs[i].remaining()))
            .max_by_key(|&(_, left)| left)?;
        if left < self.min_split.saturating_mul(2) {
            return None;
        }
        let seg = &mut self.segs[i];
        let mid = seg.pos + left / 2;
        let tail = Segment { start: mid, pos: mid, end: seg.end };
        seg.end = mid;
        Some(self.push(tail, 0, now))
    }

    /// The whole unwritten rest of the busiest segment whose connection wrote nothing for
    /// its `patience`, or has had it that long and lately runs at less than half of `speed`
    /// (a connection that has to start the rest over gains only when it is that much
    /// faster; evenly slow connections keep their segments), as a new segment. Connections
    /// waiting on the speed limit keep theirs. The losing connection may still be writing a
    /// buffer it already received: those bytes land on the new segment's range with the
    /// same content (`commit` keeps its count within its new end), so nothing is lost or
    /// counted twice.
    fn take_over(&mut self, speed: f64, now: Instant) -> Option<usize> {
        let stalled = |i: usize| now.saturating_duration_since(self.touched[i]) >= self.patience(i);
        let slower = |i: usize| {
            now.saturating_duration_since(self.claimed[i].0) >= self.patience(i)
                && self.recent_speed(i).is_some_and(|recent| recent * 2.0 < speed)
        };
        let i = (0..self.segs.len())
            .filter(|&i| self.owned[i] && self.held[i].is_none() && self.segs[i].remaining() > 0)
            .filter(|&i| stalled(i) || slower(i))
            .max_by_key(|&i| self.segs[i].remaining())?;
        let seg = &mut self.segs[i];
        let rest = Segment { start: seg.pos, pos: seg.pos, end: seg.end };
        seg.end = seg.pos;
        let taken = self.taken[i] + 1;
        Some(self.push(rest, taken, now))
    }

    /// How long the owner of segment `i` gets before it can lose it: TAKE_OVER_AFTER,
    /// doubled for each time its range was taken over before (up to 32 times as long).
    fn patience(&self, i: usize) -> Duration {
        TAKE_OVER_AFTER * 2u32.pow(self.taken[i].min(5))
    }

    /// The owner's speed between its two marks (its last 3 to 6 s of writes, up to its
    /// last write: bytes still in its buffer do not count as slowness, and silence since
    /// is for `stalled`); None until it has been writing for TAKE_OVER_AFTER.
    fn recent_speed(&self, i: usize) -> Option<f64> {
        let [(since, from), (until, to)] = self.marks[i]?;
        let secs = until.saturating_duration_since(since);
        (secs >= TAKE_OVER_AFTER).then(|| to.saturating_sub(from) as f64 / secs.as_secs_f64())
    }

    fn push(&mut self, seg: Segment, taken: u32, now: Instant) -> usize {
        self.claimed.push((now, seg.pos));
        self.segs.push(seg);
        self.owned.push(true);
        self.touched.push(now);
        self.marks.push(None);
        self.held.push(None);
        self.taken.push(taken);
        self.segs.len() - 1
    }

    /// The connection on segment `i` waits on the speed limit (`true`), or got past it.
    pub fn hold(&mut self, i: usize, held: bool, now: Instant) {
        self.held[i] = held.then_some(now);
        self.touched[i] = now;
    }

    /// Bytes written on segment `i` since its owner got it, and since when: the owner's speed.
    pub fn since_claim(&self, i: usize) -> (u64, Instant) {
        let (since, from) = self.claimed[i];
        (self.segs[i].pos.saturating_sub(from), since)
    }

    /// The connection on segment `i` failed and leaves; another connection can take the segment.
    pub fn leave(&mut self, i: usize) {
        self.owned[i] = false;
        self.workers -= 1;
    }

    /// The server turned the connection on segment `i` away. It leaves and gives the segment
    /// back, unless it is the last connection, which has to keep trying.
    pub fn step_back(&mut self, i: usize) -> bool {
        if self.workers < 2 {
            return false;
        }
        self.stepped_back += 1;
        self.leave(i);
        true
    }

    pub fn stepped_back(&self) -> usize {
        self.stepped_back
    }

    pub fn get(&self, i: usize) -> &Segment {
        &self.segs[i]
    }

    /// Records `n` more bytes written; true when the segment is finished. Bytes past the end
    /// are only possible after a take-over, and belong to the segment that took them.
    pub fn commit(&mut self, i: usize, n: u64, now: Instant) -> bool {
        let seg = &mut self.segs[i];
        seg.pos = (seg.pos + n).min(seg.end);
        self.touched[i] = now;
        let (pos, done) = (seg.pos, seg.pos >= seg.end);
        let marks = self.marks[i].get_or_insert([(now, pos); 2]);
        if now.saturating_duration_since(marks[1].0) >= TAKE_OVER_AFTER {
            marks[0] = marks[1];
            marks[1] = (now, pos);
        }
        done
    }

    /// Starts a segment over (for servers without range support).
    pub fn reset(&mut self, i: usize) {
        self.segs[i].pos = self.segs[i].start;
    }

    /// Ends a segment of unknown length where the data stopped.
    pub fn close_at_pos(&mut self, i: usize) {
        self.segs[i].end = self.segs[i].pos;
    }

    pub fn downloaded(&self) -> u64 {
        self.segs.iter().map(|s| s.pos - s.start).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.segs.iter().all(|s| s.remaining() == 0)
    }

    /// Connections currently working on a segment.
    pub fn active(&self) -> usize {
        (0..self.segs.len()).filter(|&i| self.owned[i] && self.segs[i].remaining() > 0).count()
    }

    pub fn len(&self) -> usize {
        self.segs.len()
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(c: Claim) -> usize {
        match c {
            Claim::Work(i) => i,
            other => panic!("expected work, got {other:?}"),
        }
    }

    #[test]
    fn connections_split_the_largest_segment() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        assert_eq!(t.start(now), Some(0));
        assert_eq!(t.start(now), Some(1));
        assert_eq!((t.get(0).end, t.get(1).start, t.get(1).end), (50, 50, 100));
        assert_eq!(t.start(now), Some(2));
        // The tie between 0 and 1 goes to the last one; either is fine.
        assert_eq!((t.get(1).end, t.get(2).start, t.get(2).end), (75, 75, 100));
        assert_eq!(t.active(), 3);
    }

    #[test]
    fn split_starts_from_written_position() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        t.start(now);
        assert!(!t.commit(0, 60, now));
        assert_eq!(t.start(now), Some(1));
        assert_eq!((t.get(0).end, t.get(1).start), (80, 80));
        assert!(t.commit(0, 20, now));
        assert_eq!(t.downloaded(), 80);
    }

    #[test]
    fn no_split_below_twice_min_split() {
        let now = Instant::now();
        let mut t = Table::whole(19, 10);
        t.start(now);
        assert_eq!(t.start(now), None);
        let mut t = Table::whole(20, 10);
        t.start(now);
        assert_eq!(t.start(now), Some(1));
    }

    #[test]
    fn released_segments_are_claimed_first() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        t.start(now);
        t.start(now);
        t.leave(0);
        assert_eq!(t.next(None, 0.0, now), Claim::Work(0));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn finished_segment_is_released_and_work_continues() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 50, now));
        // b finished; claiming again splits what is left of a.
        let c = work(t.next(Some(b), 0.0, now));
        assert_eq!((t.get(a).end, t.get(c).start, t.get(c).end), (25, 25, 50));
        assert!(t.commit(a, 25, now));
        assert!(!t.commit(c, 10, now));
        // 15 bytes left in c, too few to split: a waits in case c stalls.
        assert_eq!(t.next(Some(a), 0.0, now), Claim::Wait);
        assert!(t.commit(c, 15, now));
        assert_eq!(t.next(Some(c), 0.0, now), Claim::Leave);
        assert_eq!(t.next(None, 0.0, now), Claim::Leave);
        assert!(t.is_complete());
        assert_eq!(t.downloaded(), 100);
    }

    #[test]
    fn last_connection_never_steps_back() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.step_back(b));
        assert!(!t.step_back(a));
        assert_eq!(t.stepped_back(), 1);
        // a finishes its half, then picks up the half b gave back.
        assert!(t.commit(a, 50, now));
        assert_eq!(t.next(Some(a), 0.0, now), Claim::Work(b));
        assert!(t.commit(b, 50, now));
        assert_eq!(t.next(Some(b), 0.0, now), Claim::Leave);
        assert!(t.is_complete());
    }

    #[test]
    fn stalled_connection_loses_the_rest_of_its_segment() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 50, now));
        assert!(!t.commit(a, 35, now));
        // 15 bytes left in a: too few to split; b waits while a is still writing.
        assert_eq!(t.next(Some(b), 0.0, now), Claim::Wait);
        assert_eq!(t.next(None, 0.0, now + TAKE_OVER_AFTER / 2), Claim::Wait);
        // a wrote nothing for TAKE_OVER_AFTER: b takes its rest.
        let later = now + TAKE_OVER_AFTER;
        let c = work(t.next(None, 0.0, later));
        assert_eq!((t.get(a).end, t.get(c).start, t.get(c).end), (35, 35, 50));
        // a was already writing 10 bytes it had received: they count once, for c's range.
        assert!(t.commit(a, 10, later));
        assert_eq!(t.get(a).pos, 35);
        assert!(t.commit(c, 15, later));
        assert_eq!(t.downloaded(), 100);
        assert!(t.is_complete());
        assert_eq!(t.next(Some(a), 0.0, later), Claim::Leave);
        assert_eq!(t.next(Some(c), 0.0, later), Claim::Leave);
    }

    #[test]
    fn writing_connection_keeps_its_segment() {
        let (mut t, now) = (Table::whole(100, 10), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 50, now));
        let later = now + TAKE_OVER_AFTER;
        assert!(!t.commit(a, 35, later - TAKE_OVER_AFTER / 3));
        assert_eq!(t.next(Some(b), 0.0, later), Claim::Wait);
    }

    /// A connection that keeps writing, slowly, loses its rest only to one at least twice
    /// as fast: one as slow would start the rest over and finish no sooner.
    #[test]
    fn slow_connection_loses_only_to_a_much_faster_one() {
        let s = |secs: f64| Duration::from_secs_f64(secs);
        let (mut t, now) = (Table::whole(1000, 300), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 500, now));
        // a's first byte came late (2 s): that wait is not slowness, and a has written for
        // less than TAKE_OVER_AFTER, so it is not judged yet.
        assert!(!t.commit(a, 10, now + s(2.0)));
        assert_eq!(t.next(Some(b), 1e9, now + s(4.0)), Claim::Wait);
        // Then 90 bytes in TAKE_OVER_AFTER (the last just now): slow, not stalled.
        let later = now + s(5.0);
        assert!(!t.commit(a, 90, later));
        let speed = 90.0 / TAKE_OVER_AFTER.as_secs_f64();
        assert_eq!(t.next(None, speed, later), Claim::Wait);
        assert_eq!(t.next(None, speed * 1.9, later), Claim::Wait);
        let c = work(t.next(None, speed * 2.5, later));
        assert_eq!((t.get(a).end, t.get(c).start, t.get(c).end), (100, 100, 500));
    }

    /// The speed that counts is the recent one: a connection that slows down after a fast
    /// start is not kept by its start.
    #[test]
    fn connection_slowing_down_late_loses_its_rest() {
        let s = |secs: f64| Duration::from_secs_f64(secs);
        let (mut t, now) = (Table::whole(10_000, 3000), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 5000, now));
        assert!(!t.commit(a, 500, now + s(1.0)));
        assert!(!t.commit(a, 500, now + s(4.0)));
        assert!(!t.commit(a, 10, now + s(7.0)));
        assert!(!t.commit(a, 10, now + s(10.0)));
        // 520 bytes since its first write 9 s ago (58 B/s), but 10 in the last 3 s.
        assert_eq!(work(t.next(Some(b), 100.0, now + s(10.0))), 2);
        assert_eq!(t.get(a).end, 1020);
    }

    /// Each time a range is taken over, its next owner gets twice as long: connections that
    /// all need longer than TAKE_OVER_AFTER for their first write do not take it from one
    /// another forever.
    #[test]
    fn a_range_taken_over_again_waits_twice_as_long() {
        let (mut t, now) = (Table::whole(1000, 300), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 500, now));
        assert_eq!(t.next(Some(b), 0.0, now), Claim::Wait);
        let first = now + TAKE_OVER_AFTER;
        let c = work(t.next(None, 0.0, first));
        assert_eq!((t.get(a).end, t.get(c).start), (0, 0));
        // c wrote nothing either, but gets 6 s, not 3.
        assert_eq!(t.next(None, 0.0, first + TAKE_OVER_AFTER), Claim::Wait);
        let second = first + TAKE_OVER_AFTER * 2;
        let d = work(t.next(None, 0.0, second));
        assert_eq!(t.get(d).start, 0);
        assert_eq!(t.next(None, 0.0, second + TAKE_OVER_AFTER * 3), Claim::Wait, "12 s for the third owner");
    }

    /// A connection waiting on the speed limit keeps its segment however long it waits, and
    /// counts as heard from when it gets past.
    #[test]
    fn connection_held_by_the_speed_limit_keeps_its_segment() {
        let (mut t, now) = (Table::whole(1000, 300), Instant::now());
        let a = t.start(now).unwrap();
        let b = t.start(now).unwrap();
        assert!(t.commit(b, 500, now));
        t.hold(a, true, now);
        let later = now + TAKE_OVER_AFTER * 2;
        assert_eq!(t.next(Some(b), 1e9, later), Claim::Wait);
        t.hold(a, false, later);
        // It has not written yet, so it is not judged slower either, however fast the asker.
        assert_eq!(t.next(None, 1e9, later + TAKE_OVER_AFTER / 2), Claim::Wait);
        assert_eq!(work(t.next(None, 0.0, later + TAKE_OVER_AFTER)), 2);
    }
}
