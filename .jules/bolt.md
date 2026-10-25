## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - O(N) penalty in string character truncation
**Learning:** Using `.chars().count() <= max` to validate length before truncating strings via `.chars().take(max).collect()` introduces a massive O(N) performance bottleneck for large strings, iterating the entire sequence twice and creating unnecessary intermediate string allocations.
**Action:** When capping a string by character length, use `.char_indices().nth(max)` to find the exact byte boundary in O(max) time. This avoids full string traversal, limits allocation bounds accurately, and safely slices on valid multi-byte character boundaries.
