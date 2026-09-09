//! MCP (Model Context Protocol) client for Comrade.
//!
//! Connects to external MCP servers — spawned stdio child processes or remote
//! streamable-HTTP endpoints — and exposes the tools they advertise to the
//! agent as [`comrade_tool::Tool`] adapters.
//!
//! Auth for HTTP servers: static API keys (bearer/`x-api-key`) and OIDC via
//! OAuth 2.0 authorization-code + PKCE with loopback redirect.

pub mod auth;
pub mod connect;
pub mod tool;

pub use connect::{connect_all, connect_one};
pub use tool::{mcp_server_prefix, mcp_tool_name};
