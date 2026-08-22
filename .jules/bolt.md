## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2024-05-18 - String validation optimization in Control Plane protocol
**Learning:** Validation rules that check for purely ASCII properties (like `is_ascii_hexdigit`, `is_ascii_lowercase`, and `is_ascii_digit`) on string iterators incur unnecessary UTF-8 decoding overhead when `.chars()` is used.
**Action:** Use `.as_bytes().iter()` instead of `.chars()` when iterating over string characters that are guaranteed or expected to be validated purely using ASCII predicates.
