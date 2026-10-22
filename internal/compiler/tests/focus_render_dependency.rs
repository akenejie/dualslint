// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Software-3.0

//! Whether moving focus on screen has to redraw is decided from the elements,
//! before the drawing thread ever sees them. These tests pin that decision: a
//! focus-carrying element whose focus state nothing reads, and that does not
//! paint it itself, must leave the screen alone.

#![cfg(feature = "rust")]

use i_slint_compiler::diagnostics::BuildDiagnostics;
use i_slint_compiler::generator::{self, OutputFormat};
use i_slint_compiler::parser::parse;
use i_slint_compiler::{CompilerConfiguration, compile_syntax_node};

fn compile(source: &str) -> (i_slint_compiler::object_tree::Document, String) {
    let mut diagnostics = BuildDiagnostics::default();
    let syntax_node = parse(source.into(), None, &mut diagnostics);
    let config = CompilerConfiguration::new(OutputFormat::Rust);
    let (doc, diagnostics, loader) =
        spin_on::spin_on(compile_syntax_node(syntax_node, diagnostics, config));
    assert!(!diagnostics.has_errors(), "{}", diagnostics.to_string_vec().join("\n"));
    let mut output = Vec::new();
    generator::generate(OutputFormat::Rust, &mut output, None, &doc, &loader.compiler_config)
        .unwrap();
    (doc, String::from_utf8(output).unwrap())
}

fn focus_affects_rendering(source: &str) -> bool {
    compile(source)
        .0
        .focus_affects_rendering
        .get()
        .expect("the focus_render_dependency pass recorded an answer")
}

fn window(body: &str) -> String {
    format!("export component Test inherits Window {{ {body} }}")
}

#[test]
fn a_window_without_focus_has_nothing_to_redraw_for() {
    assert!(!focus_affects_rendering(&window("Rectangle { }")));
}

#[test]
fn a_focus_scope_that_nothing_reads_draws_the_same_when_focused() {
    assert!(!focus_affects_rendering(&window("FocusScope { }")));
}

#[test]
fn reading_has_focus_makes_the_focus_visible() {
    assert!(focus_affects_rendering(&window(
        "fs := FocusScope { } Rectangle { background: fs.has-focus ? red : blue; }"
    )));
}

#[test]
fn a_bound_focus_handler_makes_the_focus_visible() {
    assert!(focus_affects_rendering(&window("FocusScope { focus-gained => { } }")));
}

#[test]
fn a_text_input_paints_its_focus_itself() {
    assert!(focus_affects_rendering(&window("TextInput { }")));
}

#[test]
fn the_generated_code_carries_the_decision() {
    let (_, rust) = compile(&window("FocusScope { }"));
    assert!(
        rust.contains("set_focus_affects_rendering (false)"),
        "the drawing thread has to be told that focus draws nothing:\n{rust}"
    );
}

/// The decision is per element and carried into the item tree, so the UI thread
/// can tell from a control id alone whether a focus change has to reach the
/// drawing thread. This reads the bit back out of the generated nodes.
fn focus_draws_bits(source: &str) -> Vec<bool> {
    let rust = compile(source).1;
    let mut bits = Vec::new();
    let mut rest = rust.as_str();
    while let Some(pos) = rest.find("focus_draws") {
        rest = &rest[pos + "focus_draws".len()..];
        let value = rest.trim_start().trim_start_matches(':').trim_start();
        if value.starts_with("true") {
            bits.push(true);
        } else if value.starts_with("false") {
            bits.push(false);
        }
    }
    bits
}

#[test]
fn each_control_carries_its_own_answer() {
    // A scope nothing reads draws the same when focused; a `TextInput` paints
    // its focus itself. Both are in one window, so the answers cannot come from
    // a single document-wide bit.
    let bits = focus_draws_bits(&window(
        "a := FocusScope { } ti := TextInput { } Rectangle { background: a.has-focus ? red : blue; }",
    ));
    assert!(bits.contains(&false), "the unread scope draws nothing: {bits:?}");
    assert!(bits.contains(&true), "the text input paints its focus: {bits:?}");
}

#[test]
fn a_scope_the_application_reads_draws_on_focus() {
    let bits = focus_draws_bits(&window(
        "a := FocusScope { } Rectangle { background: a.has-focus ? red : blue; }",
    ));
    assert!(bits.contains(&true), "reading `has-focus` makes the scope draw: {bits:?}");
}

#[test]
fn a_scope_nothing_reads_still_carries_the_false_bit() {
    // The window root and the scope are both there, and neither draws for focus.
    let bits = focus_draws_bits(&window("FocusScope { }"));
    assert_eq!(bits, vec![false, false], "nothing in this window draws for focus");
}
