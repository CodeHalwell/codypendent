## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-11-20 - String ASCII Validation Optimization
**Learning:** Using `.chars()` to validate strings that are expected to be pure ASCII incurs unnecessary UTF-8 decoding overhead for every character.
**Action:** Use `.as_bytes().iter().enumerate().find(...)` (or similar byte-level iterators) for ASCII validation to bypass the decoding step. The index of the first failure is guaranteed to be a valid char boundary, allowing safe error extraction via `s[i..].chars().next().unwrap()`.
