## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-04-20 - Fast ASCII validation
**Learning:** Using `.chars()` in Rust for string validation introduces overhead from decoding UTF-8 on every character, even if we are only looking for ASCII characters.
**Action:** When validating that a string contains only ASCII characters (e.g. `is_ascii_alphanumeric` or `matches!(c, b'-' | b'_' | b'.')`), use `.bytes()` instead of `.chars()` to skip UTF-8 processing, resulting in significant performance improvements.
