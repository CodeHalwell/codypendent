## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-11-20 - String truncation with char_indices
**Learning:** Checking string limits using `.chars().count() > N` forces a full O(N) traversal of the string even if the string is huge. Slicing with `.chars().take(N).collect::<String>()` incurs another partial traversal and unnecessary string allocations.
**Action:** Use `.char_indices().nth(N)` to find the exact byte boundary in bounded time (O(N)), avoiding full traversal for strings larger than N, and slice directly (e.g., `&s[..idx]`) to avoid intermediate allocations.
