// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

// Diagnostic driver for the dualslint render-thread split.
//
// Clicks the todo UI the way the user does -- the sort checkbox, list rows,
// and the confirm popup -- while dumping, every cycle, the row titles the
// UI-side model holds and the texts the render thread's mirror actually
// draws. A duplicate row or a garbled popup label shows up as a mismatch
// between the two dumps, without needing a screen.

use slint::private_unstable_api::re_exports as sp;
use slint::{FilterModel, Model, SortModel, Timer};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

slint::include_modules!();

fn click_at(window: &slint::Weak<MainWindow>, x: f32, y: f32) {
    if let Some(w) = window.upgrade() {
        slint::render_thread::click(w.window(), x, y);
    }
}

/// Read a `Property<SharedString>` held inside a pinned item.
fn read_text(property: &sp::Property<sp::SharedString>) -> String {
    // SAFETY: the property lives inside an item that is structurally pinned
    // (`#[pin]` on the item struct), so it does not move for as long as the
    // item is alive, and the item is alive because the tree holds it.
    unsafe { std::pin::Pin::new_unchecked(property) }.get().to_string()
}

/// Walk one item tree and collect `(y, text)` of every text item, positioned
/// in window coordinates so the visual row order is comparable to the order
/// the UI-side model holds.
fn dump_texts(tree: &sp::ItemTreeRc, tag: &str) {
    let mut texts: Vec<(f32, String)> = Vec::new();
    sp::visit_items(
        tree,
        sp::TraversalOrder::FrontToBack,
        |component, _item, index, _state| {
            let item_rc = sp::ItemRc::new(component.clone(), index);
            let text = if let Some(t) = item_rc.downcast::<sp::SimpleText>() {
                Some(read_text(&t.as_pin_ref().get_ref().text))
            } else if let Some(t) = item_rc.downcast::<sp::ComplexText>() {
                Some(read_text(&t.as_pin_ref().get_ref().text))
            } else {
                None
            };
            if let Some(value) = text {
                let origin = item_rc.geometry().origin;
                let y = item_rc.map_to_native_window(origin).y;
                texts.push((y, value));
            }
            sp::ItemVisitorResult::Continue(())
        },
        (),
    );
    texts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    eprintln!("[mirror] {tag}: {texts:?}");
}

/// Dump the texts the render thread's trees hold: the window's own tree and
/// every popup it shows.
fn probe_mirror(tree: &sp::ItemTreeRc) {
    dump_texts(tree, "tree");
    let root = sp::ItemRc::new(tree.clone(), 0);
    if let Some(adapter) = root.window_adapter() {
        let inner = sp::WindowInner::from_pub(adapter.window());
        for (i, popup) in inner.active_popups().iter().enumerate() {
            dump_texts(&popup.component.clone(), &format!("popup{i}"));
        }
    }
}

fn main() {
    let todo_model = Rc::new(slint::VecModel::<TodoItem>::from(vec![
        TodoItem { checked: true, title: "Implement the .slint file".into() },
        TodoItem { checked: false, title: "Do the Rust part".into() },
        TodoItem { checked: false, title: "Make the C++ code".into() },
        TodoItem { checked: false, title: "Write some JavaScript code".into() },
        TodoItem { checked: false, title: "Test the application".into() },
        TodoItem { checked: false, title: "Ship to customer".into() },
        TodoItem { checked: false, title: "???".into() },
        TodoItem { checked: false, title: "Profit".into() },
    ]));

    let main_window = MainWindow::new().unwrap();

    // Wire the same handlers the real application wires, so the callback
    // routing and the model carries run exactly as they do there.
    main_window.on_todo_added({
        let weak = main_window.as_weak();
        let todo_model = todo_model.clone();
        move |text| {
            todo_model.push(TodoItem { checked: false, title: text });
            weak.unwrap().invoke_apply_sorting_and_filtering();
        }
    });
    main_window.on_remove_done({
        let weak = main_window.as_weak();
        let todo_model = todo_model.clone();
        move || {
            let mut offset = 0;
            let mut removed = 0;
            for i in 0..todo_model.row_count() {
                if todo_model.row_data(i - offset).unwrap().checked {
                    todo_model.remove(i - offset);
                    offset += 1;
                    removed += 1;
                }
            }
            if removed > 0 {
                weak.unwrap().invoke_apply_sorting_and_filtering();
            }
        }
    });
    // The real application hides the window when Yes is pressed, with the
    // popup still open -- the teardown that path exercises is part of the
    // test.
    main_window.on_popup_confirmed({
        let weak = main_window.as_weak();
        move || {
            eprintln!("[driver] popup_confirmed -> hide window");
            weak.unwrap().hide().unwrap();
        }
    });
    main_window.on_apply_sorting_and_filtering({
        let weak = main_window.as_weak();
        let todo_model = todo_model.clone();
        move || {
            let window = weak.unwrap();
            eprintln!("[handler] apply_sorting_and_filtering runs");
            window.set_todo_model(todo_model.clone().into());
            if window.get_hide_done_items() {
                window.set_todo_model(
                    Rc::new(FilterModel::new(window.get_todo_model(), |e| !e.checked)).into(),
                );
            }
            if window.get_is_sort_by_name() {
                window.set_todo_model(
                    Rc::new(SortModel::new(window.get_todo_model(), |lhs, rhs| {
                        lhs.title.to_lowercase().cmp(&rhs.title.to_lowercase())
                    }))
                    .into(),
                );
            }
        }
    });

    // In the constants-verification mode the .slint defaults stay untouched:
    // nothing is set on the component, so what the mirror draws before any
    // carry is exactly what the compiled tree hardcodes.
    let constants_mode = std::env::var_os("SLINT_CONSTANTS").is_some();
    if !constants_mode {
        main_window.set_todo_model(todo_model.clone().into());
        main_window.set_show_header(true);
    }
    main_window.show().unwrap();

    let timer = Timer::default();
    let weak = main_window.as_weak();
    let count = Cell::new(0u32);
    let phase_cell = Rc::new(Cell::new(0u8));
    // Control ids seen before the popup opened: a press target that is new is
    // the popup's own button.
    let seen_ids = Rc::new(RefCell::new(HashSet::<u64>::new()));
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(12), move || {
        let Some(window) = weak.upgrade() else { return };
        let n = count.get();
        count.set(n + 1);

        // Report what the UI-side model holds.
        let model = window.get_todo_model();
        let titles: Vec<String> = (0..model.row_count())
            .filter_map(|i| model.row_data(i).map(|t| t.title.to_string()))
            .collect();
        eprintln!("[ui] t={n} model({}): {titles:?}", titles.len());

        for host in slint::render_thread::hosts() {
            // Ask the render thread what it actually draws. Posted, not
            // blocking: a blocking read would serialize the race being
            // observed.
            if std::env::var_os("SLINT_NO_PROBE").is_none() {
                if std::env::var_os("SLINT_LOG_REPEATER").is_some() {
                    eprintln!("[driver] probe post");
                }
                host.post_on_screen_tree(Box::new(probe_mirror));
            }

            let targets: Vec<(u64, f32, f32, f32, f32)> = host
                .press_targets()
                .iter()
                .filter_map(|t| {
                    host.control_coord(t.id).map(|c| (t.id, c.x, c.y, c.width, c.height))
                })
                .collect();
            // A press target that was not on screen last cycle is new -- that
            // is how the popup's own buttons are told apart. Computed against
            // the previous cycle's set, then folded in, so the answer is the
            // same whichever mode runs.
            let alive: HashSet<u64> = targets.iter().map(|(id, ..)| *id).collect();
            let fresh: Vec<(u64, f32, f32, f32, f32)> = {
                let seen = seen_ids.borrow();
                targets.iter().filter(|(id, ..)| !seen.contains(id)).copied().collect()
            };
            seen_ids.borrow_mut().retain(|id| alive.contains(id));
            seen_ids.borrow_mut().extend(alive);

            if std::env::var_os("SLINT_WATCH").is_some() {
                // Watch mode: the driver only reports; the clicks come from a
                // real pointer through the window system, so the whole input
                // path (winit -> ui thread -> control protocol) is exercised.
                if n >= 1500 {
                    eprintln!("done");
                    slint::quit_event_loop().ok();
                    return;
                }
                continue;
            }
            if std::env::var_os("SLINT_ONE_CLICK").is_some() {
                // One-click verification, in phases, of what the mirror draws:
                //
                // 1. with nothing set on the component, the compiled tree's own
                //    content is on screen (the hardcoded part),
                // 2. one property carry (show-header) turns the header on,
                // 3. one click on the sort checkbox reorders the list through
                //    the routed callback and the model carries,
                // 4. the popup renders its own hardcoded labels.
                if n == 40 && constants_mode {
                    eprintln!("[driver] t={n} carry show-header=true");
                    window.set_show_header(true);
                }
                if n >= 45 && n % 6 == 0 && phase_cell.get() == 0 {
                    let mut header: Vec<_> =
                        targets.iter().filter(|(_, _, y, _, _)| *y < 140.0).collect();
                    header.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                    if let Some((id, x, y, w, h)) = header.first() {
                        let (x, y) = (x + w / 2.0, y + h / 2.0);
                        eprintln!("[driver] t={n} ONE sort click id={id} at ({x},{y})");
                        click_at(&weak, x, y);
                        phase_cell.set(1);
                    }
                }
                if n >= 70 && n % 6 == 3 && phase_cell.get() == 1 {
                    // Tick the first list row's checkbox: the write the drawn
                    // tree must hand back to the application's model.
                    let mut rows: Vec<_> =
                        targets.iter().filter(|(_, _, y, _, _)| *y > 70.0 && *y < 500.0).collect();
                    rows.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
                    if let Some((id, x, y, w, h)) = rows.first() {
                        let (x, y) = (x + w / 2.0, y + h / 2.0);
                        eprintln!("[driver] t={n} row tick id={id} at ({x},{y})");
                        click_at(&weak, x, y);
                        phase_cell.set(2);
                    }
                }
                if n >= 90 && n % 6 == 0 && phase_cell.get() == 2 {
                    // "Hide done items": the second checkbox in the header band.
                    // The header band is the strip above the list: the list's
                    // own checkboxes start lower, and an over-wide band would
                    // tick a row's checkbox instead.
                    if std::env::var_os("SLINT_LOG_REPEATER").is_some() {
                        for (id, x, y, w, h) in targets.iter().filter(|(_, _, y, ..)| *y < 110.0) {
                            eprintln!("[driver] t={n} target id={id} at ({x},{y}) {w}x{h}");
                        }
                    }
                    let mut header: Vec<_> =
                        targets.iter().filter(|(_, _, y, _, _)| *y > 40.0 && *y < 70.0).collect();
                    header.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                    if let Some((id, x, y, w, h)) = header.get(1) {
                        let (x, y) = (x + w / 2.0, y + h / 2.0);
                        eprintln!("[driver] t={n} hide-done click id={id} at ({x},{y})");
                        click_at(&weak, x, y);
                        phase_cell.set(3);
                    }
                }
                if n >= 120 && n % 30 == 0 && phase_cell.get() == 3 {
                    eprintln!("[driver] t={n} open popup");
                    window.invoke_show_confirm_popup();
                    phase_cell.set(2);
                } else if (1..12).contains(&(n % 30)) && phase_cell.get() == 2 {
                    let mut popup_buttons: Vec<_> = fresh.clone();
                    popup_buttons.sort_by(|a, b| (a.1, a.2).partial_cmp(&(b.1, b.2)).unwrap());
                    for (id, x, y, w, h) in popup_buttons.iter().take(1) {
                        let (x, y) = (x + w / 2.0, y + h / 2.0);
                        eprintln!("[driver] t={n} popup click id={id} at ({x},{y})");
                        click_at(&weak, x, y);
                    }
                } else if n % 30 == 15 && phase_cell.get() == 2 {
                    eprintln!("[driver] t={n} click outside popup");
                    click_at(&weak, 10.0, 30.0);
                }
                if n >= 240 {
                    eprintln!("done");
                    slint::quit_event_loop().ok();
                    return;
                }
                continue;
            }
            let phase = n % 300;
            if phase == 120 {
                eprintln!("[driver] t={n} open popup");
                window.invoke_show_confirm_popup();
            } else if (140..200).contains(&phase) {
                // Click the popup's own buttons: press targets that were
                // not on screen before the popup opened.
                let mut popup_buttons: Vec<_> = fresh.clone();
                popup_buttons.sort_by(|a, b| (a.1, a.2).partial_cmp(&(b.1, b.2)).unwrap());
                for (id, x, y, w, h) in popup_buttons.iter().take(1) {
                    let (x, y) = (x + w / 2.0, y + h / 2.0);
                    eprintln!("[driver] t={n} popup click id={id} at ({x},{y})");
                    click_at(&weak, x, y);
                }
            } else if phase == 210 {
                eprintln!("[driver] t={n} click outside popup");
                click_at(&weak, 10.0, 30.0);
            } else if n % 6 == 0 {
                // Click the "Sort by name" checkbox: the leftmost press
                // target in the header band.
                let mut header: Vec<_> =
                    targets.iter().filter(|(_, _, y, _, _)| *y < 140.0).collect();
                header.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                if let Some((_, x, y, w, h)) = header.first() {
                    let (x, y) = (x + w / 2.0, y + h / 2.0);
                    click_at(&weak, x, y);
                }
            } else if n % 6 == 3 {
                // Click a list row, rotating through them.
                let mut rows: Vec<_> =
                    targets.iter().filter(|(_, _, y, _, _)| *y > 140.0 && *y < 500.0).collect();
                rows.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
                if !rows.is_empty() {
                    let (_, x, y, w, h) = rows[n as usize % rows.len()];
                    let (x, y) = (x + w / 2.0, y + h / 2.0);
                    click_at(&weak, x, y);
                }
            }
        }
        // The watch mode quits on its own schedule; the other modes end here.
        if n >= 240 && std::env::var_os("SLINT_WATCH").is_none() {
            eprintln!("done");
            slint::quit_event_loop().ok();
            // Everything below talks to the render thread through calls that
            // block on its reply, and the loop that answers them is on its way
            // out: interacting past this point would stall the exit on a
            // reply that never comes.
            return;
        }
    });

    main_window.run().unwrap();
}
