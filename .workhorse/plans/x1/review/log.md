# Review loop — X1

Rigour: standard. Cap: 5 rounds. Stop rule: two clean rounds in a row, the
second by fresh eyes, with no criticals and at most a few suggestions.

## Setup

- Base: `572c664` (merge-base with `main`). 80 commits, 195 files.
- Scope: `src/` and `tests/`, snapshots excluded — 36,259 diff lines.
  Snapshots (81 files), design docs and mockups are in the diff but are not
  what the reviewers look at.
- The branch changes **no** agent-instruction file: no `CLAUDE.md`,
  `AGENTS.md`, `.cursorrules`, `.claude/` or `.mcp.json` in the diff.
  The worktree has no `.mcp.json` and no `.claude/`, so no project MCP server
  was started for the reviewers. `AGENTS.md` and `CLAUDE.md` are base content.
- The repository defines no Review Hero configuration, no custom reviewer
  prompts and no `suppressions.yml`, so no finding is suppressed and the
  custom-reviewer slot is filled by the repo's own AI rules file (`AGENTS.md`)
  and by its normative specs.
- Containment: this harness has no subagent type that cannot run commands.
  `Explore` is the most restricted (no Edit/Write/NotebookEdit) but still has
  Bash. Reviewers run as `Explore` and the report says so.

## Rounds

