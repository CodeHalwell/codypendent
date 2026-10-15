## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2023-10-27 - O(1) text length estimation
**Learning:** Using `.chars().count()` for string length estimation incurs O(N) UTF-8 decoding overhead which is unnecessary for simple heuristics.
**Action:** Replace `.chars().count()` with `.len()` when estimating string lengths (like token size approximations) to achieve O(1) performance and avoid UTF-8 processing.
