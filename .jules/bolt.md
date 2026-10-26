## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - Large string truncation optimization
**Learning:** Using `.chars().count()` on a potentially large string just to check if it exceeds a limit, followed by `.take(limit).collect()`, is extremely inefficient as it requires a full O(N) string traversal and allocates a new string.
**Action:** Use `.char_indices().nth(limit)` to find the exact byte index in O(limit) time and use `.truncate(idx)` to truncate the string in place, saving an allocation and reducing traversal bounds.
