//! Shared prompt fragments injected into every agent we spawn.
//!
//! Auto-mode runs unattended, so every agent (task, fix, future planning /
//! check helpers, …) must follow the same contract: commit work to its
//! branch, never push, never pause to ask. New agent-spawn helpers should
//! call [`unattended_contract_block`] and embed its output in their prompt.

/// The unattended-execution contract block, formatted for inclusion in any
/// agent prompt. `task_branch` is interpolated so the model can't confuse it
/// with the project's default branch.
///
/// Keep this block short (≤6 lines) and imperative. Auto-mode pauses if an
/// agent exits clean with no commits ahead of trunk, and emits a diagnostic
/// log line so violations are visible without diff inspection.
pub fn unattended_contract_block(task_branch: &str) -> String {
    format!(
        "Unattended-execution contract (this agent runs without a human at the keyboard):\n\
         - Commit all changes to the current branch (`{task_branch}`) before exiting. Use `git add -A && git commit -m '<short summary>'`.\n\
         - Do not run `git push`. Branchwork pushes on merge; pushing the task branch would race with the auto-mode loop.\n\
         - Do not ask the user for confirmation to commit, push, or merge — act. If you are genuinely stuck, say so plainly and exit non-zero so the loop can pause.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_includes_three_rules_and_branch_name() {
        let block = unattended_contract_block("branchwork/myplan/1.2");
        assert!(
            block.contains("branchwork/myplan/1.2"),
            "branch name must be interpolated literally: {block}"
        );
        assert!(
            block.contains("Commit all changes"),
            "rule 1 (commit) missing: {block}"
        );
        assert!(
            block.contains("Do not run `git push`"),
            "rule 2 (no push) missing: {block}"
        );
        assert!(
            block.contains("Do not ask the user"),
            "rule 3 (no ask) missing: {block}"
        );
    }

    #[test]
    fn block_is_short() {
        let block = unattended_contract_block("branchwork/p/1");
        let line_count = block.lines().count();
        assert!(
            line_count <= 6,
            "block grew past the 6-line cap: {line_count} lines\n{block}"
        );
    }
}
