# Echo — the smallest reference plugin

This is a source sample, not a provider that talks to a mail server. It shows
the exported ABI and the manifest shape a real module starts from.

Build it from this directory:

```sh
cargo build --release --target wasm32-unknown-unknown
```

Then copy the module and manifest into a plugin directory:

```sh
mkdir -p ~/.local/share/kimo/plugins/echo
cp plugin.json ~/.local/share/kimo/plugins/echo/
cp target/wasm32-unknown-unknown/release/kimo_plugin_echo.wasm \
   ~/.local/share/kimo/plugins/echo/echo.wasm
```

Kimo compiles and instantiates it at startup and lists it in
Preferences → Plugins. It has no permissions, so it can only return the fixed
describe response; it cannot reach the network, the database or a mailbox.
