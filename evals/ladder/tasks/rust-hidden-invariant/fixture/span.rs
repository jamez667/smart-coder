//! Byte-span arithmetic shared by everything that slices source text.

/// A half-open byte span `[start, end)` into some source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// The number of bytes this span covers.
    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

/// The span of the line containing byte offset `at`, excluding the newline that
/// ends it.
pub fn line_span(text: &str, at: usize) -> Span {
    let start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = match text[at..].find('\n') {
        Some(i) => at + i,
        None => text.len(),
    };
    Span::new(start, end)
}

/// How many chunks of at most `width` bytes it takes to cover `len` bytes.
///
/// A zero-length input still needs one (empty) chunk, so callers always have
/// something to render.
pub fn chunk_count(len: usize, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    len / width + 1
}
