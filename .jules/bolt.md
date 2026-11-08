## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-11-08 - O(N) allocation string truncation optimization
**Learning:** Using `.chars().count()` combined with `.chars().take(max).collect::<String>()` performs multiple O(N) traversals and allocates a new String for truncation.
**Action:** Use `.char_indices().nth(max)` to find the exact byte boundary to directly slice the string (e.g., `&s[..idx]`), avoiding the extra traversal and allocating only when creating the final output.
