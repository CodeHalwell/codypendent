## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2025-02-18 - Avoid full string traversal for truncation
**Learning:** Checking character count string bounds and doing token operations on substrings via `.chars().count()` combined with `.take(max).collect()` performs unneeded full-length traversals and allocations.
**Action:** Replace `.chars().count()` comparisons and `.chars().take(limit).collect()` combinations with string slicing combined with `.char_indices().nth()` to find exact byte boundaries in an O(limit) pass and slice safely.
