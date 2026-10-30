// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

/// Location information attached to a log message.
#[derive(Clone, Debug)]
pub struct LogMessageLocation<'a> {
    /// The file path of the source that emitted the log message
    pub path: &'a str,
    /// the line number of the call that emitted the log message (1-based)
    pub line: usize,
    /// The column of the call that emitted the log message (1-based)
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LogMessageSource {
    /// The message originates from Slint source code (e.g. the `debug()` function in Slint)
    SlintCode,
    /// The message originates from the Slint runtime crates
    ///
    /// For example if a file path could not be resolved at runtime.
    Runtime,
}

/// Opaque log message, emitted by [`crate::debug_log!`] as well as the Slint `debug()` function.
pub struct LogMessage<'a> {
    source: LogMessageSource,
    location: Option<LogMessageLocation<'a>>,
    arguments: core::fmt::Arguments<'a>,
}

impl<'a> LogMessage<'a> {
    pub fn new(
        source: LogMessageSource,
        location: Option<LogMessageLocation<'a>>,
        arguments: core::fmt::Arguments<'a>,
    ) -> Self {
        Self { source, location, arguments }
    }

    pub fn source(&self) -> LogMessageSource {
        self.source
    }

    pub fn location(&self) -> Option<LogMessageLocation<'_>> {
        self.location.clone()
    }

    pub fn message_arguments(&self) -> core::fmt::Arguments<'a> {
        self.arguments
    }
}

/// Log message handler type stored in a [`crate::SlintContext`].
pub type LogMessageHandler = alloc::boxed::Box<dyn for<'a> Fn(LogMessage<'a>) + 'static>;

#[doc(hidden)]
pub fn log_message(message: LogMessage) {
    crate::context::GLOBAL_CONTEXT.with(|p| match p.get() {
        Some(ctx) => ctx.dispatch_log_message(message),
        None => default_log_message(message.arguments),
    });
}

/// The diagnostics the generated carry and route code prints when the
/// `SLINT_LOG_REPEATER` environment variable is set.
///
/// The generated code runs against trees whose crate may be `no_std`, where
/// neither the environment nor a standard error stream exist, so the check
/// and the write live here, where `std` is a feature rather than a given.
/// Without `std` the diagnostics are simply not printed.
#[doc(hidden)]
pub fn log_diagnostics(arguments: core::fmt::Arguments) {
    #[cfg(feature = "std")]
    {
        if std::env::var_os("SLINT_LOG_REPEATER").is_some() {
            // A failed write is dropped: the diagnostics must not take the
            // thread that carries a value down with them.
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "{arguments}");
        }
    }
    #[cfg(not(feature = "std"))]
    let _ = arguments;
}

#[doc(hidden)]
pub fn default_log_message(_arguments: core::fmt::Arguments) {
    cfg_if::cfg_if! {
        if #[cfg(feature = "log")] {
            log::debug!("{_arguments}");
        } else if #[cfg(target_arch = "wasm32")] {
            use wasm_bindgen::prelude::*;
            use std::string::ToString;

            #[wasm_bindgen]
            extern "C" {
                #[wasm_bindgen(js_namespace = console)]
                pub fn log(s: &str);
            }

            log(&_arguments.to_string());
        } else if #[cfg(feature = "std")] {
            use std::io::Write;
            // We were seeing intermittent, albeit very rare, crashes due to `eprintln` panicking
            // if the write to stderr fails. Since this is just for debug printing, it's safe
            // to silently drop this if we can't write (since it wouldn't be written to stderr
            // anyway)
            let _ = writeln!(std::io::stderr(), "{_arguments}");
        }
    }
}

#[macro_export]
/// This macro allows producing debug output that will appear on stderr in regular builds
/// and in the console log for wasm builds.
macro_rules! debug_log {
    ($($t:tt)*) => ($crate::debug_log::log_message(
        $crate::debug_log::LogMessage::new(
            $crate::debug_log::LogMessageSource::Runtime,
            None,
            format_args!($($t)*),
        )
    ))
}
