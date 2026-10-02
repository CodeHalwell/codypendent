## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-05-18 - Fast ASCII Validation
**Learning:** For strings that are expected to be purely ASCII, using `.chars()` introduces unnecessary UTF-8 decoding overhead because Rust has to inspect bytes to determine character boundaries.
**Action:** Use `.bytes().enumerate()` instead of `.chars()` for ASCII-only validation. It's safe to reconstruct the offending character in the error path using `s[i..].chars().next().unwrap()` because the first byte of any multi-byte UTF-8 sequence is a valid character boundary.
