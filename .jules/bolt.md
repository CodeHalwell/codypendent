## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-11-20 - [Avoid UTF-8 decode overhead in ASCII validation]
**Learning:** Using `.chars()` on valid UTF-8 strings adds decode overhead. For validations enforcing strictly ASCII constraints, `.bytes()` avoids this overhead while remaining safe, since the first invalid byte index is always a valid character boundary.
**Action:** Always prefer `.bytes()` (or `.bytes().enumerate()`) for pure ASCII string validations instead of `.chars()`.
