// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! The drawn tree's editable copy of an application model.

use crate::model::{Model, ModelChangeListenerContainer, ModelNotify, ModelRc, ModelTracker};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

/// The application's model as the property the generated setter stores: a
/// wrapper that hears the source's row changes.
///
/// A model is a shared reference the thread boundary refuses to move, so the
/// drawn tree holds a copy of the rows ([`ScreenTreeModel`]) and follows the
/// application's rows through the carries. The carries that a handler's write
/// triggers cover the writes the `.slint` code makes; the rows an application
/// changes on its own -- a push to the `VecModel` it holds, say -- change
/// nothing the carry machinery sees. This wrapper is what closes that gap:
/// the generated setter stores it in place of the plain model, its listener
/// rides the source's own change notifications, and every change is read back
/// as rows and handed to `forward`, which posts them to the drawn copy. The
/// wrapper's own tracker fires the same way, so the application tree's
/// repeaters -- bound to the property this sits in -- update as upstream
/// would.
///
/// Everything else delegates: reads, writes and the `Any` downcast reach the
/// source, so application code that holds the model it set keeps working
/// unchanged. The wrapper dies when the property is assigned again, which
/// takes its listener with it.
struct CarriedModelSource<T> {
    source: ModelRc<T>,
    forward: Box<dyn Fn(Vec<T>)>,
    notify: ModelNotify,
}

impl<T: Clone + PartialEq + 'static> crate::model::ModelChangeListener for CarriedModelSource<T> {
    fn row_changed(self: core::pin::Pin<&Self>, _row: usize) {
        self.changed()
    }
    fn row_added(self: core::pin::Pin<&Self>, _index: usize, _count: usize) {
        self.changed()
    }
    fn row_removed(self: core::pin::Pin<&Self>, _index: usize, _count: usize) {
        self.changed()
    }
    fn reset(self: core::pin::Pin<&Self>) {
        self.changed()
    }
}

impl<T: Clone + PartialEq + 'static> CarriedModelSource<T> {
    /// Read the source's rows back and hand them to the forward channel, then
    /// tell this wrapper's own listeners: the application tree's repeaters sit
    /// on this tracker, and upstream they would have heard the source
    /// directly.
    fn changed(self: core::pin::Pin<&Self>) {
        let this = self.get_ref();
        let rows: Vec<T> = this.source.iter().collect();
        (this.forward)(rows.clone());
        this.notify.reset();
    }
}

struct CarriedModel<T: Clone + PartialEq + 'static> {
    inner: core::pin::Pin<Box<ModelChangeListenerContainer<CarriedModelSource<T>>>>,
}

impl<T: Clone + PartialEq + 'static> Model for CarriedModel<T> {
    type Data = T;

    fn row_count(&self) -> usize {
        self.inner.source.row_count()
    }

    fn row_data(&self, row: usize) -> Option<T> {
        self.inner.source.row_data(row)
    }

    fn set_row_data(&self, row: usize, data: T) {
        self.inner.source.set_row_data(row, data)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.inner.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self.inner.source.as_any()
    }
}

/// Wrap the model the application assigned so its row changes carry to the
/// drawn tree, and keep the application tree's own repeaters hearing them.
///
/// `forward` receives the source's rows after every change; the generated
/// setter builds it to post the rows to the drawn copy. The returned model is
/// what the property stores.
pub fn carried_model<T: Clone + PartialEq + 'static>(
    source: ModelRc<T>,
    forward: impl Fn(Vec<T>) + 'static,
) -> ModelRc<T> {
    let inner = Box::pin(ModelChangeListenerContainer::new(CarriedModelSource {
        source: source.clone(),
        forward: Box::new(forward),
        notify: ModelNotify::default(),
    }));
    // The listener rides the source's own change notifications: the application
    // mutates the model it holds, and this wrapper hears it.
    source.model_tracker().attach_peer(inner.as_ref().model_peer());
    ModelRc::new(Rc::new(CarriedModel { inner }))
}

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

/// The rows the drawn tree shows when a row is a model of its own: a copy of
/// the application's two-dimensional model, one [`ScreenTreeModel`] per row.
///
/// A two-dimensional model cannot travel the way a flat one does -- a row
/// would carry a reference into the tree the application holds -- so the
/// carry ([`screen_tree_model_nested_carry`]) takes the rows' plain values
/// and rebuilds the grid on the drawn side: every row becomes a pooled model
/// of its own, updated in place on the next carry, and a cell the user edits
/// is handed back through `write_back` with the row and column it happened
/// in.
pub struct NestedScreenTreeModel<T> {
    rows: RefCell<Vec<Row<T>>>,
    notify: ModelNotify,
    write_back: SharedWriteBack<T>,
}

/// The write-back of the grid, shared with every row so a row's closure can
/// hand an edit on without keeping the grid alive.
type SharedWriteBack<T> = Rc<dyn Fn(usize, usize, T)>;

/// One row of the grid: the pooled copy of the application's row model, and
/// the slot that says which row of the application's grid this copy
/// currently is. Rows shift when the grid loses one, so the slot is read at
/// write-back time rather than captured.
struct Row<T> {
    model: Rc<ScreenTreeModel<T>>,
    slot: Rc<Cell<usize>>,
}

impl<T: Clone + PartialEq + 'static> NestedScreenTreeModel<T> {
    fn build_row(&self, index: usize, cells: Vec<T>) -> Row<T> {
        let slot = Rc::new(Cell::new(index));
        let write_back = self.write_back.clone();
        let model = {
            let slot = slot.clone();
            Rc::new(ScreenTreeModel::with_rows(cells, move |col, data| {
                write_back(slot.get(), col, data);
            }))
        };
        Row { model, slot }
    }

    fn with_rows(rows: Vec<Vec<T>>, write_back: impl Fn(usize, usize, T) + 'static) -> Self {
        let model = Self {
            rows: RefCell::new(Vec::new()),
            notify: ModelNotify::default(),
            write_back: Rc::new(write_back),
        };
        *model.rows.borrow_mut() = rows
            .into_iter()
            .enumerate()
            .map(|(index, cells)| model.build_row(index, cells))
            .collect();
        model
    }

    /// Replace the rows, notifying only what changed.
    ///
    /// Rows that stay are updated in place -- the inner model notifies its
    /// own cells -- and rows that appear or disappear notify the outer model,
    /// so the drawn tree's repeater re-creates only the rows that came or
    /// went. Every surviving row's slot is renumbered, so a write-back from
    /// it lands on the row the application's grid holds now.
    fn set_rows(&self, rows: Vec<Vec<T>>) {
        let mut current = self.rows.borrow_mut();
        let common = current.len().min(rows.len());
        let added = rows.len().checked_sub(current.len());
        let removed = current.len().checked_sub(rows.len());
        for (index, cells) in rows.iter().enumerate().take(common) {
            current[index].model.set_rows(cells.clone());
            current[index].slot.set(index);
        }
        current.truncate(common);
        drop(current);
        if let Some(count) = added.filter(|count| *count > 0) {
            let start = common;
            let built: Vec<Row<T>> = (start..start + count)
                .map(|index| self.build_row(index, rows[index].clone()))
                .collect();
            self.rows.borrow_mut().extend(built);
            self.notify.row_added(start, count);
        }
        if let Some(count) = removed.filter(|count| *count > 0) {
            self.notify.row_removed(common, count);
        }
    }
}

impl<T: Clone + PartialEq + 'static> Model for NestedScreenTreeModel<T> {
    type Data = ModelRc<T>;

    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        self.rows.borrow().get(row).map(|row| ModelRc::from(row.model.clone()))
    }

    fn set_row_data(&self, row: usize, data: Self::Data) {
        // The framework writes through a row's own model, never through the
        // grid; a whole-row assignment copies the given model's cells over
        // the pooled copy.
        let Some(row) = self.rows.borrow().get(row).map(|row| row.model.clone()) else { return };
        for index in 0..row.row_count() {
            if let Some(cell) = data.row_data(index) {
                row.set_row_data(index, cell);
            }
        }
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// The model the drawn tree's two-dimensional property holds, carrying `rows`.
///
/// The two-dimensional counterpart of [`screen_tree_model_carry`]: the first
/// carry builds the grid of pooled row models and installs it in the
/// property, and every carry after that updates the same rows in place --
/// which the drawn tree's repeaters see as per-cell and per-row changes, so
/// their instances are reused instead of dropped and re-created.
/// `write_back` hands the cells the user edits on screen -- with the row and
/// column they happened in -- to the application's model.
pub fn screen_tree_model_nested_carry<Element: Clone + PartialEq + 'static>(
    current: &ModelRc<ModelRc<Element>>,
    rows: Vec<Vec<Element>>,
    write_back: impl Fn(usize, usize, Element) + 'static,
) -> ModelRc<ModelRc<Element>> {
    if let Some(existing) = current.as_any().downcast_ref::<NestedScreenTreeModel<Element>>() {
        existing.set_rows(rows);
        // The same model back: the repeater compares by pointer and keeps
        // its row instances.
        current.clone()
    } else {
        ModelRc::from(Rc::new(NestedScreenTreeModel::with_rows(rows, write_back)))
    }
}
