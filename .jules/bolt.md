## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2023-10-27 - Fast character bounding
**Learning:** Using `.chars().count()` followed by `.chars().take(max).collect()` results in O(N) traversals and unnecessary allocations when bounding string character counts.
**Action:** Use `s.char_indices().nth(max)` to find byte indices of the truncation boundary directly in O(max) time.
