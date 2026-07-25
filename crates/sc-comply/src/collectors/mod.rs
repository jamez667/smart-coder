//! The deterministic built-in collectors.
//!
//! Each handles one or two check kinds and nothing else. A future
//! retrieval/LLM-backed collector joins this set by implementing
//! [`Collector`](crate::collector::Collector) — registry order decides
//! precedence, so it can also shadow a built-in where the built-in is blind
//! (an unindexable language, say).

pub mod command;
pub mod file;
pub mod structured;
pub mod symbol;
pub mod text;

pub use command::CommandCollector;
pub use file::FileCollector;
pub use structured::StructuredCollector;
pub use symbol::SymbolCollector;
pub use text::RegexCollector;
