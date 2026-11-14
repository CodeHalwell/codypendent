## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-05-24 - String Truncation Bounded Time Optimization
**Learning:** Using `.chars().count() <= max` to check string limits forces an O(N) traversal of the full string, which is slow for large strings. Similarly, `.take(max).collect()` performs multiple O(N) re-allocations from individual chars.
**Action:** Use `.char_indices().nth(max)` to find the exact string byte boundary in O(max) bounded time, allowing direct byte slicing via `&s[..idx]`.
