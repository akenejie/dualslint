// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! The drawn tree's editable copy of an application model.

use crate::model::{Model, ModelNotify, ModelRc, ModelTracker};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

/// The rows the drawn tree shows: an editable copy of the model the
/// application holds.
///
/// The drawn tree is a separate component whose model property cannot hold
/// the application's `Rc`-model -- a model is not `Send` -- so a copy of the
/// rows travels there instead. A copy replaced wholesale on every carry would
/// make the drawn tree's repeater drop and re-create every row, which both
/// allocates and takes the copy away from the user: a checkbox the user ticks
/// on screen writes through the copy, and a wholesale replacement discards
/// that write. So the copy is a model of its own -- the same one across
/// carries (recognized through [`Model::as_any`], see
/// [`screen_tree_model_carry`]), updated in place with per-row notifications
/// -- and the rows the user edits are handed back to the application's model
/// through `write_back`.
pub struct ScreenTreeModel<T> {
    rows: RefCell<Vec<T>>,
    notify: ModelNotify,
    write_back: Box<dyn Fn(usize, T)>,
}

impl<T: Clone + PartialEq + 'static> ScreenTreeModel<T> {
    fn with_rows(rows: Vec<T>, write_back: impl Fn(usize, T) + 'static) -> Self {
        Self {
            rows: RefCell::new(rows),
            notify: ModelNotify::default(),
            write_back: Box::new(write_back),
        }
    }

    /// Replace the rows, notifying only what changed.
    ///
    /// Per-row notifications keep the drawn tree's repeater reusing its row
    /// instances: a full reset would tell it to drop them all, and the next
    /// draw would re-create every row from scratch -- the allocation the
    /// carry is trying to avoid.
    fn set_rows(&self, rows: Vec<T>) {
        let mut current = self.rows.borrow_mut();
        let common = current.len().min(rows.len());
        let changed: Vec<usize> = (0..common).filter(|&i| current[i] != rows[i]).collect();
        let added = rows.len().checked_sub(current.len());
        let removed = current.len().checked_sub(rows.len());
        *current = rows;
        drop(current);
        for i in &changed {
            self.notify.row_changed(*i);
        }
        if let Some(count) = added.filter(|count| *count > 0) {
            self.notify.row_added(common, count);
        }
        if let Some(count) = removed.filter(|count| *count > 0) {
            self.notify.row_removed(common, count);
        }
    }
}

impl<T: Clone + PartialEq + 'static> Model for ScreenTreeModel<T> {
    type Data = T;

    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        self.rows.borrow().get(row).cloned()
    }

    fn set_row_data(&self, row: usize, data: Self::Data) {
        {
            let mut current = self.rows.borrow_mut();
            let Some(slot) = current.get_mut(row) else { return };
            if *slot == data {
                return;
            }
            *slot = data.clone();
        }
        self.notify.row_changed(row);
        (self.write_back)(row, data);
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// The model the drawn tree's `current` property holds, carrying `rows`.
///
/// Called where a carry would have built a fresh snapshot model: the first
/// carry creates one and installs it in the property, and every carry after
/// that updates the same model in place -- which the drawn tree's repeater
/// sees as per-row changes rather than a wholesale replacement, so its row
/// instances are reused instead of dropped and re-created. `write_back`
/// hands the rows the user edits on screen to the application's model.
pub fn screen_tree_model_carry<Element: Clone + PartialEq + 'static>(
    current: &ModelRc<Element>,
    rows: Vec<Element>,
    write_back: impl Fn(usize, Element) + 'static,
) -> ModelRc<Element> {
    if let Some(existing) = current.as_any().downcast_ref::<ScreenTreeModel<Element>>() {
        existing.set_rows(rows);
        // The same model back: the repeater compares by pointer and keeps
        // its row instances.
        current.clone()
    } else {
        ModelRc::from(Rc::new(ScreenTreeModel::with_rows(rows, write_back)))
    }
}
