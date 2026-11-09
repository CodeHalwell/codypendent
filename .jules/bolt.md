## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.

## 2023-11-20 - Simplify boolean logic in desktop tools schema validation
**Learning:** Checking if a nested value is absent inside an `Option` can lead to complicated boolean logic using `!is_some_and(is_some)`.
**Action:** Use `is_none_or(...)` to simplify and improve the readability of boolean conditions evaluating optional values, satisfying `clippy::nonminimal_bool` constraints while avoiding double negatives.

## 2023-11-20 - Avoid `.chars().count()` combined with `.take().collect()` in Rust
**Learning:** Using `.chars().count()` for string truncation bounds limits causes an O(N) traversal. It is also error-prone when mixed with `.take().collect()` to truncate a multi-byte string because it triggers multiple allocations and iterations.
**Action:** Use `.char_indices().nth(max)` to find the exact byte index to directly slice the string (e.g., `&s[..idx]`). For finding the tail of a string, use `s.char_indices().rev().nth(max.saturating_sub(1))` explicitly handling the `max == 0` edge case.
