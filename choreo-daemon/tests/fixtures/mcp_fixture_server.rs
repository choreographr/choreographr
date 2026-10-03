// The daemon's MCP integration tests need the same scripted stdio server the
// `choreo-mcp` suite uses, but `CARGO_BIN_EXE_<name>` is only set for the
// package that owns the binary. Rather than duplicate the server, this bin
// includes the single fixture source from `choreo-mcp` (a textual `include!`,
// so both crates compile their own copy without a shared crate).
include!("../../../choreo-mcp/tests/fixtures/fixture_server.rs");
