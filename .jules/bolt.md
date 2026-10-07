## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-05-18 - Avoid chars() overhead in pure ASCII validation
**Learning:** In Rust, `.chars()` incurs unnecessary UTF-8 decoding overhead when validating pure ASCII strings like hex digests or slugs.
**Action:** Use `.bytes().enumerate()` and byte literals for validation, reserving `.chars()` extraction only for the slow error path.
