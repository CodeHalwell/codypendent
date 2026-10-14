## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-05-18 - Token size estimation optimization
**Learning:** Using `.chars().count()` for string length approximations (like token limits) causes an O(N) UTF-8 decoding overhead. For heuristics, `.len()` provides an O(1) byte length that is often just as good (and sometimes better, as tokenizers often process multi-byte chars as multiple tokens anyway).
**Action:** Use `.len()` instead of `.chars().count()` for string approximations and token counting heuristics where exact Unicode scalar counts are not strictly required.
