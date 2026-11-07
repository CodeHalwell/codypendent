## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - Fast string truncation without allocations
**Learning:** Truncating or slicing strings by character count using `.chars().count()` combined with `.take().collect()` causes multiple O(N) traversals and unnecessary allocations.
**Action:** Use `.char_indices().nth(max)` to find the exact byte index and directly slice the string (e.g., `&s[..idx]`), saving allocations and keeping the time complexity bounded by `max` instead of the full string length.
