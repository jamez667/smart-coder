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
        for i in 0..self.len {
            out.push(self.buf[(self.head + i) % self.cap]);
        }
        out
    }
}
