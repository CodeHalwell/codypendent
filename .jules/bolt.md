## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-05-14 - String truncation optimization
**Learning:** When truncating or slicing Rust strings by character count, avoid using `.chars().count()` combined with `.take().collect()` as it causes multiple O(N) traversals and allocations.
**Action:** Instead, use `.char_indices().nth(max)` to find the exact byte index to directly slice the string (e.g., `&s[..idx]`), which operates in O(max) time.
