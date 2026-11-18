//! Agent Skills discovery and reading. Skills do not grant permissions.
mod catalog;
mod metadata;
mod resources;
mod tools;
pub use catalog::SkillCatalog;
pub use metadata::SkillMetadata;
pub use resources::{ResourceEntry, ResourceKind, SkillAsset, SkillResource};
pub use tools::register;
#[cfg(test)]
mod tests;
