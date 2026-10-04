# Copyright © SixtyFPS GmbH <info@slint.dev>
# SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

r"""
Entry points that address the render thread of the 2-thread render separation.

The render thread owns the mirror component tree and presents frames. The UI
thread and worker threads are equal peers of it: both reach it through this
module instead of going through a window adapter.

The render thread owns the controls, so a program that wants to touch one
borrows it: identify the control, then read or assign a property. Neither the UI
thread nor a worker is more privileged here, and neither keeps its own copy of
the ``.slint`` tree: a question about a control is answered by the render thread,
which is the side that has the component.
"""

from typing import Literal, Optional

from ._native import native


def request_redraw() -> None:
    """Requests a window repaint through the render thread.

    The render thread owns the mirror component and re-encodes and re-presents
    the frame when asked, so this addresses the presentation authority directly.
    Call it from the UI thread or from any worker thread; both are equal peers
    of the render thread. It is a no-op when the render thread was never
    started.
    """
    native.request_redraw()


def control_at(x: float, y: float) -> Optional[int]:
    """Returns the control under the logical point ``x``, ``y``, or ``None``.

    Reads the geometry the render thread last composited and does not wait for
    it, so it is cheap enough for a pointer move. Use :func:`hit_test` when the
    answer must reflect the present rather than the last frame.
    """
    return native.control_at(x, y)


def hit_test(x: float, y: float) -> Optional[int]:
    """Returns the control under ``x``, ``y``, resolved on the render thread.

    Unlike :func:`control_at`, this blocks until the render thread answers, which
    is what makes it right for a click and wrong for a pointer move.
    """
    return native.hit_test(x, y)


def get_control_property(id: int, property: str):
    """Reads one property of a borrowed control, blocking until the render thread answers.

    Returns the value in the shape :func:`set_control_property` takes, so a value
    read can be handed straight back to the setter, or ``None`` when the id or
    the property name does not resolve.

    The ``.slint`` side of the tree belongs to the render thread, so a program
    that needs to know what a control looks like asks here instead of keeping a
    second copy of the component.
    """
    return native.get_control_property(id, property)


def set_control_property(
    id: int,
    property: str,
    value_type: Literal["bool", "number", "text", "color"],
    value,
) -> bool:
    """Assigns one property of a borrowed control, blocking until confirmed.

    ``value`` has to match ``value_type``: a ``bool``, a number, a ``str``, and a
    :class:`slint.Color` respectively. A color is therefore written the way CSS
    spells it, as ``slint.Color("rebeccapurple")``.

    Returns whether the property name resolved and the value was applied. A
    property that had a binding is detached first, the same way assigning
    through the normal API behaves. A ``value_type`` that names no known type
    returns ``False`` rather than guessing which field was meant, while a
    ``value`` that does not match its ``value_type`` raises, because that points
    straight at the caller's mistake, which ``False`` would hide.
    """
    return native.set_control_property(id, property, value_type, value)


def apply_control_state(id: int, hovered: bool, pressed: bool = False) -> None:
    """Reports a borrowed control as hovered and/or pressed on the render thread.

    This is how a pointer state the program resolved itself is handed over: the
    UI thread does it for real pointer input, and a worker driving a control from
    its own logic does it the same way.
    """
    native.apply_control_state(id, hovered, pressed)
