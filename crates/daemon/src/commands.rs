//! Command handling and the crash-consistent write path (STEP 1.3).
//!
//! Populated by STEP 1.3: the six-step idempotent apply sequence
//! (validate → transactionally persist → perform effect → persist outcome →
//! publish).
