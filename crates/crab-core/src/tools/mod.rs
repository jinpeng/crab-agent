pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod web_fetch;
pub mod write;

use crate::gate::PermissionGate;
use crate::state::SessionState;
use crate::tool::ToolRegistry;

/// Assemble all v0 tools. They share one SessionState and one PermissionGate.
pub fn register_all(state: SessionState, gate: PermissionGate) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(read::ReadTool::new(state.clone())));
    reg.register(Box::new(write::WriteTool::new(state.clone(), gate.clone())));
    reg.register(Box::new(edit::EditTool::new(state.clone(), gate.clone())));
    reg.register(Box::new(bash::BashTool::new(gate.clone())));
    reg.register(Box::new(glob::GlobTool::new()));
    reg.register(Box::new(grep::GrepTool::new()));
    reg.register(Box::new(web_fetch::WebFetchTool::new(gate.clone())));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_all_registers_all_tools() {
        let reg = register_all(
            crate::state::SessionState::default(),
            crate::gate::PermissionGate::auto_approve(),
        );
        for name in ["read", "write", "edit", "bash", "glob", "grep", "web_fetch"] {
            assert!(reg.contains(name), "missing tool {name}");
        }
    }
}
