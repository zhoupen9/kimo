# Sender groups — the smallest reference extension

This module returns a fixed `organizer.v1` plan for the message id `m1`. It
exists to show the declarative response shape and the extension manifest; it
is not a useful organiser and does not touch the network, the store or egui.

Build it from this directory:

```sh
cargo build --release --target wasm32-unknown-unknown
```

Then install it under the plugin directory:

```sh
mkdir -p ~/.local/share/kimo/plugins/sender-groups
cp plugin.json ~/.local/share/kimo/plugins/sender-groups/
cp ../target/wasm32-unknown-unknown/release/kimo_plugin_sender_groups.wasm \
   ~/.local/share/kimo/plugins/sender-groups/sender-groups.wasm
```
