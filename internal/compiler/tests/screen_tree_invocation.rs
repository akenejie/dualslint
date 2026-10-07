// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Software-3.0

//! A window-rooted component's exported callback and function calls are carried to the tree the
//! window shows, on the thread that draws it, so that they reach the bindings and callbacks that
//! decide what is on screen. Only values that can travel to another thread and come back qualify,
//! and only a window has a tree of its own to carry them to, so these tests pin the C++ code that
//! the generator emits for both kinds of call.

#![cfg(feature = "cpp")]

use i_slint_compiler::diagnostics::BuildDiagnostics;
use i_slint_compiler::generator::{self, OutputFormat};
use i_slint_compiler::parser::parse;
use i_slint_compiler::{CompilerConfiguration, compile_syntax_node};

/// Compile `source` to C++ and return the generated code.
fn generate_cpp(source: &str) -> String {
    let mut diagnostics = BuildDiagnostics::default();
    let syntax_node = parse(source.into(), None, &mut diagnostics);
    let config = CompilerConfiguration::new(OutputFormat::Cpp(Default::default()));
    let (doc, diagnostics, loader) =
        spin_on::spin_on(compile_syntax_node(syntax_node, diagnostics, config));
    assert!(!diagnostics.has_errors(), "{:?}", diagnostics.to_string_vec());
    let mut output = Vec::new();
    generator::generate(
        OutputFormat::Cpp(Default::default()),
        &mut output,
        None,
        &doc,
        &loader.compiler_config,
    )
    .unwrap();
    String::from_utf8(output).unwrap()
}

/// The bridge a generated invoker uses to reach the tree on screen.
const BRIDGE: &str = "slint_windowrc_run_on_screen_tree";

/// A window-rooted component carries its calls to the tree it shows.
#[test]
fn window_rooted_calls_reach_the_screen_tree() {
    let cpp = generate_cpp(
        r#"
export component App inherits Window {
    callback add(int) -> int;
    public function bump(x: int) -> int { return x; }
}
"#,
    );
    assert!(cpp.contains(BRIDGE), "both calls should be carried:\n{cpp}");
}

/// A call that carries a model is a reference into the tree the caller holds, so it stays there.
#[test]
fn calls_that_carry_a_model_stay_on_their_own_tree() {
    let cpp = generate_cpp(
        r#"
export component App inherits Window {
    callback rows([int]);
}
"#,
    );
    assert!(!cpp.contains(BRIDGE), "a model cannot cross to another thread:\n{cpp}");
}

/// A tray icon is not a window, so it has no tree of its own to ask.
#[test]
fn a_tray_icon_has_no_screen_tree_to_reach() {
    let cpp = generate_cpp(
        r#"
export component Tray inherits SystemTrayIcon {
    callback activated();
}
"#,
    );
    assert!(!cpp.contains(BRIDGE), "a tray icon has no window to ask:\n{cpp}");
}

/// A global has no screen tree of its own; its calls run against the tree the caller holds.
#[test]
fn a_global_has_no_screen_tree_to_reach() {
    let cpp = generate_cpp(
        r#"
export global Stats {
    callback changed(int);
}
export component App inherits Window {
    in property<int> unused;
}
"#,
    );
    // The window root itself contributes no portable call here, so the only bridge that could
    // appear would be the global's, which must not.
    assert!(!cpp.contains(BRIDGE), "a global has no window to ask:\n{cpp}");
}
