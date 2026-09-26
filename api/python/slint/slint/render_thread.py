# Copyright © SixtyFPS GmbH <info@slint.dev>
# SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

r"""
Entry points that address the render thread of the 2-thread render separation.

The render thread owns the mirror component tree and presents frames. The UI
thread and worker threads are equal peers of it: both reach it through this
module instead of going through a window adapter.
"""

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
