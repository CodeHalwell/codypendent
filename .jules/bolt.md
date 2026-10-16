## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-11-20 - Token Counting Optimization
**Learning:** In Rust, `.chars().count()` on pure ASCII strings (like framing punctuation, JSON strings, or tool names) incurs unnecessary UTF-8 decoding overhead compared to `.len()`.
**Action:** Replaced `.chars().count()` with `.len()` in `crates/runtime/src/agent.rs` for estimating token counts. This provides an O(1) calculation for pure ASCII framing instead of O(N), improving performance for text measurement operations without changing functionality.
