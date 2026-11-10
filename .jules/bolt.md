## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-05-15 - String truncation char traversal optimization
**Learning:** Using `.chars().count()` combined with `.take().collect()` for string truncation by character count is inefficient as it causes multiple O(N) traversals and allocations.
**Action:** Use `.char_indices().nth(max)` to find the exact byte index to directly slice the string (e.g., `&s[..idx]`), and for finding the tail, use `s.char_indices().rev().nth(max.saturating_sub(1))`. Ensure explicit handling of the `max == 0` edge case.
