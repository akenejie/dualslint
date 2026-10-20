// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Software-3.0

//! Whether an input-method value belongs to the UI thread or the drawing thread
//! is decided from the elements, before either thread sees them. These tests pin
//! that decision: a value the platform's input method is told and nothing draws
//! is the UI thread's, so its presence is recorded for the code generators.

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

fn ime_affects_rendering(source: &str) -> bool {
    compile(source)
        .0
        .ime_affects_rendering
        .get()
        .expect("the ime_render_dependency pass recorded an answer")
}

fn window(body: &str) -> String {
    format!("export component Test inherits Window {{ {body} }}")
}

#[test]
fn a_window_without_an_input_method_value_has_none_to_own() {
    assert!(!ime_affects_rendering(&window("Rectangle { }")));
}

#[test]
fn a_text_input_without_a_hint_draws_from_what_it_also_draws() {
    assert!(!ime_affects_rendering(&window("TextInput { }")));
}

#[test]
fn an_input_method_hint_is_a_value_for_the_ui_thread_to_own() {
    assert!(ime_affects_rendering(&window(
        "TextInput { input-method-hints: { auto-correct: false }; }"
    )));
}

#[test]
fn the_generated_code_carries_the_decision() {
    let (_, rust) = compile(&window("TextInput { input-method-hints: { auto-correct: false }; }"));
    assert!(
        rust.contains("set_ime_affects_rendering (true)"),
        "the UI thread has to be told that a value it owns is present:\n{rust}"
    );
}
