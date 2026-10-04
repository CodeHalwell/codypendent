## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-10-04 - Safely handling pure ASCII validations via bytes
**Learning:** You can safely skip `.chars()` for ASCII validations using `.bytes().enumerate()`, because any non-ASCII UTF-8 byte sequence starts with a byte having its high bit set. This guarantees the validation loop fails on the *first* byte of the multi-byte sequence, meaning `i` will always be exactly on a valid UTF-8 character boundary.
**Action:** When migrating from `s.chars()` to `s.bytes().enumerate()` for pure ASCII validations, construct error messages using `s[i..].chars().next().unwrap()` to retrieve the exact invalid character without introducing panics or performance hits on the happy path.
