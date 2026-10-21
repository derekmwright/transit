# Sidechain

Design: [ADR 0015](../../adr/0015-io-routing-abstraction.md), section 4.

- An insert has an optional key: `Insert.key` in `crates/model/src/mixer.rs`, with `#[serde(default)]`
  so old sessions load. The key names a source (track, bus or send) and a tap (pre or post fader).
- The key is an edge in the route graph (`crates/model/src/route_graph.rs`). A strip with a keyed
  insert renders in a later layer than its key source, so "kick ducks bass" works. A key cycle is
  refused by the same L22 rule as any loop.
- The mix writes the key tap into a preallocated per-strip buffer (`crates/mix/src/key.rs`) and
  passes it to the plug-in through `process_with_key`. Built-in compressor and gate read it.
- Hosts activate the plug-in's sidechain input (CLAP port, VST3 aux bus) at creation and feed
  silence when unrouted, so key edits never touch a live instance.
- **Alignment:** a key that arrives late delays the keyed strip's input; an early key delays only the
  key. Both are counted in PDC. See `key.rs` and `crates/mix/tests/path_pdc.rs`.
- A post-fader tap follows the fader, so muting the source stops the ducking; use a pre-fader tap
  (known issue K8, `../../known-issues/README.md`).

Tests: `crates/mix/tests/sidechain.rs`, `sidechain_hosted.rs`.
