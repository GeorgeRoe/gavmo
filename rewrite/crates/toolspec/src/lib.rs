//! The per-repo `gavmo.yaml`: parsing the untrusted tool config into types
//! that are valid by construction ([`parse_config`]), and the host-lock that
//! keeps each tool's credential on the destination it was approved for
//! ([`BaseUrl::lock`]).

mod base_url;
mod header_name;
mod parse;
mod schema;
mod secret_name;
mod tool_id;

pub use base_url::{BaseUrl, BaseUrlError, HostLockError};
pub use header_name::HeaderName;
pub use parse::{ParseError, parse_config};
pub use schema::{Auth, GavmoConfig, ToolDef, ToolKind};
pub use secret_name::SecretName;
pub use tool_id::ToolId;
