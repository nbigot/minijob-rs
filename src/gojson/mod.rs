//! Compatibility layer with Go's `encoding/json` and `fmt`.
//!
//! The Go server writes every API response, the in-memory storage file, the Redis values and
//! the callback payloads with `encoding/json`. Clients and existing data depend on its exact
//! output, so this module reproduces it:
//!
//! - [`to_vec`]: serializes like `json.Marshal` (HTML-safe escaping, Go float format, sorted
//!   map keys when the map type is ordered);
//! - [`decode`]: decodes objects like `json.Unmarshal` into structs (case-insensitive field
//!   names, `null` leaves the zero value, last duplicate wins);
//! - [`fmt_v`]: formats a decoded value like Go's `fmt.Sprintf("%v", v)`.

pub mod decode;
mod encode;
mod fmt_v;
mod quote;

pub use encode::{
    GoFormatter, GoFormatterNoHtml, format_float, to_vec, to_vec_no_html, write_string,
};
pub(crate) use encode::{push_plain, shortest_digits};
pub use fmt_v::{fmt_float_v, fmt_v};
pub use quote::{quote, quote_bytes, quote_rune};
