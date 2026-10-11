## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-05-18 - Avoid chars() UTF-8 decoding overhead for whitespace checking
**Learning:** Checking if a string consists entirely of whitespace by using `.chars().all(char::is_whitespace)` incurs unnecessary Unicode decoding overhead from `.chars()`.
**Action:** Use `.trim().is_empty()` instead, which leverages standard library optimizations and is significantly faster by avoiding `.chars()` UTF-8 overhead.
