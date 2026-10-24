// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! UI-thread property store for properties owned by the UI thread (OS communication).
//! Separated from the render tree; keyed by control id (stable id).

use i_slint_backend_scene::property_ownership::{classify, PropertyOwnership};
use i_slint_backend_scene::ControlPropertyValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
struct UiPropertyEntry {
    value: Option<ControlPropertyValue>,
}

#[derive(Clone, Default)]
pub struct UiPropertyStore {
    by_id: Arc<Mutex<HashMap<u64, HashMap<String, UiPropertyEntry>>>>,
}

impl UiPropertyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, id: u64, builtin_name: Option<&str>, name: &str, value: ControlPropertyValue) {
        let owner = classify(builtin_name, name);
        if !owner.ui_holds() {
            return;
        }
        let mut map = self.by_id.lock().unwrap();
        map.entry(id).or_default().insert(name.to_string(), UiPropertyEntry { value: Some(value) });
    }

    pub fn get(&self, id: u64, builtin_name: Option<&str>, name: &str) -> Option<ControlPropertyValue> {
        let owner = classify(builtin_name, name);
        if !owner.ui_holds() {
            return None;
        }
        let map = self.by_id.lock().unwrap();
        map.get(&id).and_then(|m| m.get(name)).and_then(|e| e.value.clone())
    }

    pub fn owns(&self, builtin_name: Option<&str>, name: &str) -> bool {
        classify(builtin_name, name).ui_holds()
    }

    pub fn clear(&self, id: u64) {
        let mut map = self.by_id.lock().unwrap();
        map.remove(&id);
    }
}
