## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-31 - String truncation optimization
**Learning:** Using `.chars().count() > MAX` and `.chars().take(MAX).collect::<String>()` does an `O(N)` traversal followed by an `O(MAX)` allocating traversal, which is extremely slow for long strings.
**Action:** Use `.char_indices().nth(MAX)` to find the precise byte boundary and slice the string in a single `O(MAX)` pass without heap-allocating intermediate characters. This reduces runtime from `O(N)` to `O(MAX)` and eliminates allocations.
