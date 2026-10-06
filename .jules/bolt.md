## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-10-27 - Rust pure-ASCII string validation

**Learning:** When validating purely ASCII strings (e.g. alphanumeric strings, slugs, hex strings), using `.chars()` incurs unnecessary UTF-8 decoding overhead. The `codypendent-control-plane-protocol` package had several such validations. Benchmarks showed replacing it with `.bytes()` provides a ~2.5x speedup for valid strings. The error path can still accurately extract the offending character using `s[i..].chars().next().unwrap()` because the index `i` is mathematically guaranteed to fall on a valid character boundary after valid ASCII.

**Action:** Whenever validating purely ASCII constraints on `&str`, iterate using `.bytes()` or `.bytes().enumerate()` instead of `.chars()` to skip UTF-8 parsing in the happy path.
