## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2025-02-13 - Replace chars().all(char::is_whitespace) with trim().is_empty()
**Learning:** Checking for whitespace by instantiating an iterator with `.chars()` and calling `.all()` is unnecessarily slow, as it decodes characters one by one. The standard library's `trim()` handles this check more efficiently.
**Action:** Use `.trim().is_empty()` to test if a string slice is entirely whitespace, instead of `.chars().all(char::is_whitespace)`.
