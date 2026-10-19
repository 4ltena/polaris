//! Private preparation for the approved MCP protocol migration.
//! Entry stop proof, complete tree inventory and atomic activation remain the
//! migration coordinator's responsibility; these helpers authorize no writes.
pub(crate) mod documents;
pub(crate) mod inventory;
pub(crate) mod paths;

pub(crate) mod mapped;
pub(crate) mod projection;
pub(crate) mod records;

pub(crate) mod namespace;
