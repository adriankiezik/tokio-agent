# tokio-agent

A fast, provider-agnostic terminal coding agent.

## Optional tool approvals

Tool calls run immediately by default. Approval modes and `/permissions` are provided by the optional official `tokio.permissions` extension, which is published but is not installed automatically. Disabling or uninstalling it returns the agent to unconditional tool execution.

This is a breaking security-default change for upgrades from releases with built-in permissions: remove the obsolete `permission_mode` base-config key (it is rejected as unknown), and install `tokio.permissions` if you want to retain approval prompts. The extension provides `--permission-mode`; the former `--yolo` option has been removed.

## Local extension development

See [`registry/extensions/LOCAL_DEV.md`](registry/extensions/LOCAL_DEV.md) for the local build and override workflow.

## License

[MIT](LICENSE)
