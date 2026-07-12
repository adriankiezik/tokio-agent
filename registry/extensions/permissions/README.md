# tokio.permissions

Official, optional tool-approval policy for tokio-agent. It is not installed by default.

Install it with the normal extension manager, then use `/permissions` to select:

- **suggest**: reads run; edits, commands, and unknown effects ask.
- **auto-edit**: reads and edits run; commands and unknown effects ask.
- **full-auto**: every tool runs without asking.

For unattended runs use `--permission-mode full-auto`; prompting modes deny rather than hanging when no interactive frontend is available. The CLI option exists only while this extension is enabled and applies only to that process.

“Allow for session” stores only a hash of a narrow operation scope. Bash approvals match the exact command and working directory, and edit approvals match their target path. They reset with the session.

Removing or disabling this extension means all valid tool calls run automatically. A crash or protocol failure while it is installed fails closed for the rest of the session instead of silently disabling protection.
