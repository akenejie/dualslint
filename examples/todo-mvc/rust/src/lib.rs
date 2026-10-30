// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

use slint::ComponentHandle;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

pub mod mvc;
pub mod ui;

mod callback;
pub use callback::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(start))]
pub fn main() {
    let main_window = init();

    // Verification driver (env-gated with SLINT_WATCH): dumps the texts the
    // render thread's mirror draws every cycle, and clicks a row's checkbox
    // and then its remove button, so the routed global callback and the
    // model carry the re-apply triggers are all exercised. Without the env
    // variable the example runs exactly as upstream does.
    if std::env::var_os("SLINT_WATCH").is_some() {
        use slint::private_unstable_api::re_exports as sp;
        let clicked = std::cell::Cell::new(false);
        let removed = std::cell::Cell::new(false);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(700), move || {
            for host in slint::render_thread::hosts() {
                if !clicked.get() {
                    // Tick the first list row's checkbox, then its remove
                    // button: the callback goes through the drawn tree's
                    // copy of the global, which the handover pointed back
                    // at the application's handler, and the re-apply the
                    // handler runs carries the change back here.
                    let targets: Vec<_> = host
                        .press_targets()
                        .iter()
                        .filter_map(|t| {
                            host.control_coord(t.id).map(|c| (t.id, c.x, c.y, c.width, c.height))
                        })
                        .filter(|(_, _, y, _, _)| *y > 40.0 && *y < 300.0)
                        .collect();
                    let first = targets.iter().min_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
                    if let Some((_, x, y, w, h)) = first {
                        let (x, y) = (x + w / 2.0, y + h / 2.0);
                        eprintln!("[driver] tick checkbox at ({x},{y})");
                        host.press_control(
                            sp::PointerEventButton::Left,
                            sp::LogicalPoint::new(x, y),
                            None,
                        );
                        host.release_control(
                            sp::PointerEventButton::Left,
                            sp::LogicalPoint::new(x, y),
                        );
                        clicked.set(true);
                    }
                } else if clicked.get() && !removed.get() {
                    // The rightmost press target in the first row's band:
                    // the row's remove button.
                    let targets: Vec<_> = host
                        .press_targets()
                        .iter()
                        .filter_map(|t| {
                            host.control_coord(t.id).map(|c| (t.id, c.x, c.y, c.width, c.height))
                        })
                        .filter(|(_, x, y, _, _)| *y > 40.0 && *y < 90.0 && *x > 300.0)
                        .collect();
                    let rightmost = targets.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                    if let Some((_, x, y, w, h)) = rightmost {
                        let (x, y) = (x + w / 2.0, y + h / 2.0);
                        eprintln!("[driver] click remove at ({x},{y})");
                        host.press_control(
                            sp::PointerEventButton::Left,
                            sp::LogicalPoint::new(x, y),
                            None,
                        );
                        host.release_control(
                            sp::PointerEventButton::Left,
                            sp::LogicalPoint::new(x, y),
                        );
                        removed.set(true);
                    }
                }
                host.post_on_screen_tree(Box::new(|tree| {
                    let mut texts = Vec::new();
                    sp::visit_items(
                        tree,
                        sp::TraversalOrder::FrontToBack,
                        |component, _item, index, _state| {
                            let item_rc = sp::ItemRc::new(component.clone(), index);
                            let text = if let Some(t) = item_rc.downcast::<sp::SimpleText>() {
                                Some(
                                    unsafe {
                                        std::pin::Pin::new_unchecked(&t.as_pin_ref().get_ref().text)
                                    }
                                    .get()
                                    .to_string(),
                                )
                            } else if let Some(t) = item_rc.downcast::<sp::ComplexText>() {
                                Some(
                                    unsafe {
                                        std::pin::Pin::new_unchecked(&t.as_pin_ref().get_ref().text)
                                    }
                                    .get()
                                    .to_string(),
                                )
                            } else {
                                None
                            };
                            if let Some(value) = text
                                && !value.is_empty()
                            {
                                texts.push((item_rc.geometry().origin.y, value));
                            }
                            sp::ItemVisitorResult::Continue(())
                        },
                        (),
                    );
                    texts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                    eprintln!("[mirror] texts: {texts:?}");
                }));
            }
        });
    }

    main_window.run().unwrap();
}

fn init() -> ui::MainWindow {
    let view_handle = ui::MainWindow::new().unwrap();

    let task_list_controller = mvc::TaskListController::new(mvc::task_repo());
    ui::task_list_adapter::connect(&view_handle, task_list_controller.clone());
    ui::navigation_adapter::connect_task_list_controller(
        &view_handle,
        task_list_controller.clone(),
    );

    let create_task_controller = mvc::CreateTaskController::new(mvc::date_time_repo());
    ui::create_task_adapter::connect(&view_handle, create_task_controller.clone());
    ui::navigation_adapter::connect_create_task_controller(&view_handle, create_task_controller);
    ui::create_task_adapter::connect_task_list_controller(&view_handle, task_list_controller);

    view_handle
}

// FIXME: android example
