You are resolving a git merge conflict. The original task was:

{{original_task_description}}

The merge into {{target_branch}} produced conflicts in:
{{conflicted_paths_list}}

Each file in your worktree currently contains conflict markers
(`<<<<<<<` / `=======` / `>>>>>>>`). For each file, decide what the
correct resolution is given the original task's intent and the changes
from both sides. Then:

1. Edit the file(s) to remove the conflict markers and reflect the
   correct combined behaviour.
2. Run `git status` and confirm no `UU` entries remain.
3. `git add` the resolved files.
4. `git commit -m "Resolve merge conflicts for {{task_id}}"`.
5. Exit cleanly (status 0).

Diff from your branch's side (--- task branch):
{{our_diff}}

Diff from the target's side (--- target branch):
{{their_diff}}

Do NOT push; the server will re-attempt the merge after you exit.
