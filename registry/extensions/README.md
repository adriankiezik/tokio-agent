# TypeScript extension API

Run `tokio-agent extension new my-extension` to create a
typed extension. `npm install && npm run build` produces the single JavaScript
entrypoint shipped in the package; users do not need Rust, Cargo, or a local
WebAssembly toolchain.

The shared declaration file, `tokio-extension.d.ts`, documents the available
callbacks and action constructors. Extensions cannot directly access files,
processes, environment variables, standard I/O, or sockets. Every effect is an
action checked against capabilities from `extension.toml`.

For example, an extension with `network_request = true` and
`storage_user = true` can fetch public HTTPS data and cache it without receiving
raw network or filesystem access:

```ts
let cached: Json = null;

tokio.defineExtension({
  loadState({ userState }) {
    cached = tokio.storage.decodeJson(userState, null);
  },
  onCommand() {
    return [tokio.actions.fetch("catalog", "https://example.com/catalog.json")];
  },
  onEvent(event) {
    if (event.type !== "network_response" || event.value.id !== "catalog") return [];
    if (event.value.error) return [tokio.actions.notice("error", event.value.error)];
    cached = JSON.parse(event.value.body ?? "null") as Json;
    return [tokio.actions.persistUserJson(cached)];
  },
});
```

Network requests are GET-only, limited to 30 per minute and 256 KiB per UTF-8
response, and may target any public HTTPS origin. Redirects are checked again;
localhost, private/link-local networks, metadata services, URL credentials,
ambient proxies, and cookies are blocked.
