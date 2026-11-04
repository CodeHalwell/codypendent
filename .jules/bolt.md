## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - Fast bounded string operations without full O(N) traversals
**Learning:** Using `.chars().count()` and `.chars().take(max).collect()` performs full O(N) traversals and allocations just to truncate a string or check its length.
**Action:** Use `s.char_indices().nth(max)` to find the exact byte index to slice strings in bounded time (`O(max)`). For the tail, use `s.char_indices().rev().nth(max - 1)` (which is `O(max)` from the end).
