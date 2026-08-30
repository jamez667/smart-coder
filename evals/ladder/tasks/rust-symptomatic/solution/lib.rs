//! A fixed-capacity ring buffer of readings, with a rolling mean.

pub struct Window {
    buf: Vec<i64>,
    cap: usize,
    head: usize,
    len: usize,
    sum: i64,
}

impl Window {
    pub fn new(cap: usize) -> Self {
        Window {
            buf: vec![0; cap],
            cap,
            head: 0,
            len: 0,
            sum: 0,
        }
    }

    /// Add a reading, evicting the oldest once the window is full.
    pub fn push(&mut self, v: i64) {
        if self.len == self.cap {
            self.sum -= self.buf[self.head];
        } else {
            self.len += 1;
        }
        self.buf[self.head] = v;
        self.sum += v;
        self.head = (self.head + 1) % self.cap;
    }

    /// Mean of the readings currently in the window, truncated toward zero.
    pub fn mean(&self) -> i64 {
        if self.len == 0 {
            return 0;
        }
        self.sum / self.len as i64
    }

    /// The readings, oldest first.
    pub fn values(&self) -> Vec<i64> {
        let mut out = Vec::with_capacity(self.len);
        // Oldest-first. When the window is full, head points AT the oldest slot; while
        // it is still filling, the oldest is slot 0 and head is one past the newest.
        let start = if self.len == self.cap {
            self.head
        } else {
            0
        };
        for i in 0..self.len {
            out.push(self.buf[(start + i) % self.cap]);
        }
        out
    }
}
