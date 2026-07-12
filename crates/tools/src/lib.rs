mod bash;
mod edit;
mod fs;
mod glob;
mod grep;
mod multi_edit;
mod read;
mod web_search;
mod write;

use std::sync::Arc;

use tokio_agent_core::Tool;

pub use bash::{Bash, BashConfig, BashKill, BashWait};
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use multi_edit::MultiEdit;
pub use read::Read;
pub use web_search::WebSearch;
pub use write::Write;

#[must_use]
pub fn builtins() -> Vec<Arc<dyn Tool>> {
    builtins_with_bash_config(BashConfig::default())
}

#[must_use]
pub fn builtins_with_bash_config(config: BashConfig) -> Vec<Arc<dyn Tool>> {
    let [bash, bash_wait, bash_kill] = bash::tools(config);
    vec![
        Arc::new(Read),
        Arc::new(Write),
        Arc::new(Edit),
        Arc::new(MultiEdit),
        bash,
        bash_wait,
        bash_kill,
        Arc::new(Glob),
        Arc::new(Grep),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_core_builtins_are_registered() {
        let names: Vec<_> = builtins()
            .into_iter()
            .map(|tool| tool.schema().name)
            .collect();
        assert_eq!(
            names,
            [
                "read",
                "write",
                "edit",
                "multi_edit",
                "bash",
                "bash_wait",
                "bash_kill",
                "glob",
                "grep"
            ]
        );
    }
}
