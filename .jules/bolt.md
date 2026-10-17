## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-31 - Avoiding `.chars().count()` in truncation functions
**Learning:** Using `.chars().count()` followed by `.chars().take().collect()` processes strings multiple times and allocates unnecessarily. This creates O(N) operations with a high hidden cost.
**Action:** When truncating or slicing by character count, prefer a single pass using `.char_indices().nth(max)`. This extracts the exact byte index to directly slice the string avoiding `.chars().count()` and `.collect::<String>()` allocations entirely. For finding the tail of a string, `s.char_indices().rev().nth(max - 1)` provides the exact slice boundary in bounded time without scanning the entire prefix.
