# Testing extensions locally

Install the build tools once:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-tools --locked
```

From the repository root, build and project-link an extension:

```sh
cargo run -p tokio-agent -- extension build registry/extensions/loop
cargo run -p tokio-agent -- extension link registry/extensions/loop \
  --project --approve --dev-override
```

Use `registry/extensions/goal` to test Goal or `registry/extensions/permissions` to test the optional Permissions tool gate. `--dev-override` is a safety acknowledgement required only for IDs in the reserved `tokio.*` namespace; ordinary local extension IDs do not need it. The project link takes precedence over the published package and appears as a local override in the installed extensions menu.

The running session reloads programmable contributions at the next admission boundary; manifest-declared CLI options are available on the next process invocation. To return to the published version:

```sh
cargo run -p tokio-agent -- extension unlink tokio.loop --project
```
