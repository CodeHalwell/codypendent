## 2023-10-27 - Table detection parsing optimization
**Learning:** Checking if an ASCII character (like `-`) appears in a string by using `.chars()` incurs Unicode decoding overhead.
**Action:** Use `.as_bytes().iter().all(|&c| c == b'-')` instead of `.chars().all(|c| c == '-')` for pure ASCII checks to skip UTF-8 processing, resulting in significant performance improvements.
## 2024-05-19 - Fast token context estimation
**Learning:** Using `.chars().count()` for estimating token sizes is O(N) due to UTF-8 decoding. Since LLM tokenizers often split non-ASCII characters anyway, using byte length via `.len()` is an O(1) operation that is safer for context bounds estimation and significantly faster on large tool outputs.
**Action:** Use `.len()` instead of `.chars().count()` for length approximations in token sizing or fast context bounds checking.
