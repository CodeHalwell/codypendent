use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn cap_statement_old(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}\u{2026}")
}

fn cap_statement_new(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some(_) => {
            let cut_idx = s.char_indices().nth(max.saturating_sub(1)).map_or(0, |(idx, _)| idx);
            let mut truncated = String::with_capacity(cut_idx + 3);
            truncated.push_str(&s[..cut_idx]);
            truncated.push('\u{2026}');
            truncated
        }
        None => s.to_string(),
    }
}

fn criterion_benchmark(c: &mut Criterion) {
    let s = "Hello, world! This is a relatively long string that will be truncated. ".repeat(10);
    c.bench_function("cap_statement_old", |b| b.iter(|| cap_statement_old(black_box(&s), black_box(50))));
    c.bench_function("cap_statement_new", |b| b.iter(|| cap_statement_new(black_box(&s), black_box(50))));
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
