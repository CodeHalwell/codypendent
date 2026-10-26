use codypendent_tui::{render_splash, Theme};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn draw(
    size: (u16, u16),
    tick: u64,
    ready: bool,
    needs_setup: bool,
    stage: &str,
    warnings: &[String],
    theme: &Theme,
) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    terminal
        .draw(|frame| render_splash(frame, tick, stage, warnings, ready, needs_setup, theme))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            (buffer.area.left()..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn ready_status_and_enter_survive_short_screens_and_warning_pressure() {
    for width in [30, 50, 64, 65, 69, 70, 80, 100] {
        for height in 2..=32 {
            for count in [0, 1, 4, 5, 16] {
                let warnings: Vec<_> = (0..count).map(|i| format!("Startup warning {i}")).collect();
                let output = text(&draw(
                    (width, height),
                    0,
                    true,
                    false,
                    "Ready",
                    &warnings,
                    &Theme::dark(),
                ));
                for required in ["Ready", "ENTER", "open workspace"] {
                    assert!(
                        output.contains(required),
                        "{required} missing at {width}x{height} with {count} warnings:\n{output}"
                    );
                }
            }
        }
    }
}

#[test]
fn warnings_report_the_exact_number_hidden_by_the_limit_or_available_height() {
    for (size, count, shown) in [((100, 30), 5, 4), ((100, 30), 6, 4), ((50, 7), 6, 3)] {
        let warnings: Vec<_> = (1..=count)
            .map(|i| format!("Startup warning {i}"))
            .collect();
        let output = text(&draw(
            size,
            0,
            true,
            false,
            "Ready",
            &warnings,
            &Theme::dark(),
        ));
        for (index, warning) in warnings.iter().enumerate() {
            assert_eq!(output.contains(warning), index < shown, "{output}");
        }
        assert!(
            output.contains(&format!("+{} more", count - shown)),
            "{output}"
        );
        assert!(output.contains("ENTER"), "{output}");
    }
}

#[test]
fn readiness_is_static_in_every_theme_including_first_run_setup() {
    for theme in [
        Theme::dark(),
        Theme::light(),
        Theme::ansi256(),
        Theme::ansi16(),
        Theme::monochrome(),
    ] {
        for needs_setup in [false, true] {
            let first = draw((100, 30), 0, true, needs_setup, "Ready", &[], &theme);
            let later = draw((100, 30), u64::MAX, true, needs_setup, "Ready", &[], &theme);
            assert_eq!(first, later, "readiness must not animate");
            assert!(text(&first).contains(if needs_setup {
                "connect a model"
            } else {
                "open workspace"
            }));
        }
    }
}

#[test]
fn tiny_and_empty_terminals_accept_long_unicode_diagnostics() {
    let stage = "Preparing 界 👩‍💻 e\u{301} ".repeat(600);
    let warnings = vec![stage.clone(); 8];
    for width in [0, 1, 2, 8, 20] {
        for height in [0, 1, 2, 3, 6] {
            for ready in [false, true] {
                draw(
                    (width, height),
                    u64::MAX,
                    ready,
                    true,
                    &stage,
                    &warnings,
                    &Theme::dark(),
                );
            }
        }
    }
}

#[test]
fn diagnostic_controls_and_newlines_cannot_displace_the_ready_gate() {
    let output = text(&draw(
        (100, 18),
        0,
        true,
        false,
        "\x1b[31mReady\x1b[0m\nfor work",
        &["\x1b]0;Injected title\x07Model\r\nwarning\u{202e}".to_owned()],
        &Theme::dark(),
    ));
    assert!(output.contains("Ready for work"), "{output}");
    assert!(output.contains("Model warning"), "{output}");
    assert!(!output.contains("Injected title"), "{output}");
    assert!(
        !output.contains(['\x1b', '\x07', '\r', '\u{202e}']),
        "{output}"
    );
    assert!(output.contains("ENTER"), "{output}");
}
