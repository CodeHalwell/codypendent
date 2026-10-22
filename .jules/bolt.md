## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-03-22 - Bounded string operations
**Learning:** Using `s.chars().count()` is an O(N) operation and shouldn't be used just to check if a string exceeds a length limit. It creates a performance cliff for large inputs, especially when we only need a bounded operation from the tail or head (like `tail_cap`).
**Action:** Use `.char_indices().rev().nth(max - 1)` to find tail boundaries in bounded O(max) time instead of O(N) full traversals, and use `.char_indices().nth(max).is_some()` to efficiently check if length > max.
