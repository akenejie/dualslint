// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! UI-thread property store for properties owned by the UI thread (OS communication).
//! Separated from the render tree; keyed by control id (stable id).

use i_slint_backend_scene::ControlPropertyValue;
use i_slint_backend_scene::property_ownership::classify;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
struct UiPropertyEntry {
    value: Option<ControlPropertyValue>,
}

/// The values the UI thread holds for the properties the compiler assigned to
/// it, keyed by the control id the render thread publishes.
///
/// OS communication values -- what the platform's input method carries, what
/// the window manager changed -- live here rather than in the render-owned
/// tree, so the UI thread answers them from its own state without a round
/// trip through a thread that draws.
#[derive(Clone, Default)]
pub struct UiPropertyStore {
    by_id: Arc<Mutex<HashMap<u64, HashMap<String, UiPropertyEntry>>>>,
}

impl UiPropertyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the value of a UI-owned property of the control `id`.  A
    /// property the compiler assigned elsewhere is refused: writing it here
    /// would split the truth between two owners.
    pub fn set(
        &self,
        id: u64,
        builtin_name: Option<&str>,
        name: &str,
        value: ControlPropertyValue,
    ) {
        let owner = classify(builtin_name, name);
        if !owner.ui_holds() {
            return;
        }
        let mut map = self.by_id.lock().unwrap();
        map.entry(id).or_default().insert(name.to_string(), UiPropertyEntry { value: Some(value) });
    }

    /// The recorded value of a UI-owned property of the control `id`, or
    /// `None` when nothing was recorded or the property is not UI-owned.
    pub fn get(
        &self,
        id: u64,
        builtin_name: Option<&str>,
        name: &str,
    ) -> Option<ControlPropertyValue> {
        let owner = classify(builtin_name, name);
        if !owner.ui_holds() {
            return None;
        }
        let map = self.by_id.lock().unwrap();
        map.get(&id).and_then(|m| m.get(name)).and_then(|e| e.value.clone())
    }

    /// Whether the compiler assigned the property to the UI thread, which is
    /// what decides whether this store would accept it.
    pub fn owns(&self, builtin_name: Option<&str>, name: &str) -> bool {
        classify(builtin_name, name).ui_holds()
    }

    /// Drop every recorded value of the control `id`, for when the control is
    /// gone and its ids may be reused.
    pub fn clear(&self, id: u64) {
        let mut map = self.by_id.lock().unwrap();
        map.remove(&id);
    }
}
