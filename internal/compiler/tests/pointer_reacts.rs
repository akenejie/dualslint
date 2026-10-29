// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! Whether a pointer event on a control has to reach the drawing thread is
//! decided from the elements, before either thread sees them. These tests pin
//! that decision: an event on a control the application does not react to
//! changes nothing on screen, so it need not be sent on at all.

#![cfg(feature = "rust")]

use i_slint_compiler::diagnostics::BuildDiagnostics;
use i_slint_compiler::generator::{self, OutputFormat};
use i_slint_compiler::parser::parse;
use i_slint_compiler::{CompilerConfiguration, compile_syntax_node};

fn compile(source: &str) -> String {
    let mut diagnostics = BuildDiagnostics::default();
    let syntax_node = parse(source.into(), None, &mut diagnostics);
    let config = CompilerConfiguration::new(OutputFormat::Rust);
    let (doc, diagnostics, loader) =
        spin_on::spin_on(compile_syntax_node(syntax_node, diagnostics, config));
    assert!(!diagnostics.has_errors(), "{}", diagnostics.to_string_vec().join("\n"));
    let mut output = Vec::new();
    generator::generate(OutputFormat::Rust, &mut output, None, &doc, &loader.compiler_config)
        .unwrap();
    String::from_utf8(output).unwrap()
}

fn window(body: &str) -> String {
    format!("export component Test inherits Window {{ {body} }}")
}

/// The decision is per element and carried into the item tree, so the UI thread
/// can tell from a control id alone whether a pointer event on the control has
/// to reach the drawing thread. This reads the bit back out of the generated
/// nodes.
fn pointer_reacts_bits(source: &str) -> Vec<bool> {
    let rust = compile(source);
    let mut bits = Vec::new();
    let mut rest = rust.as_str();
    while let Some(pos) = rest.find("pointer_reacts") {
        rest = &rest[pos + "pointer_reacts".len()..];
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
fn a_control_nothing_reacts_to_carries_no_answer() {
    let bits = pointer_reacts_bits(&window("Rectangle { }"));
    assert!(!bits.is_empty(), "the window has a node to carry the bit: {bits:?}");
    assert!(bits.iter().all(|b| !*b), "nothing reacts to a pointer here: {bits:?}");
}

#[test]
fn a_bound_click_handler_means_the_pointer_reaches_the_application() {
    let bits = pointer_reacts_bits(&window("TouchArea { clicked => { } }"));
    assert!(bits.contains(&true), "the handler answers a press: {bits:?}");
}

#[test]
fn a_control_nobody_answers_does_not_reach_the_application() {
    let bits = pointer_reacts_bits(&window("TouchArea { }"));
    assert!(bits.iter().all(|b| !*b), "no handler, no pointer event: {bits:?}");
}

#[test]
fn reading_where_the_pointer_is_means_the_pointer_reaches_the_application() {
    // The pressed state is read by another element, so the drawing thread has
    // to be told about it.
    let bits = pointer_reacts_bits(&window(
        "ta := TouchArea { } Rectangle { background: ta.pressed ? red : blue; }",
    ));
    assert!(bits.contains(&true), "the pressed state is read: {bits:?}");
}

#[test]
fn reading_where_the_pointer_is_leaves_the_other_control_alone() {
    // Only the control carrying the pointer state reacts; the rectangle that
    // draws it has no pointer of its own.
    let bits = pointer_reacts_bits(&window(
        "ta := TouchArea { } Rectangle { background: ta.pressed ? red : blue; }",
    ));
    assert!(bits.contains(&false), "the rectangle reacts to nothing: {bits:?}");
}

#[test]
fn a_text_input_answers_the_pointer_by_itself() {
    // The caret follows the pointer, which shows, so the event has to reach
    // the tree that draws it even when the application says nothing.
    let bits = pointer_reacts_bits(&window("TextInput { }"));
    assert!(bits.contains(&true), "the caret follows the pointer: {bits:?}");
}

#[test]
fn a_control_reaches_the_item_inside_it() {
    // A press on the outer control reaches the inner one, so the outer control
    // carries the answer of the inner one too.
    let bits = pointer_reacts_bits(&window("TouchArea { Rectangle { } TextInput { } }"));
    assert!(bits.contains(&true), "the inner input answers the pointer: {bits:?}");
}

#[test]
fn a_control_that_only_holds_a_rectangle_answers_nothing() {
    let bits = pointer_reacts_bits(&window("TouchArea { Rectangle { } }"));
    assert!(bits.iter().all(|b| !*b), "nothing inside answers the pointer: {bits:?}");
}

#[test]
fn a_cursor_the_application_sets_says_nothing_about_the_pointer() {
    // The application tells the cursor, it does not read where the pointer is,
    // so a press changes nothing on screen.
    let bits = pointer_reacts_bits(&window("TouchArea { mouse-cursor: pointer; }"));
    assert!(bits.iter().all(|b| !*b), "the cursor is set, not read: {bits:?}");
}
