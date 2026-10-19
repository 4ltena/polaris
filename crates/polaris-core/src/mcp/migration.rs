//! Core-private MCP migration authority. Read-only MCP observations never grant
//! entry exclusion, stopping, intent writes or activation.
mod account_home;
mod coordinator;
mod entry_gate;
mod preflight;
mod read_admission;
#[cfg(test)]
mod test_fixture;

use polaris_mcp::migration_observation as records;
