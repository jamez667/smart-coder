//! SWE-bench: the industry benchmark, run through smart-coder's own agent loop.
//!
//! Spec 07 sequences this as *a `sc-eval` SWE-bench adapter + a tiny pure-Python
//! subset*, and names three preconditions. Two had landed (`sc-index` retrieval,
//! structured pytest parsing in `sc-verify`); the third — per-task environment
//! isolation — turned out not to need building at all: the benchmark publishes a
//! pre-built image per instance, carrying the repo at its base commit with pinned
//! dependencies. See [`instance::SweInstance::image`].
//!
//! What this module adds over [`crate::runner`] is the grading model. `run_task`
//! scores a workspace as a single boolean — the verifier exited 0 or it did not.
//! SWE-bench asks two questions instead: did the named failing tests start passing
//! (FAIL_TO_PASS), and did the named passing tests keep passing (PASS_TO_PASS). A
//! boolean cannot express that, so scoring here is by *test name* — see
//! [`score::SweScore`], and [`runner::PYTEST_FLAGS`] for the flag that makes those
//! names available.
//!
//! The TDD invariants of [`crate::runner`] are kept, translated:
//!
//! - **verify-red-first** — the image ships the *old* tests; applying the instance's
//!   test patch is what turns it red. An instance already green at setup is reported
//!   as unusable, never as solved ([`score::SweScore::is_red_start`]).
//! - **frozen contract tests** — enforced structurally: only the source subtree is
//!   copied to the host, so the agent never has a test file to edit. Asserted anyway
//!   with a `git diff` after the solve.
//! - **green-after-solve** — [`score::SweScore::resolved`], SWE-bench's own strict
//!   definition.

pub mod agent;
pub mod container;
pub mod instance;
pub mod runner;
pub mod score;
pub mod solver;

pub use agent::SweAgentSolver;
pub use instance::{Subset, SweInstance};
pub use runner::{run_instance, InstanceRun, SolveReport, SweSolver};
pub use score::SweScore;
pub use solver::{GoldPatchSolver, NoopSweSolver};
