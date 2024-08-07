pub mod paths;

use crate::{
    http::RequestError,
    template::ChainError,
    tui::message::{Message, MessageSender},
};
use chrono::{
    format::{DelayedFormat, StrftimeItems},
    DateTime, Duration, Local, Utc,
};
use derive_more::{DerefMut, Display};
use dialoguer::console::Style;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;
use std::{
    fmt::{self, Debug, Formatter},
    ops::Deref,
};
use tracing::error;

const WEBSITE: &str = "https://slumber.lucaspickering.me";
pub const NEW_ISSUE_LINK: &str =
    "https://github.com/LucasPickering/slumber/issues/new/choose";

/// Get a link to a page on the doc website. This will append the doc prefix,
/// as well as the suffix.
///
/// ```
/// assert_eq!(
///     doc_link("api/chain"),
///     "https://slumber.lucaspickering.me/book/api/chain.html",
/// );
/// ```
pub fn doc_link(path: &str) -> String {
    format!("{WEBSITE}/book/{path}.html")
}

/// Parse bytes (probably from a file) into YAML. This will merge any
/// anchors/aliases.
pub fn parse_yaml<T: DeserializeOwned>(bytes: &[u8]) -> serde_yaml::Result<T> {
    // Two-step parsing is required for anchor/alias merging
    let mut yaml_value = serde_yaml::from_slice::<serde_yaml::Value>(bytes)?;
    yaml_value.apply_merge()?;
    serde_yaml::from_value(yaml_value)
}

/// Format a datetime for the user
pub fn format_time(time: &DateTime<Utc>) -> DelayedFormat<StrftimeItems> {
    time.with_timezone(&Local).format("%b %-d %H:%M:%S")
}

/// Format a duration for the user
pub fn format_duration(duration: &Duration) -> String {
    let ms = duration.num_milliseconds();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}

/// A value that can be replaced in-place. This is useful for two purposes:
/// - Transferring ownership of values from old to new
/// - Dropping the old value before creating the new one
/// This struct has one invariant: The value is always defined, *except* while
/// the replacement closure is executing. Better make sure that guy doesn't
/// panic!
#[derive(Debug)]
pub struct Replaceable<T>(Option<T>);

impl<T> Replaceable<T> {
    pub fn new(value: T) -> Self {
        Self(Some(value))
    }

    /// Replace the old value with the new one. The function that generates the
    /// new value consumes the old one.
    ///
    /// The only time this value will panic on access is while the passed
    /// closure is executing (or during unwind if it panicked).
    pub fn replace(&mut self, f: impl FnOnce(T) -> T) {
        let old = self.0.take().expect("Replaceable value not present!");
        self.0 = Some(f(old));
    }
}

/// Access the inner value. If mid-replacement, this will panic
impl<T> Deref for Replaceable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("Replacement in progress or failed")
    }
}

/// Access the inner value. If mid-replacement, this will panic
impl<T> DerefMut for Replaceable<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("Replacement in progress or failed")
    }
}

pub trait ResultExt<T, E>: Sized {
    /// If this is an error, trace it. Return the same result.
    fn traced(self) -> Self;

    /// If this result is an error, send it over the message channel to be
    /// shown the user, and return `None`. If it's `Ok`, return `Some`.
    fn reported(self, messages_tx: &MessageSender) -> Option<T>;
}

// This is deliberately *not* implemented for non-anyhow errors, because we only
// want to trace errors that have full context attached
impl<T> ResultExt<T, anyhow::Error> for anyhow::Result<T> {
    fn traced(self) -> Self {
        if let Err(err) = &self {
            error!(error = err.deref());
        }
        self
    }

    fn reported(self, messages_tx: &MessageSender) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                messages_tx.send(Message::Error { error });
                None
            }
        }
    }
}

impl<T> ResultExt<T, RequestError> for Result<T, RequestError> {
    fn traced(self) -> Self {
        if let Err(err) = &self {
            error!(error = %err);
        }
        self
    }

    fn reported(self, messages_tx: &MessageSender) -> Option<T> {
        self.map_err(anyhow::Error::from).reported(messages_tx)
    }
}

impl<T> ResultExt<T, ChainError> for Result<T, ChainError> {
    fn traced(self) -> Self {
        if let Err(err) = &self {
            error!(error = %err);
        }
        self
    }

    fn reported(self, messages_tx: &MessageSender) -> Option<T> {
        self.map_err(anyhow::Error::from).reported(messages_tx)
    }
}

/// Helper to printing bytes. If the bytes aren't valid UTF-8, they'll be
/// printed in hex representation instead
pub struct MaybeStr<'a>(pub &'a [u8]);

impl<'a> Display for MaybeStr<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Ok(s) = std::str::from_utf8(self.0) {
            write!(f, "{s}")
        } else {
            let bytes_per_line = 12;
            // Format raw bytes in pairs of bytes
            for (i, byte) in self.0.iter().enumerate() {
                if i > 0 {
                    // Add whitespace before this group. Only use line breaks
                    // in alternate mode
                    if f.alternate() && i % bytes_per_line == 0 {
                        writeln!(f)?;
                    } else {
                        write!(f, " ")?;
                    }
                }

                write!(f, "{byte:02x}")?;
            }
            Ok(())
        }
    }
}

/// Wrapper making it easy to print a header map
pub struct HeaderDisplay<'a>(pub &'a HeaderMap);

impl<'a> Display for HeaderDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let key_style = Style::new().bold();
        for (key, value) in self.0 {
            writeln!(
                f,
                "{}: {}",
                key_style.apply_to(key),
                MaybeStr(value.as_bytes()),
            )?;
        }
        Ok(())
    }
}

/// A static mapping between values (of type `T`) and labels (strings). Used to
/// both stringify from and parse to `T`.
pub struct Mapping<'a, T: Copy>(&'a [(T, &'a [&'a str])]);

impl<'a, T: Copy> Mapping<'a, T> {
    /// Construct a new mapping
    pub const fn new(mapping: &'a [(T, &'a [&'a str])]) -> Self {
        Self(mapping)
    }

    /// Get a value by one of its labels
    pub fn get(&self, s: &str) -> Option<T> {
        for (value, strs) in self.0 {
            for other_string in *strs {
                if *other_string == s {
                    return Some(*value);
                }
            }
        }
        None
    }

    /// Get the label mapped to a value. If it has multiple labels, use the
    /// first. Panic if the value has no mapped labels
    pub fn get_label(&self, value: T) -> &str
    where
        T: Debug + PartialEq,
    {
        let (_, strings) = self
            .0
            .iter()
            .find(|(v, _)| v == &value)
            .unwrap_or_else(|| panic!("Unknown value {value:?}"));
        strings
            .first()
            .unwrap_or_else(|| panic!("No mapped strings for value {value:?}"))
    }

    /// Get all available mapped strings
    pub fn all_strings(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .flat_map(|(_, strings)| strings.iter().copied())
    }
}
