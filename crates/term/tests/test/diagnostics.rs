use diagnostics::{Diagnostic, DiagnosticProvider, NumberOrString, Severity};
use editor_config::DiagnosticFilter;
use theme::{Color, Modifier, Theme};
use view::{current, current_ref};

use super::helpers::{keys, test_config, AppBuilder};

#[tokio::test(flavor = "multi_thread")]
async fn overlay_diagnostics_preserve_severity_styles() -> anyhow::Result<()> {
    let mut config = test_config();
    config.editor.diagnostics.end_of_line.severity = DiagnosticFilter::Disable;
    config.editor.diagnostics.inline.cursor_line = DiagnosticFilter::Disable;
    config.editor.diagnostics.inline.other_lines = DiagnosticFilter::Disable;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_input_text("#[x|]#\n")
        .build()?;
    app.editor.theme = Theme::from(toml::from_str::<toml::Value>(
        r#"
        "ui.background" = { fg = "gray", bg = "black" }
        "ui.gutter" = "gray"
        "ui.gutter.selected" = "light-gray"
        "ui.cursor" = { modifiers = ["reversed"] }
        "ui.selection" = { bg = "blue" }
        warning = { fg = "yellow", modifiers = ["bold"] }
        error = { fg = "red", modifiers = ["bold"] }
        info = { fg = "blue", modifiers = ["bold"] }
        hint = { fg = "green", modifiers = ["bold"] }
        "#,
    )?);

    for (severity, color) in [
        (Some(Severity::Error), Color::Red),
        (Some(Severity::Warning), Color::Yellow),
        (Some(Severity::Info), Color::Blue),
        (Some(Severity::Hint), Color::Green),
        (None, Color::Yellow),
    ] {
        current!(app.editor).1.replace_diagnostics(
            [Diagnostic {
                range: editor_core::Range::new(0, 1).into(),
                ends_at_word: false,
                starts_at_word: false,
                zero_width: false,
                line: 0,
                message: "diagnostic message\nsecond line".into(),
                severity,
                code: Some(NumberOrString::String("test-code".into())),
                provider: DiagnosticProvider::Spelling,
                tags: vec![],
                source: None,
                data: None,
            }],
            &[],
            None,
        );
        keys(&mut app, "<esc>").await?;
        let (view, doc) = current_ref!(app.editor);
        let area = view.inner_area(doc);
        let buffer = app.terminal_backend().buffer();
        for (row, expected) in ["diagnostic message", "second line", "(test-code)"]
            .into_iter()
            .enumerate()
        {
            let y = area.y + 1 + row as u16;
            let x = area.right() - expected.len() as u16;
            let actual: String = (x..area.right()).map(|x| buffer[(x, y)].symbol()).collect();
            assert_eq!(actual, expected);
            for x in x..area.right() {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.fg, color, "{severity:?}: {expected}");
                assert_eq!(cell.bg, Color::Black);
                assert!(cell.modifier.contains(Modifier::BOLD));
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_inside_unresolved_conflicts_are_hidden() -> anyhow::Result<()> {
    let mut config = test_config();
    config.editor.diagnostics.end_of_line.severity = DiagnosticFilter::Disable;
    config.editor.diagnostics.inline.cursor_line = DiagnosticFilter::Enable(Severity::Hint);
    config.editor.diagnostics.inline.other_lines = DiagnosticFilter::Enable(Severity::Hint);
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_input_text("#[b|]#ad\n<<<<<<< a\nbad\n=======\nbad\n>>>>>>> b\nbad\n")
        .build()?;
    app.editor.theme = Theme::from(toml::from_str::<toml::Value>(
        r#"
        "ui.background" = { fg = "gray", bg = "black" }
        "ui.cursor" = { modifiers = ["reversed"] }
        "ui.selection" = { bg = "blue" }
        error = "magenta"
        "diagnostic.error" = { fg = "red" }
        "#,
    )?);
    use stdx::rope::RopeExt as _;
    let lines = [(0, "before"), (2, "current"), (4, "incoming"), (6, "after")];
    let diagnostics = lines.map(|(line, message)| {
        let start = current_ref!(app.editor).1.text().line_to_char_idx(line);
        Diagnostic {
            range: editor_core::Range::new(start, start + 3).into(),
            ends_at_word: false,
            starts_at_word: false,
            zero_width: false,
            line,
            message: format!("{message} message").into(),
            severity: Some(Severity::Error),
            code: None,
            provider: DiagnosticProvider::Spelling,
            tags: vec![],
            source: None,
            data: None,
        }
    });
    let rendered = |app: &term::application::TestApplication| {
        use stdx::rope::RopeExt as _;
        let (view, doc) = current_ref!(app.editor);
        let (area, inner) = (view.area, view.inner_area(doc));
        let buffer = app.terminal_backend().buffer();
        let screen: String = (area.y..area.bottom())
            .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
            .map(|position| buffer[position].symbol())
            .collect();
        // Text rows whose first three cells are underlined, and rows with a gutter sign.
        let rows = |cell: &dyn Fn(u16) -> bool| -> Vec<usize> {
            (0..inner.height)
                .filter(|row| cell(inner.y + row))
                .map(|row| {
                    let pos = view
                        .pos_at_screen_coords(doc, inner.y + row, inner.x, true)
                        .unwrap();
                    doc.text().char_to_line_idx(pos)
                })
                .collect()
        };
        let underlined = rows(&|y| buffer[(inner.x + 1, y)].fg == Color::Red);
        let signs = rows(&|y| (area.x..inner.x).any(|x| buffer[(x, y)].fg == Color::Magenta));
        let messages = ["before", "current", "incoming", "after"]
            .map(|message| screen.contains(&format!("{message} message")));
        (messages, underlined, signs)
    };
    current!(app.editor)
        .1
        .replace_diagnostics(diagnostics, &[], None);
    keys(&mut app, "<esc>").await?;
    assert_eq!(
        rendered(&app),
        ([true, false, false, true], vec![0, 6], vec![0, 6])
    );
    // Without its opening marker this is no longer a conflict.
    keys(&mut app, "jxd").await?;
    assert_eq!(
        rendered(&app),
        ([true; 4], vec![0, 1, 3, 5], vec![0, 1, 3, 5])
    );
    Ok(())
}
