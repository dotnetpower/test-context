pub mod ast_graph;
pub mod cache;
pub mod catalog;
pub mod daemon;
pub mod diagnostics;
pub mod exclusion_policy;
pub mod mcp;
pub mod policy;
pub mod registration;
pub mod runner;
pub mod shared_mcp;
pub mod workspace;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}
