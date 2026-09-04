// Included verbatim by both src/main.rs and build.rs (which cannot `use` items
// from the crate it builds), so the exe's FileDescription and the title
// reported over MCP stay in step. Not a module -- see the include! calls.

/// Human-readable product name, as distinct from the `tui_mcp` identifier.
const DISPLAY_NAME: &str = "TUI MCP";
