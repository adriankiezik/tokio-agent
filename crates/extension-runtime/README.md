# Bundled JavaScript extension runtime

This crate is the trusted QuickJS-NG runtime used by every programmable
extension. Community packages contain JavaScript, not WebAssembly. The host
instantiates this same embedded component in a separate Wasmtime store for each
extension and still enforces fuel, deadlines, memory, payload, and capability
limits.

The checked-in component at
`../extension-host/runtime/js-runtime.component.wasm` imports only WASI clocks
and secure random. Filesystem, environment, standard I/O, processes, and raw
sockets are not available to extension code. Network access is exposed only
through the host-validated `tokio.actions.fetch` API.

## Rebuild the embedded component

Prerequisites are the `wasm32-wasip1` Rust target, `wasm-tools`, `wasi-virt`,
Node.js, and `@bytecodealliance/jco@1.25.2` installed in a temporary tooling
directory. From the repository root:

```sh
rustup target add wasm32-wasip1
cargo build --manifest-path crates/extension-runtime/Cargo.toml \
  --release --target wasm32-wasip1
wasm-tools component new \
  crates/extension-runtime/target/wasm32-wasip1/release/tokio_agent_extension_runtime.wasm \
  --adapt /path/to/jco/lib/wasi_snapshot_preview1.reactor.wasm \
  -o /tmp/tokio-agent-extension-runtime.component.wasm
wasi-virt /tmp/tokio-agent-extension-runtime.component.wasm \
  --allow-clocks --allow-random --stdio=ignore \
  -o crates/extension-host/runtime/js-runtime.component.wasm
```

Verify the result with
`wasm-tools component wit crates/extension-host/runtime/js-runtime.component.wasm`.
The import list must contain only wall clock, monotonic clock, and random.

The companion starts lazily and uses a persistent Wasmtime compilation cache,
so multiple CLI processes do not repeat compilation. Each active CLI currently
has its own companion process; a per-user socket daemon is the remaining step
if its roughly 28 MiB per-process baseline becomes significant in practice.
