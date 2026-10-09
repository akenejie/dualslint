// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Name-based bridge between the public API (`get_property`, `invoke`,
//! `set_callback`, …) and the LLR's index-based `MemberReference`s.
//!
//! Each `PublicComponent::public_properties` entry carries a
//! `MemberReference`; dispatch forwards to the evaluator helpers in
//! [`crate::eval`].

use crate::Value;
use crate::api::SetPropertyError;
use crate::eval::{EvalContext, invoke_callback, invoke_function, load_property, store_property};
use crate::instance::{Instance, SubComponentInstance};
use i_slint_compiler::langtype::Type;
use i_slint_compiler::llr::{
    CompilationUnit, LocalMemberIndex, LocalMemberReference, MemberReference, PublicComponent,
    PublicProperty, SubComponentPublicProperty,
};
use i_slint_compiler::object_tree::PropertyVisibility;
use i_slint_core::item_tree::{ItemRc, ItemTreeRc, ItemTreeVTable};
use i_slint_core::model::Model;
use i_slint_core::window::WindowInner;
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use vtable::{VRc, VRef};

/// Look up a public property by name on the given public component.
/// Normalizes `name` through `normalize_identifier` so
/// snake_case and kebab-case both work.
pub fn find_public_property<'a>(
    public: &'a PublicComponent,
    name: &str,
) -> Option<&'a PublicProperty> {
    let normalized = i_slint_compiler::parser::normalize_identifier(name);
    public.public_properties.get(normalized.as_str())
}

/// Read the value of a public property on `instance`.
pub fn get(instance: &VRc<ItemTreeVTable, Instance>, name: &str) -> Option<Value> {
    // What the application reads has to be the value the tree the window draws
    // holds, because that tree's bindings decide what is seen, and a handler
    // that ran there changed the property where it ran. A value that cannot
    // travel is read where the caller is, exactly as a call that cannot travel
    // runs there.
    //
    // "Cannot travel" is this fork's one accepted divergence, and it is a
    // physical one, not a choice: a model, an image, a brush or a struct
    // holding one of those is a shared reference that `Send` refuses to move
    // across a thread. Such a property is owned by whichever tree the caller
    // runs against, and each tree keeps its own copy; the drawn tree's copy is
    // whatever its own bindings made it. Everything that can travel is routed,
    // writes included, so for it the two trees are the same tree.
    let crosses = {
        let (public, _) = resolve(instance)?;
        let prop = find_public_property(public, name)?;
        if !prop.ty.is_property_type() {
            return None;
        }
        i_slint_compiler::llr::is_thread_portable_type(&prop.ty)
    };
    if crosses && let Some(value) = get_from_screen_tree(instance, name) {
        return Some(value);
    }
    get_local(instance, name)
}

/// Read a public property from the instance itself, with no thought for a tree
/// another thread may be drawing.
fn get_local(instance: &Instance, name: &str) -> Option<Value> {
    let (public, sub) = resolve_root(instance)?;
    let prop = find_public_property(public, name)?;
    if !prop.ty.is_property_type() {
        return None;
    }
    let ctx = EvalContext::new(sub);
    Some(load_property(&ctx, &prop.prop))
}

/// Read a public property from the tree the window shows.
///
/// A backend that draws a tree of its own answers from there; one that draws the
/// tree the instance holds answers from that one, which is the same tree. When
/// there is no window to ask, `None` says so and the caller reads the instance
/// it holds.
fn get_from_screen_tree(instance: &Instance, name: &str) -> Option<Value> {
    let adapter = instance.window_adapter_or_default()?;
    let local_tree = WindowInner::from_pub(adapter.window()).try_component()?;
    // The answer comes back through a pointer rather than a channel, because
    // `Value` is not `Send` -- a model is a shared reference. The task the
    // backend runs is what fills the slot, and the backend runs it before
    // `run_on_screen_tree` returns, so the slot is complete before it is read.
    #[derive(Copy, Clone)]
    struct Answer(*const RefCell<Option<Value>>);
    // SAFETY: the slot lives on this stack frame, and the task that writes
    // through the pointer runs before `run_on_screen_tree` returns.
    unsafe impl Send for Answer {}
    impl Answer {
        fn fill(&self, value: Option<Value>) {
            // SAFETY: the pointee outlives this call, which returns before it.
            unsafe { *(*self.0).borrow_mut() = value }
        }
        fn take(&self) -> Option<Value> {
            // SAFETY: the pointee is this frame's slot, which the task above
            // already wrote, and nothing writes it again before this reads it.
            unsafe { (*self.0).borrow().clone() }
        }
    }
    let slot = RefCell::new(None);
    let answer = Answer(&slot as *const _);
    let name = name.to_string();
    let run = move |tree: &ItemTreeRc| {
        let screen = VRef::downcast_pin::<Instance>(VRc::borrow_pin(tree))
            .expect("the tree on screen is not this interpreter component");
        answer.fill(get_local(screen.get_ref(), &name));
    };
    adapter.run_on_screen_tree(local_tree, Box::new(run));
    answer.take()
}

/// Write a public property on `instance`.
pub fn set(
    instance: &VRc<ItemTreeVTable, Instance>,
    name: &str,
    value: Value,
) -> Result<(), SetPropertyError> {
    set_on(instance, name, value)
}

/// Write a public property, addressed by the instance itself.
pub fn set_on(instance: &Instance, name: &str, mut value: Value) -> Result<(), SetPropertyError> {
    let (public, sub) = resolve_root(instance).ok_or(SetPropertyError::NoSuchProperty)?;
    let prop = find_public_property(public, name).ok_or(SetPropertyError::NoSuchProperty)?;
    if !prop.ty.is_property_type() {
        return Err(SetPropertyError::NoSuchProperty);
    }
    if prop.read_only() {
        return Err(SetPropertyError::AccessDenied);
    }
    if !check_and_coerce(&mut value, &prop.ty) {
        return Err(SetPropertyError::WrongType);
    }
    let portable = i_slint_compiler::llr::is_thread_portable_type(&prop.ty);
    let ctx = EvalContext::new(sub);
    store_property(&ctx, &prop.prop, value.clone());
    // The value is now on the tree the instance holds, but a backend may draw a
    // tree of its own, and that is the one whose bindings decide what is seen.
    // Carry the value there too, for a property whose type can travel; one that
    // cannot (a model, an image, a brush -- a shared reference `Send` refuses to
    // cross a thread) is the one known gap, and it is owned by whichever tree
    // the caller ran against, each side keeping its own copy.
    if portable {
        forward_public_property(instance, name, value);
    }
    Ok(())
}

/// Write a public property on `instance` itself, without carrying the value to
/// the tree a backend may draw.
///
/// The handover and the carry below both land on the tree that is drawn, so the
/// write has to stop here: forwarding it again would send the value back to the
/// tree it just reached.
fn set_local(instance: &Instance, name: &str, mut value: Value) -> Result<(), SetPropertyError> {
    let (public, sub) = resolve_root(instance).ok_or(SetPropertyError::NoSuchProperty)?;
    let prop = find_public_property(public, name).ok_or(SetPropertyError::NoSuchProperty)?;
    if !prop.ty.is_property_type() {
        return Err(SetPropertyError::NoSuchProperty);
    }
    if prop.read_only() {
        return Err(SetPropertyError::AccessDenied);
    }
    if !check_and_coerce(&mut value, &prop.ty) {
        return Err(SetPropertyError::WrongType);
    }
    let ctx = EvalContext::new(sub);
    store_property(&ctx, &prop.prop, value);
    Ok(())
}

/// Carry `value` of the public property `name` from the instance the application
/// holds to the tree the window shows, when a backend draws one of its own.
///
/// A backend that draws the tree the instance holds runs the task against that
/// same tree, where [`set_local`] assigns the value that is already set.
fn forward_public_property(instance: &Instance, name: &str, value: Value) {
    let Some(adapter) = instance.window_adapter_or_default() else { return };
    let Some(local_tree) = WindowInner::from_pub(adapter.window()).try_component() else {
        return;
    };
    // `Value` is not `Send` because a model is a shared reference, but the
    // carried value is one that can travel, and the task runs before
    // `run_on_screen_tree` returns.
    struct Carried {
        name: String,
        value: Value,
    }
    unsafe impl Send for Carried {}
    let state = Carried { name: name.to_string(), value };
    struct CarriedPtr(*const Carried);
    unsafe impl Send for CarriedPtr {}
    impl CarriedPtr {
        fn get(&self) -> &Carried {
            // SAFETY: the pointee outlives this call, which returns before it.
            unsafe { &*self.0 }
        }
    }
    let ptr = CarriedPtr(&state as *const _);
    let run = move |tree: &ItemTreeRc| {
        let carried = ptr.get();
        let screen = VRef::downcast_pin::<Instance>(VRc::borrow_pin(tree))
            .expect("the tree on screen is not this interpreter component");
        let _ = set_local(screen.get_ref(), &carried.name, carried.value.clone());
    };
    adapter.run_on_screen_tree(local_tree, Box::new(run));
}

/// The public properties of `instance` whose value can cross to another thread,
/// with their current values.
///
/// A backend that draws a tree of its own builds it from the declaration, so the
/// tree starts from the declared values and knows nothing the application set on
/// the instance it holds. This is what the instance offers to carry over: the
/// names and values that can travel, which the drawn tree can be given.
pub fn portable_public_state(instance: &Instance) -> Vec<(String, Value)> {
    let Some((public, sub)) = resolve_root(instance) else { return Vec::new() };
    let ctx = EvalContext::new(sub);
    public
        .public_properties
        .values()
        .filter(|prop| {
            prop.ty.is_property_type()
                && !prop.read_only()
                && i_slint_compiler::llr::is_thread_portable_type(&prop.ty)
        })
        .map(|prop| (prop.display_name.to_string(), load_property(&ctx, &prop.prop)))
        .collect()
}

/// Give `instance` the values [`portable_public_state`] collected from another
/// instance of the same component.
///
/// A name that no longer fits the component is skipped rather than refused,
/// because the two instances come from the same declaration and a mismatch can
/// only mean one of them is not what the caller thinks it is.
pub fn apply_portable_public_state(instance: &Instance, state: &[(String, Value)]) {
    for (name, value) in state {
        let _ = set_local(instance, name, value.clone());
    }
}

/// The sub-component that owns the item at `flat_item_index`, together with the
/// compilation unit its declarations live in.
///
/// `item_table` maps a flat tree index to the `(sub_component_path, local_item)`
/// that backs it.  The unit is returned by `Rc` so the caller can look a
/// declaration up in it without borrowing through the instance.
fn resolve_item(
    instance: &VRc<ItemTreeVTable, Instance>,
    flat_item_index: u32,
) -> Option<(Arc<CompilationUnit>, Pin<Rc<SubComponentInstance>>)> {
    let entry = instance.item_table.get(flat_item_index as usize)?.as_ref()?;
    let mut owner = instance.root_sub_component.clone();
    for &sub_idx in entry.0.iter() {
        owner = owner.sub_components[sub_idx].clone();
    }
    let cu = owner.compilation_unit.clone();
    Some((cu, owner))
}

/// The `Instance` whose item tree `target` is.
///
/// An [`ItemRc`] names its item by the item tree it lives in plus an index
/// within that tree, and an item tree belongs to one *instance*: a repeated
/// element gets one of its own, so index 3 of a list row and index 3 of the
/// window are different items. A caller that only holds the root instance
/// therefore has to find out which instance an item actually came from before
/// the index means anything.
fn instance_of_item_tree(
    target: &vtable::VRc<ItemTreeVTable>,
    root: &VRc<ItemTreeVTable, Instance>,
) -> Option<VRc<ItemTreeVTable, Instance>> {
    fn walk(
        sub: &Pin<Rc<crate::instance::SubComponentInstance>>,
        target: &vtable::VRc<ItemTreeVTable>,
    ) -> Option<VRc<ItemTreeVTable, Instance>> {
        for repeater in &sub.repeaters {
            for instance in repeater.instances_vec() {
                if vtable::VRc::ptr_eq(target, &vtable::VRc::into_dyn(instance.clone())) {
                    return Some(instance);
                }
                // The match was the whole sub-tree, but a nested repeated
                // element under it can be the one that was asked for.
                if let Some(found) = walk(&instance.root_sub_component, target) {
                    return Some(found);
                }
            }
        }
        for nested in &sub.sub_components {
            if let Some(found) = walk(nested, target) {
                return Some(found);
            }
        }
        None
    }

    let root_dyn = vtable::VRc::into_dyn(root.clone());
    if vtable::VRc::ptr_eq(target, &root_dyn) {
        return Some(root.clone());
    }
    walk(&root.root_sub_component, target)
}

/// The component that owns `item`, found the way a caller that was handed an
/// item rather than an index has to.
fn resolve_item_rc(
    instance: &VRc<ItemTreeVTable, Instance>,
    item: &ItemRc,
) -> Option<(Arc<CompilationUnit>, Pin<Rc<SubComponentInstance>>)> {
    let owner = instance_of_item_tree(item.item_tree(), instance)?;
    resolve_item(&owner, item.index())
}

/// The public property named `name`, found on the component that owns the item
/// or on one of the components around it, together with the sub-component whose
/// state holds it.
///
/// A composite component is not an item: a `CheckBox` contributes a `TouchArea`
/// and a few more items to the tree, and the `checked` property the user wrote
/// is compiled into bindings on them.  The items are what a caller can point at,
/// but the property lives on the component around them, so a name has to be
/// resolved against the component that owns the item rather than the item.
///
/// The search therefore walks towards the root.  It has to: a widget's own
/// property is declared on the outer component, while the item the user pointed
/// at may sit in a private sub-component that declares nothing of its own.  The
/// walk stops at the first component that declares the name, so the innermost
/// declaration wins, which is the one closest to the item.
///
/// Returning the sub-component alongside the declaration matters: the
/// declaration names where the value is, but the state lives in the instance,
/// and for a property found on an ancestor those are two different objects.
fn find_property_of_item<'a>(
    cu: &'a CompilationUnit,
    owner: &Pin<Rc<SubComponentInstance>>,
    name: &str,
) -> Option<(&'a SubComponentPublicProperty, Pin<Rc<SubComponentInstance>>)> {
    let mut current = Some(owner.clone());
    while let Some(sub) = current {
        if let Some(prop) = cu.sub_components[sub.sub_component_idx].public_properties.get(name) {
            return Some((prop, sub));
        }
        current = sub.parent.upgrade().map(Pin::new);
    }
    None
}

/// Where the value of a public property of the sub-component lives.
///
/// The LLR records the property by name, because a [`MemberReference`] would
/// have to be renumbered by the passes that drop unused members. Looking the
/// name up in the sub-component's own properties is the same lookup the
/// interpreter does for a component instantiated from `.slint` code.
fn member_reference_of(
    cu: &CompilationUnit,
    sub: &Pin<Rc<SubComponentInstance>>,
    prop: &SubComponentPublicProperty,
) -> MemberReference {
    let sc = &cu.sub_components[sub.sub_component_idx];
    let index = sc
        .properties
        .iter_enumerated()
        .find(|(_, p)| p.name == prop.property_name)
        .map(|(index, _)| index)
        .unwrap_or_else(|| {
            panic!(
                "public property `{}` is not a property of the sub-component",
                prop.property_name
            )
        });
    MemberReference::Relative {
        parent_level: 0,
        local_reference: LocalMemberReference {
            sub_component_path: Vec::new(),
            reference: LocalMemberIndex::Property(index),
        },
    }
}

/// Read a public property of the component that owns the item at
/// `flat_item_index`.
///
/// This is the composite-component counterpart of [`get`]: a caller that can
/// name an item can also reach the properties of the components that item lives
/// in, which is how `CheckBox.checked` is answered even though no item in the
/// tree is named `checked`.
pub fn get_for_item(
    instance: &VRc<ItemTreeVTable, Instance>,
    flat_item_index: u32,
    name: &str,
) -> Option<Value> {
    let (cu, owner) = resolve_item(instance, flat_item_index)?;
    read_property_of_component(&cu, &owner, name)
}

/// Read a public property of the component that owns `item`.
///
/// The counterpart of [`get_for_item`] for a caller that was handed the item
/// itself, which is what the render thread has.
pub fn get_for_item_rc(
    instance: &VRc<ItemTreeVTable, Instance>,
    item: &ItemRc,
    name: &str,
) -> Option<Value> {
    let (cu, owner) = resolve_item_rc(instance, item)?;
    read_property_of_component(&cu, &owner, name)
}

/// Write a public property of the component that owns the item at
/// `flat_item_index`: the counterpart of [`get_for_item`].
pub fn set_for_item(
    instance: &VRc<ItemTreeVTable, Instance>,
    flat_item_index: u32,
    name: &str,
    value: Value,
) -> Result<(), SetPropertyError> {
    let (cu, owner) =
        resolve_item(instance, flat_item_index).ok_or(SetPropertyError::NoSuchProperty)?;
    write_property_of_component(&cu, &owner, name, value)
}

/// Write a public property of the component that owns `item`: the counterpart
/// of [`get_for_item_rc`].
pub fn set_for_item_rc(
    instance: &VRc<ItemTreeVTable, Instance>,
    item: &ItemRc,
    name: &str,
    value: Value,
) -> Result<(), SetPropertyError> {
    let (cu, owner) = resolve_item_rc(instance, item).ok_or(SetPropertyError::NoSuchProperty)?;
    write_property_of_component(&cu, &owner, name, value)
}

fn read_property_of_component(
    cu: &Arc<CompilationUnit>,
    owner: &Pin<Rc<SubComponentInstance>>,
    name: &str,
) -> Option<Value> {
    let (prop, sub) = find_property_of_item(cu, owner, name)?;
    if !prop.ty.is_property_type() {
        return None;
    }
    let reference = member_reference_of(cu, &sub, prop);
    let ctx = EvalContext::new(sub);
    Some(load_property(&ctx, &reference))
}

fn write_property_of_component(
    cu: &Arc<CompilationUnit>,
    owner: &Pin<Rc<SubComponentInstance>>,
    name: &str,
    mut value: Value,
) -> Result<(), SetPropertyError> {
    let (prop, sub) =
        find_property_of_item(cu, owner, name).ok_or(SetPropertyError::NoSuchProperty)?;
    if !prop.ty.is_property_type() {
        return Err(SetPropertyError::NoSuchProperty);
    }
    if prop.visibility == PropertyVisibility::Output {
        return Err(SetPropertyError::AccessDenied);
    }
    if !check_and_coerce(&mut value, &prop.ty) {
        return Err(SetPropertyError::WrongType);
    }
    let reference = member_reference_of(cu, &sub, prop);
    let ctx = EvalContext::new(sub);
    store_property(&ctx, &reference, value);
    Ok(())
}

/// Return true if `value` matches `ty` — and coerce it in place when useful
/// (struct values get missing fields filled with the type's defaults).
pub(crate) fn check_and_coerce(value: &mut Value, ty: &Type) -> bool {
    match ty {
        Type::Void => true,
        Type::Invalid
        | Type::InferredProperty
        | Type::InferredCallback
        | Type::Callback(_)
        | Type::Function(_)
        | Type::ElementReference
        | Type::Closure => false,
        Type::Float32 | Type::Int32 => matches!(value, Value::Number(_)),
        Type::String => matches!(value, Value::String(_)),
        Type::Color | Type::Brush => matches!(value, Value::Brush(_)),
        Type::UnitProduct(_)
        | Type::Duration
        | Type::PhysicalLength
        | Type::LogicalLength
        | Type::Rem
        | Type::Angle
        | Type::Percent => matches!(value, Value::Number(_)),
        Type::Image => matches!(value, Value::Image(_)),
        Type::Bool => matches!(value, Value::Bool(_)),
        Type::Model => matches!(value, Value::Model(_) | Value::Bool(_) | Value::Number(_)),
        Type::PathData => matches!(value, Value::PathData(_)),
        Type::DataTransfer => matches!(value, Value::DataTransfer(_)),
        Type::Easing => matches!(value, Value::EasingCurve(_)),
        Type::MouseCursor => matches!(value, Value::MouseCursorInner(_)),
        Type::Array(inner) => match value {
            Value::Model(m) => {
                let mut ok = true;
                for i in 0..m.row_count() {
                    if let Some(mut v) = m.row_data(i)
                        && !check_and_coerce(&mut v, inner)
                    {
                        ok = false;
                        break;
                    }
                }
                ok
            }
            _ => false,
        },
        Type::Struct(s) => {
            let Value::Struct(str_value) = value else { return false };
            // Every provided key must be declared on the struct and have the
            // right type.
            let keys: Vec<String> = str_value.iter().map(|(k, _)| k.to_string()).collect();
            for k in keys {
                let Some(field_ty) = s.fields.get(k.as_str()) else {
                    return false;
                };
                let Some(v) = str_value.get_field(&k).cloned() else { continue };
                let mut v = v;
                if !check_and_coerce(&mut v, field_ty) {
                    return false;
                }
                str_value.set_field(k, v);
            }
            // Fill any declared field that wasn't provided with the type
            // default so downstream consumers always see a complete struct.
            for (k, field_ty) in s.fields.iter() {
                if str_value.get_field(k.as_str()).is_none() {
                    str_value
                        .set_field(k.to_string(), crate::eval::default_value_for_type(field_ty));
                }
            }
            true
        }
        Type::Enumeration(en) => {
            matches!(value, Value::EnumerationValue(name, _) if name == en.name.as_str())
        }
        Type::Keys => matches!(value, Value::Keys(_)),
        Type::LayoutCache => matches!(value, Value::LayoutCache(_)),
        Type::ArrayOfU16 => matches!(value, Value::ArrayOfU16(_)),
        Type::ComponentFactory => matches!(value, Value::ComponentFactory(_)),
        Type::StyledText => matches!(value, Value::StyledText(_)),
    }
}

/// Invoke a public callback or function by name.
pub fn invoke(
    instance: &VRc<ItemTreeVTable, Instance>,
    name: &str,
    args: &[Value],
) -> Option<Value> {
    invoke_on(instance, name, args)
}

/// Whether a call to `name` on `instance` can be carried to the thread that
/// draws the window, run there, and bring its value back.
///
/// A window-rooted component's callback or function runs against the tree the
/// window shows, which a backend may draw on another thread, so its arguments
/// travel there and its return value comes back. Only plain values can make the
/// trip; a model or a callback is a reference into the tree the caller holds.
pub fn can_cross_threads(instance: &Instance, name: &str) -> bool {
    use i_slint_compiler::llr::is_thread_portable_type;
    let Some((public, _)) = resolve_root(instance) else { return false };
    let Some(prop) = find_public_property(public, name) else { return false };
    let (args, ret) = match &prop.ty {
        Type::Callback(callback) => (&callback.args, &callback.return_type),
        Type::Function(function) => (&function.args, &function.return_type),
        _ => return false,
    };
    is_thread_portable_type(ret) && args.iter().all(is_thread_portable_type)
}

/// Run a public callback or function on `instance` without any thought for
/// another thread.
///
/// [`invoke`] is what the public API calls, and it may carry the call to the
/// tree the window shows; this is the call itself, taking the instance it runs
/// on directly so a backend that has that tree at hand can reach it.
pub fn invoke_on(instance: &Instance, name: &str, args: &[Value]) -> Option<Value> {
    let (public, sub) = resolve_root(instance)?;
    let prop = find_public_property(public, name)?;
    // Only callbacks and functions are callable; propagate a miss for
    // anything else so the public API surfaces a `NoSuchCallable` error.
    if !matches!(&prop.ty, Type::Callback(_) | Type::Function(_)) {
        return None;
    }
    let ctx = EvalContext::new(sub);
    Some(if matches!(&prop.ty, Type::Function(_)) || prop.prop.is_function() {
        invoke_function(&ctx, &prop.prop, args.to_vec())
    } else {
        invoke_callback(&ctx, &prop.prop, args)
    })
}

/// Install a host-side handler on a public callback.
///
/// Host handlers take the callback args as a flat `&[Value]` and return a
/// `Value`; they're adapted to the sub-component's
/// `Callback<[Value], Value>` shape before being installed.
pub fn set_callback(
    instance: &VRc<ItemTreeVTable, Instance>,
    name: &str,
    handler: Box<dyn Fn(&[Value]) -> Value>,
) -> Result<(), ()> {
    let (public, sub) = resolve(instance).ok_or(())?;
    let prop = find_public_property(public, name).ok_or(())?;
    match &prop.prop {
        MemberReference::Relative { parent_level, local_reference } => {
            let target = walk_to(sub, *parent_level, &local_reference.sub_component_path);
            match &local_reference.reference {
                i_slint_compiler::llr::LocalMemberIndex::Callback(idx) => {
                    let cb = Pin::as_ref(&target.callbacks[*idx]);
                    cb.set_handler(handler);
                    if let Some(tracker) = target.callback_trackers[*idx].as_ref() {
                        Pin::as_ref(tracker).mark_dirty();
                    }
                    Ok(())
                }
                i_slint_compiler::llr::LocalMemberIndex::Native {
                    item_index, prop_name, ..
                } => {
                    Pin::as_ref(&target.items[*item_index]).set_callback_handler(prop_name, handler)
                }
                _ => Err(()),
            }
        }
        MemberReference::Global { global_index, member } => {
            // An alias like `callback foo <=> Glo.bar` surfaces as a
            // public property whose `prop` is a global reference. Route
            // directly to the matching `GlobalInstance::callbacks` slot.
            let global_inst = instance.globals.get(*global_index).ok_or(())?;
            let i_slint_compiler::llr::LocalMemberIndex::Callback(idx) = member else {
                return Err(());
            };
            let cb = Pin::as_ref(&global_inst.callbacks[*idx]);
            cb.set_handler(handler);
            if let Some(tracker) = global_inst.callback_trackers[*idx].as_ref() {
                Pin::as_ref(tracker).mark_dirty();
            }
            Ok(())
        }
    }
}

fn resolve(
    instance: &VRc<ItemTreeVTable, Instance>,
) -> Option<(&PublicComponent, Pin<Rc<SubComponentInstance>>)> {
    resolve_root(instance)
}

/// The public component and the root sub-component of an instance, addressed
/// directly rather than through the `VRc` that normally carries it.
fn resolve_root(instance: &Instance) -> Option<(&PublicComponent, Pin<Rc<SubComponentInstance>>)> {
    let cu = &instance.root_sub_component.compilation_unit;
    let public_index = instance.public_component_index?;
    let public = cu.public_components.get(public_index)?;
    Some((public, instance.root_sub_component.clone()))
}

/// Name-based lookup of a public property on an exported global singleton.
/// Returns the looked-up property plus the runtime `GlobalInstance`.
fn resolve_global<'a>(
    instance: &'a VRc<ItemTreeVTable, Instance>,
    global_name: &str,
    prop_name: &str,
) -> Option<(&'a PublicProperty, Rc<crate::globals::GlobalInstance>)> {
    let cu = &instance.root_sub_component.compilation_unit;
    let (_global, global_instance) = instance.globals.find_by_name(cu, global_name)?;
    let global_instance = global_instance.clone();
    let needle = i_slint_compiler::parser::normalize_identifier(prop_name);
    let global = &cu.globals[global_instance.global_idx];
    let prop = global.public_properties.get(needle.as_str())?;
    Some((prop, global_instance))
}

/// Resolve a public global property to its underlying `(GlobalInstance,
/// LocalMemberIndex)`. A `data <=> G1.data` alias surfaces as
/// `MemberReference::Global` pointing at a *different* global from the one
/// whose `public_properties` map carries the entry, so the member index
/// must be resolved against the target global, not the source.
fn resolve_global_property(
    instance: &VRc<ItemTreeVTable, Instance>,
    source_inst: Rc<crate::globals::GlobalInstance>,
    prop: &PublicProperty,
) -> Option<(Rc<crate::globals::GlobalInstance>, i_slint_compiler::llr::LocalMemberIndex)> {
    match &prop.prop {
        MemberReference::Global { global_index, member } => {
            let target = instance.globals.get(*global_index)?.clone();
            Some((target, member.clone()))
        }
        MemberReference::Relative { local_reference, .. } => {
            Some((source_inst, local_reference.reference.clone()))
        }
    }
}

/// Read a property on a public global singleton.
pub fn get_global(
    instance: &VRc<ItemTreeVTable, Instance>,
    global_name: &str,
    prop_name: &str,
) -> Option<Value> {
    let (prop, source_inst) = resolve_global(instance, global_name, prop_name)?;
    let (target_inst, member) = resolve_global_property(instance, source_inst, prop)?;
    match member {
        i_slint_compiler::llr::LocalMemberIndex::Property(idx) => {
            Some(Pin::as_ref(&target_inst.properties[idx]).get())
        }
        _ => None,
    }
}

/// Write a property on a public global singleton.
pub fn set_global(
    instance: &VRc<ItemTreeVTable, Instance>,
    global_name: &str,
    prop_name: &str,
    mut value: Value,
) -> Result<(), SetPropertyError> {
    let (prop, source_inst) =
        resolve_global(instance, global_name, prop_name).ok_or(SetPropertyError::NoSuchProperty)?;
    if prop.read_only() {
        return Err(SetPropertyError::AccessDenied);
    }
    if !check_and_coerce(&mut value, &prop.ty) {
        return Err(SetPropertyError::WrongType);
    }
    let (target_inst, member) = resolve_global_property(instance, source_inst, prop)
        .ok_or(SetPropertyError::NoSuchProperty)?;
    match member {
        i_slint_compiler::llr::LocalMemberIndex::Property(_) => {
            crate::eval::store_global(&target_inst, &member, value);
            Ok(())
        }
        _ => Err(SetPropertyError::NoSuchProperty),
    }
}

/// Install a handler on a public callback declared on an exported global
/// singleton.
pub fn set_global_callback(
    instance: &VRc<ItemTreeVTable, Instance>,
    global_name: &str,
    callback_name: &str,
    handler: Box<dyn Fn(&[Value]) -> Value>,
) -> Result<(), ()> {
    let (prop, source_inst) = resolve_global(instance, global_name, callback_name).ok_or(())?;
    let (target_inst, member) = resolve_global_property(instance, source_inst, prop).ok_or(())?;
    match member {
        i_slint_compiler::llr::LocalMemberIndex::Callback(idx) => {
            if let Some(native) = &target_inst.native {
                let g = &target_inst.compilation_unit.globals[target_inst.global_idx];
                return native.as_ref().set_callback_handler(&g.callbacks[idx].name, handler);
            }
            let cb = Pin::as_ref(&target_inst.callbacks[idx]);
            cb.set_handler(handler);
            if let Some(tracker) = target_inst.callback_trackers[idx].as_ref() {
                Pin::as_ref(tracker).mark_dirty();
            }
            Ok(())
        }
        _ => Err(()),
    }
}

/// Invoke a public callback or function on an exported global singleton.
pub fn invoke_global(
    instance: &VRc<ItemTreeVTable, Instance>,
    global_name: &str,
    name: &str,
    args: &[Value],
) -> Option<Value> {
    let (prop, source_inst) = resolve_global(instance, global_name, name)?;
    let (target_inst, member) = resolve_global_property(instance, source_inst, prop)?;
    match member {
        LocalMemberIndex::Callback(idx) => {
            let cu = &target_inst.compilation_unit;
            let cb_decl = &cu.globals[target_inst.global_idx].callbacks[idx];
            if let Some(native) = &target_inst.native {
                let res =
                    native.as_ref().invoke_callback(&cb_decl.name, args).unwrap_or(Value::Void);
                return Some(crate::eval::ensure_typed_default(res, &cb_decl.ret_ty));
            }
            let cb = Pin::as_ref(&target_inst.callbacks[idx]);
            Some(crate::eval::ensure_typed_default(cb.call(args), &cb_decl.ret_ty))
        }
        LocalMemberIndex::Function(fn_idx) => {
            let cu = &instance.root_sub_component.compilation_unit;
            let global = &cu.globals[target_inst.global_idx];
            let function = &global.functions[fn_idx];
            let expr = function.code.read().clone();
            let mut ctx = crate::eval::EvalContext::for_global(
                std::rc::Rc::downgrade(&instance.globals),
                cu.clone(),
            );
            ctx.function_arg_types = function.args.clone();
            ctx.function_arguments = args.to_vec();
            Some(crate::eval::eval_expression(&mut ctx, &expr))
        }
        _ => None,
    }
}

fn walk_to(
    start: Pin<Rc<SubComponentInstance>>,
    parent_level: usize,
    path: &[i_slint_compiler::llr::SubComponentInstanceIdx],
) -> Pin<Rc<SubComponentInstance>> {
    crate::eval::walk_sub_path(crate::eval::walk_parent(&start, parent_level), path)
}

#[cfg(test)]
mod tests {
    //! The composite-component case: a name that no item answers to, resolved
    //! against the component that owns the item.

    use super::*;
    use crate::api::Compiler;
    use i_slint_core::item_tree::TraversalOrder;
    use i_slint_core::items::{TextInput, TouchArea};

    fn compile(code: &str, name: &str) -> crate::api::ComponentInstance {
        i_slint_backend_testing::init_no_event_loop();
        let compiler = spin_on::spin_on(
            Compiler::default().build_from_source(code.into(), Default::default()),
        );
        assert!(!compiler.has_errors(), "{:?}", compiler.diagnostics().collect::<Vec<_>>());
        compiler.component(name).expect("component compiles").create().unwrap()
    }

    /// The flat index of the first `TouchArea` in the tree, which is how a
    /// caller that hit-tested one arrives here: a control is an item, and the
    /// index is what says which item.
    fn first_touch_area(instance: &VRc<ItemTreeVTable, Instance>) -> u32 {
        let mut found = None;
        i_slint_core::item_tree::visit_items(
            &VRc::into_dyn(instance.clone()),
            TraversalOrder::BackToFront,
            |tree, _item, index, _| {
                if found.is_none()
                    && ItemRc::new(tree.clone(), index).downcast::<TouchArea>().is_some()
                {
                    found = Some(index);
                }
                i_slint_core::item_tree::ItemVisitorResult::Continue(())
            },
            (),
        );
        found.expect("the component has a TouchArea")
    }

    /// Every `TouchArea` in the tree, as the item reference a caller would
    /// hold after a hit test.
    fn touch_areas(instance: &VRc<ItemTreeVTable, Instance>) -> Vec<ItemRc> {
        let mut found = Vec::new();
        i_slint_core::item_tree::visit_items(
            &VRc::into_dyn(instance.clone()),
            TraversalOrder::BackToFront,
            |tree, _item, index, _| {
                let item = ItemRc::new(tree.clone(), index);
                if item.downcast::<TouchArea>().is_some() {
                    found.push(item);
                }
                i_slint_core::item_tree::ItemVisitorResult::Continue(())
            },
            (),
        );
        found
    }

    /// A repeated element gets an item tree of its own, so the same index in
    /// two rows names two different items. A caller that passes the item it
    /// hit-tested must still reach the row that item belongs to, rather than
    /// whatever row happens to sit at that index in the first one found.
    #[test]
    fn a_repeated_element_is_reached_through_its_own_item() {
        let instance = compile(
            r#"
                import { CheckBox } from "std-widgets.slint";
                export struct Row { title: string, done: bool }
                export component TestCase inherits Window {
                    width: 300px; height: 300px;
                    in property <[Row]> rows: [
                        { title: "first", done: false },
                        { title: "second", done: false },
                    ];
                    for row in root.rows: HorizontalLayout {
                        CheckBox {
                            text: row.title;
                            checked <=> row.done;
                        }
                    }
                }
            "#,
            "TestCase",
        );
        let areas = touch_areas(instance.inner.vrc());
        assert_eq!(areas.len(), 2, "one CheckBox TouchArea per row");
        // The rows are the same component, so the indices agree -- which is
        // exactly what makes it possible to answer for the wrong one.
        assert_eq!(areas[0].index(), areas[1].index());
        assert!(!vtable::VRc::ptr_eq(areas[0].item_tree(), areas[1].item_tree(),));

        for (area, expected) in areas.iter().zip(["first", "second"]) {
            assert_eq!(
                get_for_item_rc(instance.inner.vrc(), area, "text"),
                Some(Value::String(expected.into()))
            );
        }

        // A write through one row's item reaches that row's component and not
        // the other, which is the property that the shared index cannot give.
        // The write is aimed at the `CheckBox`, so it travels down the two-way
        // link into the row of the model it came from, and only that one.
        set_for_item_rc(instance.inner.vrc(), &areas[1], "checked", Value::Bool(true)).unwrap();
        assert_eq!(
            get_for_item_rc(instance.inner.vrc(), &areas[1], "checked"),
            Some(Value::Bool(true))
        );
        assert_eq!(
            get_for_item_rc(instance.inner.vrc(), &areas[0], "checked"),
            Some(Value::Bool(false))
        );
    }

    /// A property declared on the component that owns the pointed-at item is
    /// reached by naming the item, which is the whole point: the caller has an
    /// item, and the property belongs to the component around it.
    #[test]
    fn property_of_the_owning_component_is_reached_through_its_item() {
        let instance = compile(
            r#"
                export component TestCase inherits Window {
                    width: 200px; height: 100px;
                    in-out property <bool> flagged: true;
                    in-out property <int> counter: 7;
                    TouchArea { }
                }
            "#,
            "TestCase",
        );
        let root = instance.inner.vrc().clone();
        let root = &root;
        let flat = first_touch_area(root);

        assert_eq!(get_for_item(root, flat, "flagged"), Some(Value::Bool(true)));
        assert_eq!(get_for_item(root, flat, "counter"), Some(Value::Number(7.)));
        // A name the component does not declare resolves to nothing rather
        // than to a default.
        assert_eq!(get_for_item(root, flat, "nope"), None);

        set_for_item(root, flat, "flagged", Value::Bool(false)).unwrap();
        assert_eq!(get_for_item(root, flat, "flagged"), Some(Value::Bool(false)));

        // A name the component does not declare, and a wrongly typed value, are
        // both refused rather than quietly doing something else.
        assert_eq!(
            set_for_item(root, flat, "nope", Value::Bool(false)),
            Err(SetPropertyError::NoSuchProperty)
        );
        assert_eq!(
            set_for_item(root, flat, "flagged", Value::String("x".into())),
            Err(SetPropertyError::WrongType)
        );
    }

    /// The property of a component that comes from another file, which is what
    /// every built-in widget is.
    ///
    /// `std-widgets.slint` is compiled into its own compilation unit, and a
    /// `CheckBox` inlined into the importing file has its state in the
    /// importing file's unit. Reaching `checked` therefore cannot go through
    /// that unit's `PublicComponent` list, where the widget is absent.
    #[test]
    fn property_of_a_component_from_another_file_is_reached_through_its_item() {
        let instance = compile(
            r#"
                import { CheckBox } from "std-widgets.slint";
                export component TestCase inherits Window {
                    width: 200px; height: 100px;
                    CheckBox { text: "with milk"; checked: true; }
                }
            "#,
            "TestCase",
        );
        let root = instance.inner.vrc().clone();
        let root = &root;
        let flat = first_touch_area(root);

        assert_eq!(get_for_item(root, flat, "checked"), Some(Value::Bool(true)));
        // The rest of the widget's public API resolves the same way.
        assert_eq!(get_for_item(root, flat, "text"), Some(Value::String("with milk".into())));
        // The window's own property is not reachable from the widget, because
        // the search walks the other way.
        assert_eq!(get_for_item(root, flat, "width"), None);

        set_for_item(root, flat, "checked", Value::Bool(false)).unwrap();
        assert_eq!(get_for_item(root, flat, "checked"), Some(Value::Bool(false)));
    }

    /// A `LineEdit` is the case that matters for typing, and it is the one
    /// that shows what a property has to be written through.
    ///
    /// `text` is declared on the widget and two-way bound to the application,
    /// so the value the application reads has to be the value the user typed.
    /// A write straight to the `TextInput` item detaches the binding that
    /// carries the value out, and the application is left with a field it can
    /// see the text in and cannot read.
    #[test]
    fn what_was_typed_reaches_the_application_through_the_widgets_own_property() {
        let instance = compile(
            r#"
                import { LineEdit } from "std-widgets.slint";
                export component TestCase inherits Window {
                    width: 200px; height: 100px;
                    in-out property <string> typed <=> line-edit.text;
                    line-edit := LineEdit { }
                }
            "#,
            "TestCase",
        );
        let root = instance.inner.vrc().clone();
        let root = &root;
        let text_input = first_text_input(root).downcast::<TextInput>().unwrap();
        text_input.text.set("buy milk".into());

        assert_eq!(
            instance.get_property("typed"),
            Ok(Value::String("buy milk".into())),
            "the application must see what was typed into its widget"
        );
    }

    /// A widget property that `remove_aliases` folded into the item it was
    /// bound to is not a property of the sub-component, so there is no name to
    /// find it under. The item table is the only route to it, which is why the
    /// component seam is a fallback and not the whole of the lookup.
    #[test]
    fn a_widget_property_that_lives_on_the_item_is_not_in_the_components_public_api() {
        let instance = compile(
            r#"
                import { LineEdit } from "std-widgets.slint";
                export component TestCase inherits Window {
                    width: 200px; height: 100px;
                    line-edit := LineEdit { }
                }
            "#,
            "TestCase",
        );
        let root = instance.inner.vrc().clone();
        let root = &root;
        let text_input = first_text_input(root);

        assert_eq!(get_for_item_rc(root, &text_input, "text"), None);
    }

    fn first_text_input(instance: &VRc<ItemTreeVTable, Instance>) -> ItemRc {
        let mut found = None;
        i_slint_core::item_tree::visit_items(
            &VRc::into_dyn(instance.clone()),
            TraversalOrder::BackToFront,
            |tree, _item, index, _| {
                if found.is_none()
                    && ItemRc::new(tree.clone(), index)
                        .downcast::<i_slint_core::items::TextInput>()
                        .is_some()
                {
                    found = Some(ItemRc::new(tree.clone(), index));
                }
                i_slint_core::item_tree::ItemVisitorResult::Continue(())
            },
            (),
        );
        found.expect("the component has a TextInput")
    }
}
