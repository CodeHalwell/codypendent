## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-11-21 - O(1) String Truncation
**Learning:** Using `.chars().count()` to check string length or bounds incurs a full O(N) traversal, and `.chars().take(limit).collect::<String>()` allocates a new string.
**Action:** Use `.char_indices().nth(limit)` to find exact byte boundaries in O(limit) time. This allows safe, zero-allocation slicing (e.g. `&s[..idx]`), which avoids traversing the tail of massive strings.
