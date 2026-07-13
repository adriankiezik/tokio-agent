# Tokio

A fast, provider-agnostic terminal coding agent.

## Local extension development

See [`registry/extensions/LOCAL_DEV.md`](registry/extensions/LOCAL_DEV.md) for the local build and override workflow.

Programmable extensions are authored in TypeScript and installed as pre-bundled JavaScript. They run in the shared QuickJS WebAssembly sandbox and can only affect the agent through capability-checked Tokio APIs.

## License

[MIT](LICENSE)
