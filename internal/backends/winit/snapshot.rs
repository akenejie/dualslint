// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// The snapshot encoder now lives in the shared `i-slint-backend-scene` crate,
// so other 2-thread backends can reuse the same protocol.  This module is a
// thin re-export so existing `crate::snapshot::…` call paths keep working.

pub use i_slint_backend_scene::snapshot::{encode_window_scene, encode_window_scene_full};
