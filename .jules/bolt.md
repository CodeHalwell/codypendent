## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-05-24 - ASCII Validation Error Reporting
**Learning:** Extracting an offending character from a byte index using `.chars().next().unwrap()` provides error reporting without impacting the happy path performance of `.bytes()` loops.
**Action:** Use `.bytes().enumerate()` and extract the error character with `s[i..].chars().next().unwrap()`.
