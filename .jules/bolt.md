## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - Fast string truncation without allocations
**Learning:** Using `.chars().count()` combined with `.take().collect()` for string truncation causes multiple O(N) traversals and unnecessary allocations.
**Action:** Use `.char_indices().nth(max)` to find the exact byte index and slice the string directly (`&s[..idx]`), which avoids allocations and redundant traversals.
