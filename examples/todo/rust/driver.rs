// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

// Temporary driver: clicks the "Sort by name" checkbox the way the user does,
// then reports the list rows the render thread published, so a duplicated-row
// bug shows up in the control table without a screen.

use slint::Timer;
use std::rc::Rc;

slint::include_modules!();

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
    use slint::{FilterModel, Model, SortModel};
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
    main_window.on_popup_confirmed(|| {});
    main_window.on_apply_sorting_and_filtering({
        let weak = main_window.as_weak();
        let todo_model = todo_model.clone();
        move || {
            let window = weak.unwrap();
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

    main_window.set_todo_model(todo_model.clone().into());
    main_window.set_show_header(true);
    main_window.show().unwrap();

    let timer = Timer::default();
    let weak = main_window.as_weak();
    let count = std::cell::Cell::new(0u32);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(60),
        move || {
            let Some(window) = weak.upgrade() else { return };
            let n = count.get();
            count.set(n + 1);
            let hosts = slint::render_thread::hosts();
            let mut clicked = false;
            for host in &hosts {
                // Even cycles: click the sort checkbox. Odd cycles: click a
                // list row, the way checking an item off does.
                // Interleave everything the user does: sort checkbox, list
                // checkbox, and the confirm popup.
                let list_row = n % 3 != 2;
                if n % 3 == 2 {
                    window.invoke_show_confirm_popup();
                }
                let mut candidates: Vec<_> = host
                    .press_targets()
                    .iter()
                    .filter_map(|t| host.control_coord(t.id).map(|c| (t.id, c)))
                    .filter(|(_, c)| {
                        if list_row {
                            c.y > 140.0 && c.y < 500.0
                        } else {
                            c.y > 20.0 && c.y < 140.0 && c.x < 200.0 && c.width < 200.0
                        }
                    })
                    .collect();
                candidates.sort_by(|a, b| {
                    a.1.x.partial_cmp(&b.1.x).unwrap_or(std::cmp::Ordering::Equal)
                });
                if let Some((_, c)) = candidates.first() {
                    let x = c.x + c.width / 2.0;
                    let y = c.y + c.height / 2.0;
                    host.press_control(
                        slint::private_unstable_api::re_exports::PointerEventButton::Left,
                        slint::private_unstable_api::re_exports::LogicalPoint::new(x, y),
                        None,
                    );
                    host.release_control(
                        slint::private_unstable_api::re_exports::PointerEventButton::Left,
                        slint::private_unstable_api::re_exports::LogicalPoint::new(x, y),
                    );
                    clicked = true;
                }
                // Report the rows: controls below the header, one per list row.
                let rows: Vec<(f32, f32)> = host
                    .press_targets()
                    .iter()
                    .filter_map(|t| host.control_coord(t.id))
                    .filter(|c| c.y > 140.0)
                    .map(|c| (c.y, c.x))
                    .collect();
                // No blocking reads here: the application does not poll, and a
                // synchronous read would serialize exactly the race we are
                // trying to observe.
                eprintln!("cycle {n} clicked={clicked} list-controls={}", rows.len());
            }
            if n >= 100 {
                eprintln!("done");
                slint::quit_event_loop().ok();
            }
        },
    );

    main_window.run().unwrap();
}
