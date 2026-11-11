## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-11-23 - ASCII string validation optimization
**Learning:** Using `.chars()` for pure ASCII validation loops incurs unnecessary UTF-8 decoding overhead.
**Action:** Iterate over byte indices using `.bytes().enumerate()`, which is significantly faster. If an invalid character is found, it can safely be extracted using `s[i..].chars().next().unwrap()` because the index of the first non-ASCII byte is guaranteed to be a valid character boundary.
