## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - O(N) string tail retrieval
**Learning:** Retrieving the tail (last N characters) of a string via `.chars().count()` and `nth()` triggers multiple O(N) traversals of the string, which is slow for long text inputs.
**Action:** Use `.char_indices().rev().nth(max - 1)` to fetch the byte index from the end in bounded O(max) time instead.
