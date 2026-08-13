//! Windows filesystem mechanics (concrete, selected by the crate-root
//! selector).

pub mod durability;
pub mod identity;
pub mod links;
pub mod path;
pub mod permissions;
pub mod replace;
pub mod volume;

pub(crate) mod verbatim;

pub(crate) use verbatim::verbatim_path;
