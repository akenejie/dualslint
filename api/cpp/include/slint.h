// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell:ignore itemvtable
#pragma once

#include "private/slint_internal.h"
#include "private/slint_platform_internal.h"
#include "private/slint_qt_internal.h"
#include "private/slint_window.h"
#include "private/slint_models.h"
#include "private/slint_item_tree.h"
#include "private/slint_keys.h"
#include "private/slint_data_transfer.h"

#include <vector>
#include <chrono>
#include <span>
#include <concepts>
#include <limits>

#ifndef SLINT_FEATURE_FREESTANDING
#    include <mutex>
#    include <condition_variable>
#    include <cstdint>
#    include <memory>
#endif

/// Request a window repaint through the render thread; see
/// `slint::render_thread::request_redraw()`.
extern "C" void slint_render_thread_request_redraw();

extern "C" uint64_t slint_render_thread_control_at(float x, float y);
extern "C" uint64_t slint_render_thread_hit_test(float x, float y);
extern "C" void slint_render_thread_apply_control_state(uint64_t id, bool hovered, bool pressed);
/// Creates a component on the calling (render) thread and returns the address
/// of a heap-allocated owning pointer to it; see
/// `slint::render_thread::attach_component()`.
using SlintRenderThreadFactory = void *(*)();
extern "C" bool slint_render_thread_attach_component(SlintRenderThreadFactory factory);

/// Tag values of `SlintControlPropertyValue::tag`.
#define SLINT_CONTROL_PROPERTY_BOOL 0u
#define SLINT_CONTROL_PROPERTY_NUMBER 1u
#define SLINT_CONTROL_PROPERTY_TEXT 2u
#define SLINT_CONTROL_PROPERTY_COLOR 3u

/// A property value to assign to a borrowed control, as a tagged struct; see
/// `slint::render_thread::PropertyValue`.
struct SlintControlPropertyValue {
    uint32_t tag;
    float number;
    bool bool_;
    uint8_t color[4];
    /// NUL-terminated UTF-8, or null for the empty string.
    const char *text;
};

extern "C" bool slint_render_thread_set_control_property(
        uint64_t id, const char *property, SlintControlPropertyValue value);

/// Read one of a borrowed control's properties, blocking until the render
/// thread answers. Returns whether the id and the property name resolved.
///
/// When the value is a string, `out.text` borrows a buffer owned by the caller
/// of this function; it stays valid until the next read on the same thread.
extern "C" bool slint_render_thread_get_control_property(
        uint64_t id, const char *property, SlintControlPropertyValue *out);
struct SlintControlPropertyValue;

/// Read one property of a component handed to the render thread.
///
/// `component` is the pointer the factory returned, `item` is the item that
/// carried the query, and `property` the name the render thread could not
/// resolve against its own items. The value goes into `out`; returning `false`
/// says the component declares no such property, which is not an error but the
/// answer "look elsewhere".
///
/// When the tag written into `out` is `SLINT_CONTROL_PROPERTY_TEXT`, `out.text`
/// must point to a NUL-terminated string that stays valid until the next read or
/// write on the same thread: the value is copied after this call returns, so a
/// buffer this call owns and destroys on its way out is gone by then. Holding
/// the text in whatever the component keeps it in satisfies this, which is where
/// the answer comes from anyway.
using SlintRenderThreadPropertyRead = bool (*)(const void *component, const void *item,
                                               const char *property,
                                               SlintControlPropertyValue *out);

/// Assign one property of a component handed to the render thread; the
/// counterpart of `SlintRenderThreadPropertyRead`, with the same contract plus
/// `value`, which the callee only reads.
using SlintRenderThreadPropertyWrite = bool (*)(const void *component, const void *item,
                                                 const char *property,
                                                 const SlintControlPropertyValue *value);

/// The pair of calls that answer for a component the render thread draws.
///
/// A `.slint` widget's property belongs to the component around its items rather
/// than to any item, so only the application that created the component can say
/// what it is. Both calls run on the render thread. A null `read` leaves every
/// query to the render thread's own items, and a null `write` refuses every
/// assignment.
struct SlintRenderThreadPropertyAccess {
    SlintRenderThreadPropertyRead read;
    SlintRenderThreadPropertyWrite write;
};

extern "C" bool
slint_render_thread_attach_component_with_property_access(SlintRenderThreadFactory factory,
                                                          SlintRenderThreadPropertyAccess access);

/// The `slint` namespace is the primary entry point into the Slint C++ API.
/// All available types are in this namespace.
///
/// See the Overview documentation for the C++ integration and how
/// to load `.slint` designs.
namespace slint {

namespace private_api {

/// Saturating float-to-int cast matching Rust's `as i32` (NaN maps to 0).
inline int saturating_float_to_int(double value)
{
    if (value != value) // NaN
        return 0;
    if (value >= std::numeric_limits<int>::max())
        return std::numeric_limits<int>::max();
    if (value <= std::numeric_limits<int>::min())
        return std::numeric_limits<int>::min();
    return static_cast<int>(value);
}

/// Convert a slint `{height: length, width: length, x: length, y: length}` to a Rect
inline cbindgen_private::Rect convert_anonymous_rect(std::tuple<float, float, float, float> tuple)
{
    // alphabetical order
    auto [h, w, x, y] = tuple;
    return cbindgen_private::Rect { .x = x, .y = y, .width = w, .height = h };
}

inline void dealloc(const ItemTreeVTable *vtable, uint8_t *ptr,
                    [[maybe_unused]] vtable::Layout layout)
{
    vtable::dealloc(vtable, ptr, layout);
}

template<typename T>
inline vtable::Layout drop_in_place(ItemTreeRef item_tree)
{
    return vtable::drop_in_place<ItemTreeVTable, T>(item_tree);
}

#if !defined(DOXYGEN)
#    if defined(_WIN32) || defined(_WIN64)
// On Windows cross-dll data relocations are not supported:
//     https://docs.microsoft.com/en-us/cpp/c-language/rules-and-limitations-for-dllimport-dllexport?view=msvc-160
// so we have a relocation to a function that returns the address we seek. That
// relocation will be resolved to the locally linked stub library, the implementation of
// which will be patched.
#        define SLINT_GET_ITEM_VTABLE(VTableName) slint::private_api::slint_get_##VTableName()
#    else
#        define SLINT_GET_ITEM_VTABLE(VTableName) (&slint::private_api::VTableName)
#    endif
#endif // !defined(DOXYGEN)

inline std::optional<cbindgen_private::ItemRc>
upgrade_item_weak(const cbindgen_private::ItemWeak &item_weak)
{
    if (auto item_tree_strong = item_weak.item_tree.lock()) {
        return { { *item_tree_strong, item_weak.index } };
    } else {
        return std::nullopt;
    }
}

inline void debug(const SharedString &str)
{
    cbindgen_private::slint_debug(&str);
}

} // namespace private_api

namespace cbindgen_private {
inline LayoutInfo LayoutInfo::merge(const LayoutInfo &other) const
{
    // Note: This "logic" is duplicated from LayoutInfo::merge in layout.rs.
    return LayoutInfo { std::min(max, other.max),
                        std::min(max_percent, other.max_percent),
                        std::max(min, other.min),
                        std::max(min_percent, other.min_percent),
                        std::max(preferred, other.preferred),
                        std::min(stretch, other.stretch) };
}
inline bool operator==(const EasingCurve &a, const EasingCurve &b)
{
    if (a.tag != b.tag) {
        return false;
    } else if (a.tag == EasingCurve::Tag::CubicBezier) {
        return std::equal(a.cubic_bezier._0, a.cubic_bezier._0 + 4, b.cubic_bezier._0);
    } else if (a.tag == EasingCurve::Tag::Spring) {
        return a.spring._0 == b.spring._0;
    }
    return true;
}
}

namespace private_api {

inline static void register_item_tree(const vtable::VRc<ItemTreeVTable> *c,
                                      const std::optional<slint::Window> &maybe_window)
{
    const cbindgen_private::WindowAdapterRcOpaque *window_ptr =
            maybe_window.has_value() ? &maybe_window->window_handle().handle() : nullptr;
    cbindgen_private::slint_register_item_tree(c, window_ptr);
}

inline SharedVector<float> solve_box_layout(const cbindgen_private::BoxLayoutData &data,
                                            cbindgen_private::Slice<int> repeater_indices)
{
    SharedVector<float> result;
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::slint_solve_box_layout(&data, ri, &result);
    return result;
}

inline SharedVector<float> solve_box_layout_ortho(const cbindgen_private::BoxLayoutOrthoData &data,
                                                  cbindgen_private::Slice<int> repeater_indices)
{
    SharedVector<float> result;
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::slint_solve_box_layout_ortho(&data, ri, &result);
    return result;
}

inline SharedVector<uint16_t>
organize_grid_layout(cbindgen_private::Slice<cbindgen_private::GridLayoutInputData> input_data,
                     cbindgen_private::Slice<int> repeater_indices,
                     cbindgen_private::Slice<int> repeater_steps)
{
    SharedVector<uint16_t> result;
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::Slice<uint32_t> rs =
            make_slice(reinterpret_cast<uint32_t *>(repeater_steps.ptr), repeater_steps.len);
    cbindgen_private::slint_organize_grid_layout(input_data, ri, rs, &result);
    return result;
}

inline SharedVector<uint16_t> organize_dialog_button_layout(
        cbindgen_private::Slice<cbindgen_private::GridLayoutInputData> input_data,
        cbindgen_private::Slice<DialogButtonRole> dialog_button_roles)
{
    SharedVector<uint16_t> result;
    cbindgen_private::slint_organize_dialog_button_layout(input_data, dialog_button_roles, &result);
    return result;
}

inline SharedVector<float>
solve_grid_layout(const cbindgen_private::GridLayoutData &data,
                  cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> constraints,
                  cbindgen_private::Orientation orientation,
                  cbindgen_private::Slice<int> repeater_indices,
                  cbindgen_private::Slice<int> repeater_steps)
{
    SharedVector<float> result;
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::Slice<uint32_t> rs =
            make_slice(reinterpret_cast<uint32_t *>(repeater_steps.ptr), repeater_steps.len);
    cbindgen_private::slint_solve_grid_layout(&data, constraints, orientation, ri, rs, &result);
    return result;
}

inline cbindgen_private::LayoutInfo
grid_layout_info(const cbindgen_private::GridLayoutOrganizedData &organized_data,
                 cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> constraints,
                 cbindgen_private::Slice<int> repeater_indices,
                 cbindgen_private::Slice<int> repeater_steps, float spacing,
                 const cbindgen_private::Padding &padding,
                 cbindgen_private::Orientation orientation)
{
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::Slice<uint32_t> rs =
            make_slice(reinterpret_cast<uint32_t *>(repeater_steps.ptr), repeater_steps.len);
    return cbindgen_private::slint_grid_layout_info(&organized_data, constraints, ri, rs, spacing,
                                                    &padding, orientation);
}

inline cbindgen_private::LayoutInfo
box_layout_info(cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells, float spacing,
                const cbindgen_private::Padding &padding,
                cbindgen_private::LayoutAlignment alignment)
{
    return cbindgen_private::slint_box_layout_info(cells, spacing, &padding, alignment);
}

inline cbindgen_private::LayoutInfo
box_layout_info_ortho(cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells,
                      const cbindgen_private::Padding &padding)
{
    return cbindgen_private::slint_box_layout_info_ortho(cells, &padding);
}

inline SharedVector<float> solve_flexbox_layout(const cbindgen_private::FlexboxLayoutData &data,
                                                cbindgen_private::Slice<int> repeater_indices)
{
    SharedVector<float> result;
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::slint_solve_flexbox_layout(&data, ri, &result, nullptr, nullptr);
    return result;
}

// C thunk for the flexbox measure callbacks: unpack the type-erased functor
// and forward. `measure(index, w, h, known_w, known_h)` returns `{width,
// height}`; a dimension taffy has not determined (`known_* == false`) arrives
// pre-resolved to the cell's preferred size.
template<typename MeasureFn>
inline void flexbox_measure_thunk(void *user_data, uintptr_t child_index, float width, float height,
                                  bool known_width, bool known_height, float *out_width,
                                  float *out_height)
{
    auto *f = reinterpret_cast<MeasureFn *>(user_data);
    auto wh = (*f)(child_index, width, height, known_width, known_height);
    *out_width = wh.first;
    *out_height = wh.second;
}

// Like `solve_flexbox_layout`, but with a measure callback (see
// `flexbox_measure_thunk`) used for height-for-width.
template<typename MeasureFn>
inline SharedVector<float>
solve_flexbox_layout_with_measure(const cbindgen_private::FlexboxLayoutData &data,
                                  cbindgen_private::Slice<int> repeater_indices, MeasureFn measure)
{
    SharedVector<float> result;
    cbindgen_private::Slice<uint32_t> ri =
            make_slice(reinterpret_cast<uint32_t *>(repeater_indices.ptr), repeater_indices.len);
    cbindgen_private::slint_solve_flexbox_layout(
            &data, ri, &result, reinterpret_cast<const void *>(&flexbox_measure_thunk<MeasureFn>),
            reinterpret_cast<void *>(&measure));
    return result;
}

inline cbindgen_private::LayoutInfo
flexbox_layout_info_main_axis(cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells,
                              float spacing, const cbindgen_private::Padding &padding,
                              cbindgen_private::FlexboxLayoutWrap flex_wrap)
{
    return cbindgen_private::slint_flexbox_layout_info_main_axis(cells, spacing, &padding,
                                                                 flex_wrap);
}

inline float
flexbox_layout_unwrapped_main(cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells,
                              float spacing, const cbindgen_private::Padding &padding)
{
    return cbindgen_private::slint_flexbox_layout_unwrapped_main(cells, spacing, &padding);
}

inline cbindgen_private::LayoutInfo
flexbox_layout_info_cross_axis(cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells_h,
                               cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells_v,
                               cbindgen_private::Slice<cbindgen_private::FlexItemProps> flex_props,
                               float spacing_h, float spacing_v,
                               const cbindgen_private::Padding &padding_h,
                               const cbindgen_private::Padding &padding_v,
                               cbindgen_private::FlexboxLayoutDirection direction,
                               cbindgen_private::LayoutAlignment alignment,
                               cbindgen_private::FlexboxLayoutWrap flex_wrap, float constraint_size)
{
    return cbindgen_private::slint_flexbox_layout_info_cross_axis(
            cells_h, cells_v, flex_props, spacing_h, spacing_v, &padding_h, &padding_v, direction,
            alignment, flex_wrap, constraint_size);
}

// Like `flexbox_layout_info_cross_axis`, but with a measure callback (see
// `flexbox_measure_thunk`) so height-for-width cells are re-measured at the
// size taffy assigns them.
template<typename MeasureFn>
inline cbindgen_private::LayoutInfo flexbox_layout_info_cross_axis_with_measure(
        cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells_h,
        cbindgen_private::Slice<cbindgen_private::LayoutItemInfo> cells_v,
        cbindgen_private::Slice<cbindgen_private::FlexItemProps> flex_props, float spacing_h,
        float spacing_v, const cbindgen_private::Padding &padding_h,
        const cbindgen_private::Padding &padding_v,
        cbindgen_private::FlexboxLayoutDirection direction,
        cbindgen_private::LayoutAlignment alignment, cbindgen_private::FlexboxLayoutWrap flex_wrap,
        float constraint_size, MeasureFn measure)
{
    return cbindgen_private::slint_flexbox_layout_info_cross_axis_with_measure(
            cells_h, cells_v, flex_props, spacing_h, spacing_v, &padding_h, &padding_v, direction,
            alignment, flex_wrap, constraint_size,
            reinterpret_cast<const void *>(&flexbox_measure_thunk<MeasureFn>),
            reinterpret_cast<void *>(&measure));
}

/// Access the layout cache of an item within a repeater (standard cache)
template<typename T>
inline T layout_cache_access(const SharedVector<T> &cache, int offset, int repeater_index,
                             int entries_per_item)
{
    size_t idx = size_t(cache[offset]) + repeater_index * entries_per_item;
    return idx < cache.size() ? cache[idx] : 0;
}

/// Access the layout cache of an item within a grid repeater (two-level indirection cache)
/// Formula: cache[cache[jump_index] + repeater_index * stride + child_offset]
template<typename T>
inline T layout_cache_grid_repeater_access(const SharedVector<T> &cache, size_t jump_index,
                                           size_t repeater_index, size_t stride,
                                           size_t child_offset)
{
    size_t base = jump_index < cache.size() ? size_t(cache[jump_index]) : 0;
    size_t data_idx = base + repeater_index * stride + child_offset;
    return data_idx < cache.size() ? cache[data_idx] : 0;
}

template<typename VT, typename ItemType>
inline cbindgen_private::LayoutInfo
item_layout_info(VT *itemvtable, ItemType *item_ptr, cbindgen_private::Orientation orientation,
                 float cross_axis_constraint, WindowAdapterRc *window_adapter,
                 const ItemTreeRc &component_rc, uint32_t item_index)
{
    cbindgen_private::ItemRc item_rc { component_rc, item_index };
    return itemvtable->layout_info({ itemvtable, item_ptr }, orientation, cross_axis_constraint,
                                   window_adapter, &item_rc);
}
} // namespace private_api

namespace private_api {

template<typename T>
union MaybeUninitialized {
    T value;
    ~MaybeUninitialized() { }
    MaybeUninitialized() { }
    T take()
    {
        T result = std::move(value);
        value.~T();
        return result;
    }
};

inline vtable::VRc<cbindgen_private::MenuVTable>
create_menu_wrapper(const ItemTreeRc &menu_item_tree,
                    bool (*condition)(const ItemTreeRc *menu_tree) = nullptr,
                    bool (*visible)(const ItemTreeRc *menu_tree) = nullptr)
{
    MaybeUninitialized<vtable::VRc<cbindgen_private::MenuVTable>> maybe;
    cbindgen_private::slint_menus_create_wrapper(&menu_item_tree, &maybe.value, condition, visible);
    return maybe.take();
}

inline void setup_popup_menu_from_menu_item_tree(
        const vtable::VRc<cbindgen_private::MenuVTable> &shared,
        Property<std::shared_ptr<Model<cbindgen_private::MenuEntry>>> &entries,
        Callback<std::shared_ptr<Model<cbindgen_private::MenuEntry>>(cbindgen_private::MenuEntry)>
                &sub_menu,
        Callback<void(cbindgen_private::MenuEntry)> &activated)
{
    using cbindgen_private::MenuEntry;
    entries.set_binding([shared] {
        SharedVector<MenuEntry> entries_sv;
        shared.vtable()->sub_menu(shared.borrow(), nullptr, &entries_sv);
        std::vector<MenuEntry> entries_vec(entries_sv.begin(), entries_sv.end());
        return std::make_shared<VectorModel<MenuEntry>>(std::move(entries_vec));
    });
    sub_menu.set_handler([shared](const auto &entry) {
        SharedVector<MenuEntry> entries_sv;
        shared.vtable()->sub_menu(shared.borrow(), &entry, &entries_sv);
        std::vector<MenuEntry> entries_vec(entries_sv.begin(), entries_sv.end());
        return std::make_shared<VectorModel<MenuEntry>>(std::move(entries_vec));
    });
    activated.set_handler(
            [shared](const auto &entry) { shared.vtable()->activate(shared.borrow(), &entry); });
}

// Set up a menu bar from a menu item tree: register its shortcuts, install the native menu bar when
// the platform provides one, and always wire the fallback handlers, which also keep the tree alive
// on the component (the native menu bar holds only a weak reference to it).
inline void setup_menu_bar_from_menu_item_tree(
        const cbindgen_private::WindowAdapterRcOpaque *window_handle, bool no_native,
        const vtable::VRc<cbindgen_private::MenuVTable> &shared,
        Property<std::shared_ptr<Model<cbindgen_private::MenuEntry>>> &entries,
        Callback<std::shared_ptr<Model<cbindgen_private::MenuEntry>>(cbindgen_private::MenuEntry)>
                &sub_menu,
        Callback<void(cbindgen_private::MenuEntry)> &activated)
{
    cbindgen_private::slint_windowrc_setup_menu_bar_shortcuts(window_handle, &shared);
    if (!no_native && cbindgen_private::slint_windowrc_supports_native_menu_bar(window_handle)) {
        cbindgen_private::slint_windowrc_setup_native_menu_bar(window_handle, &shared);
    }
    setup_popup_menu_from_menu_item_tree(shared, entries, sub_menu, activated);
}

inline SharedString translate(const SharedString &original, const SharedString &context,
                              const SharedString &domain,
                              cbindgen_private::Slice<SharedString> arguments, int n,
                              const SharedString &plural)
{
    SharedString result = original;
    cbindgen_private::slint_translate(&result, &context, &domain, arguments, n, &plural);
    return result;
}

inline SharedString decimal_separator()
{
    SharedString out;
    cbindgen_private::slint_decimal_separator(&out);
    return out;
}

inline SharedString default_window_title()
{
    SharedString out;
    cbindgen_private::slint_default_window_title(&out);
    return out;
}

inline StyledText parse_markdown(const SharedString &format_string,
                                 cbindgen_private::Slice<StyledText> args)
{
    StyledText result;
    cbindgen_private::slint_parse_markdown(&format_string, args, &result);
    return result;
}

inline StyledText string_to_styled_text(const SharedString &text)
{
    StyledText result;
    cbindgen_private::slint_string_to_styled_text(&text, &result);
    return result;
}

inline StyledText color_to_styled_text(const Color &color)
{
    StyledText result;
    cbindgen_private::slint_color_to_styled_text(&color, &result);
    return result;
}

inline bool open_url(const SharedString &url, const WindowAdapterRc &window_adapter)
{
    return cbindgen_private::slint_open_url(&url, &window_adapter.handle());
}

inline void macos_bring_all_windows_to_front()
{
    cbindgen_private::slint_macos_bring_all_windows_to_front();
}

inline SharedString translate_from_bundle(std::span<const char8_t *const> strs,
                                          cbindgen_private::Slice<SharedString> arguments)
{
    SharedString result;
    cbindgen_private::slint_translate_from_bundle(
            make_slice((reinterpret_cast<char const *const *>(strs.data())), strs.size()),
            arguments, &result);
    return result;
}
inline SharedString
translate_from_bundle_with_plural(std::span<const char8_t *const> strs,
                                  std::span<const uint32_t> indices,
                                  std::span<uintptr_t (*const)(int32_t)> plural_rules,
                                  cbindgen_private::Slice<SharedString> arguments, int n)
{
    SharedString result;
    cbindgen_private::Slice<const char *> strs_slice =
            make_slice(reinterpret_cast<char const *const *>(strs.data()), strs.size());
    cbindgen_private::Slice<uint32_t> indices_slice =
            make_slice(reinterpret_cast<const uint32_t *>(indices.data()), indices.size());
    cbindgen_private::Slice<uintptr_t (*)(int32_t)> plural_rules_slice =
            make_slice(reinterpret_cast<uintptr_t (*const *)(int32_t)>(plural_rules.data()),
                       plural_rules.size());
    cbindgen_private::slint_translate_from_bundle_with_plural(
            strs_slice, indices_slice, plural_rules_slice, arguments, n, &result);
    return result;
}

template<typename Component>
inline float get_resolved_default_font_size(const Component &component)
{
    ItemTreeRc item_tree_rc = (*component.self_weak.lock()).into_dyn();
    return slint::cbindgen_private::slint_windowrc_resolved_default_font_size(&item_tree_rc);
}

} // namespace private_api

// Translator API is currently considered experimental due to discussions
// about the returned string type (SharedString vs. Cow<str> etc.). Also it
// is not available with no_std due to the tr crate.
// See discussion in https://github.com/slint-ui/slint/pull/10979.
#if defined(SLINT_FEATURE_EXPERIMENTAL) && !defined(SLINT_FEATURE_FREESTANDING)
/// Interface for an external translator.
struct Translator
{
    /// Destroys the translator.
    virtual ~Translator() { }
    /// Translate a singular string. Arguments are passed as UTF-8 strings.
    /// Slint will call this method from the thread which runs the event loop.
    virtual SharedString translate(std::string_view string, std::string_view context) const = 0;
    /// Translate a plural string. Arguments are passed as UTF-8 strings.
    /// Slint will call this method from the thread which runs the event loop.
    virtual SharedString ntranslate(uint64_t n, std::string_view singular, std::string_view plural,
                                    std::string_view context) const = 0;
};

namespace private_api {

/// Helper to dispatch calls from the Rust translator to the C++ translator.
struct TranslatorDispatcher
{
    static void drop(const void *obj) { delete cast(obj); }

    static void translate(const void *obj, private_api::Slice<uint8_t> string,
                          private_api::Slice<uint8_t> context, slint::SharedString *out)
    {
        *out = cast(obj)->translate(private_api::slice_to_string_view(string),
                                    private_api::slice_to_string_view(context));
    }

    static void ntranslate(const void *obj, uint64_t n, private_api::Slice<uint8_t> singular,
                           private_api::Slice<uint8_t> plural, private_api::Slice<uint8_t> context,
                           slint::SharedString *out)
    {
        *out = cast(obj)->ntranslate(n, private_api::slice_to_string_view(singular),
                                     private_api::slice_to_string_view(plural),
                                     private_api::slice_to_string_view(context));
    }

private:
    static const Translator *cast(const void *obj) { return static_cast<const Translator *>(obj); }
};

} // namespace private_api

/// Register a custom translator.
///
/// Allows using a custom translation framework by implementing the
/// `slint::Translator` interface. Passing `nullptr` will unregister any
/// previously registered translator.
///
/// Returns `true` on success, `false` if no platform is available.
///
/// Safety & Ownership:
/// * The ownership of the translator object is passed to Slint. It will be
///   destroyed automatically when the program quits or when
///   `set_external_translator()` is called the next time.
/// * The methods on the translator object will be called from the thread
///   which the Slint event loop is running.
///
/// The function is only available when Slint is compiled with
/// `SLINT_FEATURE_EXPERIMENTAL` and without `SLINT_FEATURE_FREESTANDING`.
///
/// Note that this function has no effect if the `.slint` file was compiled
/// with bundled translations.
///
/// Example:
/// \code
///     struct MyTranslator : public slint::Translator {
///       slint::SharedString translate(std::string_view string,
///                                     std::string_view context) const override {
///         return slint::SharedString("Singular String");
///       }
///
///       slint::SharedString ntranslate(uint64_t n,
///                                      std::string_view singular,
///                                      std::string_view plural,
///                                      std::string_view context) const override {
///         return slint::SharedString("Plural String");
///       }
///     };
///
///     slint::set_external_translator(std::make_unique<MyTranslator>());
/// \endcode
inline bool set_translator(std::unique_ptr<Translator> obj)
{
    const bool success = cbindgen_private::slint_translate_set_translator(
            obj.get(), &private_api::TranslatorDispatcher::drop,
            &private_api::TranslatorDispatcher::translate,
            &private_api::TranslatorDispatcher::ntranslate);
    if (success) {
        obj.release(); // Ownership is moved to Rust.
    }
    return success;
}
#endif

/// Forces all the strings that are translated with `@tr(...)` to be re-evaluated.
/// Call this function after changing the language at run-time and when translating
/// with either gettext or a custom translator. For bundled translations, there is no need
/// to call this function.
///
/// Example (assuming usage of gettext):
/// ```cpp
///     my_ui->global<LanguageSettings>().on_french_selected([] {
///        setenv("LANGUAGE", langs[l], true);
///        slint::update_all_translations();
///    });
/// ```
inline void update_all_translations()
{
    cbindgen_private::slint_translations_mark_dirty();
}

/// Select the current translation language when using bundled translations.
/// This function requires that the application's `.slint` file was compiled with bundled
/// translations. It must be called after creating the first component.
///
/// The language string is the locale, which matches the name of the folder that contains the
/// `LC_MESSAGES` folder. An empty string or `"en"` will select the default language.
///
/// Returns true if the language was selected; false if the language was not found in the list of
/// bundled translations.
inline bool select_bundled_translation(std::string_view language)
{
    return cbindgen_private::slint_translate_select_bundled_translation(
            slint::private_api::string_to_slice(language));
}

#if !defined(DOXYGEN)
cbindgen_private::Flickable::Flickable()
{
    slint_flickable_data_init(&data);
}
cbindgen_private::Flickable::~Flickable()
{
    slint_flickable_data_free(&data);
}

cbindgen_private::Path::Path()
{
    slint_path_fitted_cache_init(&fitted_path);
}
cbindgen_private::Path::~Path()
{
    slint_path_fitted_cache_free(&fitted_path);
}

cbindgen_private::SystemTrayIcon::SystemTrayIcon()
{
    slint_system_tray_icon_data_init(&data);
}
cbindgen_private::SystemTrayIcon::~SystemTrayIcon()
{
    slint_system_tray_icon_data_free(&data);
}

cbindgen_private::FocusScope::FocusScope()
{
    slint_maybe_key_binding_list_init(&key_bindings);
}
cbindgen_private::FocusScope::~FocusScope()
{
    slint_maybe_key_binding_list_free(&key_bindings);
}

cbindgen_private::NativeStyleMetrics::NativeStyleMetrics(void *)
{
    slint_native_style_metrics_init(this);
}

cbindgen_private::NativeStyleMetrics::~NativeStyleMetrics()
{
    slint_native_style_metrics_deinit(this);
}

cbindgen_private::NativePalette::NativePalette(void *)
{
    slint_native_palette_init(this);
}

cbindgen_private::NativePalette::~NativePalette()
{
    slint_native_palette_deinit(this);
}
#endif // !defined(DOXYGEN)

namespace private_api {
// Was used in Slint <= 1.1.0 to have an error message in case of mismatch
template<int Major, int Minor, int Patch>
struct [[deprecated]] VersionCheckHelper
{
};
}

/// Enum for the event loop mode parameter of the slint::run_event_loop() function.
/// It is used to determine when the event loop quits.
enum class EventLoopMode {
    /// The event loop quits when the last window is closed and the last
    /// visible system tray icon is hidden, or when slint::quit_event_loop()
    /// is called. A visible SystemTrayIcon keeps the loop alive on its own.
    QuitOnLastWindowClosed,

    /// The event loop keeps running until slint::quit_event_loop() is
    /// called, even when no windows or system tray icons are visible.
    RunUntilQuit
};

/// Enters the main event loop. This is necessary in order to receive
/// events from the windowing system in order to render to the screen
/// and react to user input.
///
/// The mode parameter determines when the loop returns. The default,
/// QuitOnLastWindowClosed, returns once the last window is closed and the
/// last visible system tray icon is hidden.
inline void run_event_loop(EventLoopMode mode = EventLoopMode::QuitOnLastWindowClosed)
{
    private_api::assert_main_thread();
    cbindgen_private::slint_run_event_loop(mode == EventLoopMode::QuitOnLastWindowClosed);
}

/// Schedules the main event loop for termination. This function is meant
/// to be called from callbacks triggered by the UI. After calling the function,
/// it will return immediately and once control is passed back to the event loop,
/// the initial call to slint::run_event_loop() will return.
inline void quit_event_loop()
{
    cbindgen_private::slint_quit_event_loop();
}

/// Adds the specified functor to an internal queue, notifies the event loop to wake up.
/// Once woken up, any queued up functors will be invoked.
/// This function is thread-safe and can be called from any thread, including the one
/// running the event loop. The provided functors will only be invoked from the thread
/// that started the event loop.
///
/// You can use this to set properties or use any other Slint APIs from other threads,
/// by collecting the code in a functor and queuing it up for invocation within the event loop.
///
/// The following example assumes that a status message received from a network thread is
/// shown in the UI:
///
/// ```
/// #include "my_application_ui.h"
/// #include <thread>
///
/// int main(int argc, char **argv)
/// {
///     auto ui = NetworkStatusUI::create();
///     ui->set_status_label("Pending");
///
///     slint::ComponentWeakHandle<NetworkStatusUI> weak_ui_handle(ui);
///     std::thread network_thread([=]{
///         std::string message = read_message_blocking_from_network();
///         slint::invoke_from_event_loop([&]() {
///             if (auto ui = weak_ui_handle.lock()) {
///                 ui->set_status_label(message);
///             }
///         });
///     });
///     ...
///     ui->run();
///     ...
/// }
/// ```
///
/// See also blocking_invoke_from_event_loop() for a blocking version of this function
template<std::invocable Functor>
void invoke_from_event_loop(Functor f)
{
    cbindgen_private::slint_post_event(
            [](void *data) { (*reinterpret_cast<Functor *>(data))(); }, new Functor(std::move(f)),
            [](void *data) { delete reinterpret_cast<Functor *>(data); });
}

#if !defined(SLINT_FEATURE_FREESTANDING) || defined(DOXYGEN)

/// Blocking version of invoke_from_event_loop()
///
/// Just like invoke_from_event_loop(), this will run the specified functor from the thread running
/// the slint event loop. But it will block until the execution of the functor is finished,
/// and return that value.
///
/// This function must be called from a different thread than the thread that runs the event loop
/// otherwise it will result in a deadlock. Calling this function if the event loop is not running
/// will also block forever or until the event loop is started in another thread.
///
/// The following example is reading the message property from a thread
///
/// ```
/// #include "my_application_ui.h"
/// #include <thread>
///
/// int main(int argc, char **argv)
/// {
///     auto ui = MyApplicationUI::create();
///     ui->set_status_label("Pending");
///
///     std::thread worker_thread([ui]{
///         while (...) {
///             auto message = slint::blocking_invoke_from_event_loop([ui]() {
///                return ui->get_message();
///             }
///             do_something(message);
///             ...
///         });
///     });
///     ...
///     ui->run();
///     ...
/// }
/// ```
template<std::invocable Functor>
auto blocking_invoke_from_event_loop(Functor f) -> std::invoke_result_t<Functor>
{
    std::optional<std::invoke_result_t<Functor>> result;
    std::mutex mtx;
    std::condition_variable cv;
    invoke_from_event_loop([&] {
        auto r = f();
        std::unique_lock lock(mtx);
        result = std::move(r);
        cv.notify_one();
    });
    std::unique_lock lock(mtx);
    cv.wait(lock, [&] { return result.has_value(); });
    return std::move(*result);
}

#    if !defined(DOXYGEN) // Doxygen doesn't see this as an overload of the previous one
// clang-format off
template<std::invocable Functor>
    requires(std::is_void_v<std::invoke_result_t<Functor>>)
void blocking_invoke_from_event_loop(Functor f)
// clang-format on
{
    std::mutex mtx;
    std::condition_variable cv;
    bool ok = false;
    invoke_from_event_loop([&] {
        f();
        std::unique_lock lock(mtx);
        ok = true;
        cv.notify_one();
    });
    std::unique_lock lock(mtx);
    cv.wait(lock, [&] { return ok; });
}
#    endif
#endif

/// Sets the application id for use on Wayland or X11 with
/// [xdg](https://specifications.freedesktop.org/desktop-entry-spec/latest/) compliant window
/// managers. This must be set before the window is shown.
inline void set_xdg_app_id(std::string_view xdg_app_id)
{
    private_api::assert_main_thread();
    SharedString s = xdg_app_id;
    cbindgen_private::slint_set_xdg_app_id(&s);
}

/// The `render_thread` namespace groups entry points that address the render
/// thread of the 2-thread render separation; the UI thread and worker threads
/// use them instead of going through a window adapter. In particular
/// [`slint::render_thread::request_redraw()`] requests a repaint of the
/// window directly on the render thread, whether or not a mirror component is
/// attached.
///
/// The render thread owns the controls, so a program that wants to touch one
/// borrows it here: identify the control, then assign a property. The UI thread
/// and any worker thread are equal peers on this API; neither is more
/// privileged than the other.
inline namespace render_thread {
    /// Request a window repaint through the render thread.
    ///
    /// This is the replacement for the removed `Window::request_redraw()`:
    /// the render thread owns the mirror component and re-encodes and
    /// re-presents the frame when asked, regardless of the UI-side window
    /// state. It is a no-op when the render thread was never started.
    inline void request_redraw()
    {
        slint_render_thread_request_redraw();
    }

    /// A control owned by the render thread, or the empty value when no control
    /// was found.
    ///
    /// Ids are only meaningful to the render thread that published them, and
    /// they stay valid for as long as that control exists.
    struct ControlId {
        /// Zero means "no control": a real control never has this id.
        uint64_t value = 0;

        /// Whether this refers to a control at all.
        bool has_value() const { return value != 0; }
        explicit operator bool() const { return has_value(); }

        bool operator==(const ControlId &) const = default;
    };

    /// The control under the logical point `x`, `y`.
    ///
    /// Reads the geometry the render thread last composited and does not wait
    /// for it, so it is cheap enough for a pointer move. Use
    /// [`hit_test()`] instead when the answer must reflect the present rather
    /// than the last frame.
    [[nodiscard]] inline ControlId control_at(float x, float y)
    {
        return ControlId { slint_render_thread_control_at(x, y) };
    }

    /// The control under the logical point `x`, `y`, resolved on the render
    /// thread itself. Blocks until the render thread answers, which is what
    /// makes it right for a click and wrong for a pointer move.
    [[nodiscard]] inline ControlId hit_test(float x, float y)
    {
        return ControlId { slint_render_thread_hit_test(x, y) };
    }

    /// A property value to assign to a borrowed control.
    struct PropertyValue {
        /// Which of the fields below carries the value.
        enum class Kind : uint32_t {
            Boolean,
            Number,
            Text,
            Color,
        };

        Kind kind = Kind::Boolean;
        bool boolean = false;
        float number = 0;
        /// RGBA, each component 0-255.
        uint8_t color[4] = { 0, 0, 0, 0 };
        /// NUL-terminated UTF-8. Only read when `kind` is `Text`.
        const char *text = nullptr;

        /// A `bool` property, such as a `TouchArea`'s `enabled`.
        static PropertyValue from_bool(bool v) noexcept
        {
            PropertyValue p;
            p.kind = Kind::Boolean;
            p.boolean = v;
            return p;
        }

        /// A numeric property, such as `opacity` or `width`.
        static PropertyValue from_number(float v) noexcept
        {
            PropertyValue p;
            p.kind = Kind::Number;
            p.number = v;
            return p;
        }

        /// A string property, such as a `Text`'s `text`.
        static PropertyValue from_text(const char *v) noexcept
        {
            PropertyValue p;
            p.kind = Kind::Text;
            p.text = v;
            return p;
        }

        /// A color property, such as a `Text`'s `color`.
        static PropertyValue from_color(uint8_t r, uint8_t g, uint8_t b,
                                        uint8_t a = 255) noexcept
        {
            PropertyValue p;
            p.kind = Kind::Color;
            p.color[0] = r;
            p.color[1] = g;
            p.color[2] = b;
            p.color[3] = a;
            return p;
        }
    };

    /// Assign one property of a borrowed control, blocking until the render
    /// thread confirms the assignment. Returns whether the property name
    /// resolved and the value was applied.
    ///
    /// A property that has a binding is detached first, the same way assigning
    /// through the normal API behaves.
    inline bool set_control_property(ControlId id, const char *property,
                                     PropertyValue value)
    {
        if (!id.has_value() || property == nullptr) {
            return false;
        }
        return slint_render_thread_set_control_property(
                id.value, property,
                SlintControlPropertyValue { static_cast<uint32_t>(value.kind),
                                            value.number, value.boolean,
                                            { value.color[0], value.color[1],
                                              value.color[2], value.color[3] },
                                            value.text });
    }

    /// Read one property of a borrowed control, blocking until the render
    /// thread answers. Returns whether the id and the property name resolved.
    ///
    /// This is how a caller learns what a control looks like without keeping
    /// a second copy of the component: the `.slint` side of the tree belongs
    /// to the render thread, so the answer comes from there.
    ///
    /// The text case borrows a buffer that is valid until the next read on
    /// the same thread; copy the string before reading again.
    inline std::optional<PropertyValue> get_control_property(ControlId id,
                                                             const char *property)
    {
        if (!id.has_value() || property == nullptr) {
            return std::nullopt;
        }
        SlintControlPropertyValue value {};
        if (!slint_render_thread_get_control_property(id.value, property, &value)) {
            return std::nullopt;
        }
        PropertyValue result {};
        result.kind = static_cast<PropertyValue::Kind>(value.tag);
        result.number = value.number;
        result.boolean = value.bool_;
        result.color[0] = value.color[0];
        result.color[1] = value.color[1];
        result.color[2] = value.color[2];
        result.color[3] = value.color[3];
        if (value.text != nullptr) {
            result.text = value.text;
        }
        return result;
    }

    /// Report a control as hovered and/or pressed on the render thread.
    ///
    /// This is how a pointer state that the program resolved itself is handed
    /// over; the UI thread does it for real pointer input, and a worker driving
    /// a control from its own logic (a gamepad cursor, a scripted highlight)
    /// does it the same way.
    inline void apply_control_state(ControlId id, bool hovered, bool pressed)
    {
        if (id.has_value()) {
            slint_render_thread_apply_control_state(id.value, hovered, pressed);
        }
    }

    /// Hand the render thread a component to instantiate, which makes the
    /// render thread the owner of the controls from then on.
    ///
    /// `factory` is called on the render thread, so the component it creates
    /// belongs to that thread. It must return the address of a heap-allocated
    /// owning pointer to the component -- the result of `Box::into_raw` on a
    /// boxed component, not the address of the component itself. Once handed
    /// over the render thread owns that allocation and keeps the component alive
    /// for as long as it draws.
    ///
    /// Only available when the C++ API is built against a render-thread capable
    /// backend; returns false otherwise.
    inline bool attach_component(SlintRenderThreadFactory factory)
    {
        return factory != nullptr && slint_render_thread_attach_component(factory);
    }

    /// How the application answers for the properties that belong to the
    /// component the render thread draws.
    ///
    /// The render thread resolves a property name against its own items first
    /// (`enabled` on a `TouchArea`, `text` on a `TextInput`) and asks here for
    /// what is left. Everything a `.slint` file declares around its items -- a
    /// `CheckBox`'s `checked` -- is that.
    ///
    /// Both calls run on the render thread and are given the pointer the
    /// factory returned, so they are expected to answer from the component the
    /// factory created there rather than reaching across threads for it.
    struct PropertyAccess {
        /// Reads a property of `component`. Return false, or leave `out`
        /// alone, when the component declares no such property.
        SlintRenderThreadPropertyRead read = nullptr;
        /// Assigns a property of `component`. Return false when the component
        /// declares no such property or the value does not fit it.
        SlintRenderThreadPropertyWrite write = nullptr;
    };

    /// `attach_component`, with a way for the application to answer for the
    /// properties that belong to the component rather than to any item in it.
    ///
    /// A widget from a `.slint` file is a group of items plus the bindings
    /// between them, so what a caller usually wants to know -- whether a
    /// `CheckBox` is `checked` -- is a property of that group and of no item in
    /// it. The render thread is a windowing backend and does not know what a
    /// `.slint` component is, so `access` is how the application, which does,
    /// lends it that knowledge for the component it just handed over.
    ///
    /// A null `read` leaves every query to the render thread's own items, and a
    /// null `write` refuses every assignment; both are how an application says
    /// "nothing of mine answers this".
    inline bool attach_component(SlintRenderThreadFactory factory, PropertyAccess access)
    {
        return factory != nullptr
                && slint_render_thread_attach_component_with_property_access(
                        factory, SlintRenderThreadPropertyAccess { access.read, access.write });
    }
} // namespace render_thread

} // namespace slint
