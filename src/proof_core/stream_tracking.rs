//! Pure model for per-stream cleanup bookkeeping.

#![deny(unsafe_code)]

pub(crate) const STREAM_TRACKING_SLOTS: usize = 4;
pub(crate) const BLOCKED_QUEUE_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamTrackingFlags {
    pub(crate) blocked_set: bool,
    pub(crate) blocked_queue_entries: usize,
    pub(crate) pending_body: bool,
    pub(crate) trailer_fin: bool,
    pub(crate) pending_write: bool,
    pub(crate) pending_response: bool,
}

impl StreamTrackingFlags {
    pub(crate) fn has_any_state(self) -> bool {
        self.blocked_set
            || self.blocked_queue_entries > 0
            || self.pending_body
            || self.trailer_fin
            || self.pending_write
            || self.pending_response
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamTrackingModel {
    blocked_set: [bool; STREAM_TRACKING_SLOTS],
    blocked_queue: [usize; BLOCKED_QUEUE_CAPACITY],
    blocked_queue_len: usize,
    pending_body: [bool; STREAM_TRACKING_SLOTS],
    trailer_fin: [bool; STREAM_TRACKING_SLOTS],
    pending_write: [bool; STREAM_TRACKING_SLOTS],
    pending_response: [bool; STREAM_TRACKING_SLOTS],
}

impl Default for StreamTrackingModel {
    fn default() -> Self {
        Self {
            blocked_set: [false; STREAM_TRACKING_SLOTS],
            blocked_queue: [0; BLOCKED_QUEUE_CAPACITY],
            blocked_queue_len: 0,
            pending_body: [false; STREAM_TRACKING_SLOTS],
            trailer_fin: [false; STREAM_TRACKING_SLOTS],
            pending_write: [false; STREAM_TRACKING_SLOTS],
            pending_response: [false; STREAM_TRACKING_SLOTS],
        }
    }
}

impl StreamTrackingModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn track_blocked(&mut self, stream_slot: usize) {
        let stream_slot = model_slot(stream_slot);
        if !self.blocked_set[stream_slot] {
            self.blocked_set[stream_slot] = true;
            self.push_blocked_queue_entry(stream_slot);
        }
    }

    pub(crate) fn inject_blocked_queue_entry(&mut self, stream_slot: usize) {
        self.push_blocked_queue_entry(model_slot(stream_slot));
    }

    pub(crate) fn track_pending_body(&mut self, stream_slot: usize) {
        self.pending_body[model_slot(stream_slot)] = true;
    }

    pub(crate) fn track_trailer_fin(&mut self, stream_slot: usize) {
        self.trailer_fin[model_slot(stream_slot)] = true;
    }

    pub(crate) fn track_pending_write(&mut self, stream_slot: usize) {
        self.pending_write[model_slot(stream_slot)] = true;
    }

    pub(crate) fn track_pending_response(&mut self, stream_slot: usize) {
        self.pending_response[model_slot(stream_slot)] = true;
    }

    pub(crate) fn cleanup_if_closed(&mut self, stream_slot: usize, closed: bool) {
        if closed {
            self.cleanup_closed_stream(stream_slot);
        }
    }

    pub(crate) fn cleanup_closed_stream(&mut self, stream_slot: usize) {
        self.cleanup_h3_closed_stream(stream_slot);
        self.cleanup_worker_closed_stream(stream_slot);
    }

    pub(crate) fn cleanup_h3_closed_stream(&mut self, stream_slot: usize) {
        let stream_slot = model_slot(stream_slot);
        self.blocked_set[stream_slot] = false;
        self.remove_blocked_queue_entries(stream_slot);
        self.pending_body[stream_slot] = false;
        self.trailer_fin[stream_slot] = false;
    }

    pub(crate) fn cleanup_worker_closed_stream(&mut self, stream_slot: usize) {
        let stream_slot = model_slot(stream_slot);
        self.pending_write[stream_slot] = false;
        self.pending_response[stream_slot] = false;
    }

    pub(crate) fn flags(&self, stream_slot: usize) -> StreamTrackingFlags {
        let stream_slot = model_slot(stream_slot);
        StreamTrackingFlags {
            blocked_set: self.blocked_set[stream_slot],
            blocked_queue_entries: self.blocked_queue_entries(stream_slot),
            pending_body: self.pending_body[stream_slot],
            trailer_fin: self.trailer_fin[stream_slot],
            pending_write: self.pending_write[stream_slot],
            pending_response: self.pending_response[stream_slot],
        }
    }

    pub(crate) fn has_any_state(&self, stream_slot: usize) -> bool {
        self.flags(stream_slot).has_any_state()
    }

    pub(crate) fn blocked_queue_entries(&self, stream_slot: usize) -> usize {
        let stream_slot = model_slot(stream_slot);
        let blocked_queue_len = self.blocked_queue_len.min(BLOCKED_QUEUE_CAPACITY);
        let mut count = 0usize;
        for idx in 0..BLOCKED_QUEUE_CAPACITY {
            if idx < blocked_queue_len && self.blocked_queue[idx] == stream_slot {
                count += 1;
            }
        }
        count
    }

    fn push_blocked_queue_entry(&mut self, stream_slot: usize) {
        if self.blocked_queue_len < BLOCKED_QUEUE_CAPACITY {
            self.blocked_queue[self.blocked_queue_len] = stream_slot;
            self.blocked_queue_len += 1;
        }
    }

    fn remove_blocked_queue_entries(&mut self, stream_slot: usize) {
        let blocked_queue_len = self.blocked_queue_len.min(BLOCKED_QUEUE_CAPACITY);
        let mut write = 0usize;

        for read in 0..BLOCKED_QUEUE_CAPACITY {
            if read < blocked_queue_len {
                let entry = self.blocked_queue[read];
                if entry != stream_slot {
                    self.blocked_queue[write] = entry;
                    write += 1;
                }
            }
        }

        for idx in 0..BLOCKED_QUEUE_CAPACITY {
            if idx >= write {
                self.blocked_queue[idx] = 0;
            }
        }

        self.blocked_queue_len = write;
    }
}

pub(crate) fn model_slot(stream_slot: usize) -> usize {
    stream_slot % STREAM_TRACKING_SLOTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_cleanup_removes_all_target_state() {
        let mut model = StreamTrackingModel::new();
        model.track_blocked(2);
        model.inject_blocked_queue_entry(2);
        model.track_pending_body(2);
        model.track_trailer_fin(2);
        model.track_pending_write(2);
        model.track_pending_response(2);

        model.cleanup_closed_stream(2);

        assert!(!model.has_any_state(2));
        assert_eq!(model.blocked_queue_entries(2), 0);
    }

    #[test]
    fn open_cleanup_is_noop() {
        let mut model = StreamTrackingModel::new();
        model.track_blocked(1);
        model.track_pending_body(1);
        let before = model;

        model.cleanup_if_closed(1, false);

        assert_eq!(model, before);
    }

    #[test]
    fn closed_cleanup_preserves_other_streams() {
        let mut model = StreamTrackingModel::new();
        model.track_blocked(1);
        model.inject_blocked_queue_entry(1);
        model.track_pending_write(1);
        let other_before = model.flags(1);

        model.track_blocked(2);
        model.track_pending_body(2);
        model.cleanup_closed_stream(2);

        assert_eq!(model.flags(1), other_before);
    }
}
