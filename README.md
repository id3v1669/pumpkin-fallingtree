# pumpkin-fallingtree

FallingTree-style tree felling for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin)
Minecraft server, written in Rust as a WebAssembly plugin against plugin **ABI v0.2**.

## Behaviour

- **Sneak** and break a natural log with an **axe** to fell the whole tree.
  Without sneaking, a log breaks normally.
- Connected logs of the same species at or above the cut are broken, diagonals
  included, up to 256 logs. Supported: `acacia`, `birch`, `cherry`, `dark_oak`, `jungle`,
  `mangrove`, `oak`, `pale_oak`, `poplar`, `spruce` (`_log` / `_wood`) and the huge fungi
  `crimson`, `warped` (`_stem` / `_hyphae`). Stripped logs and crop or mushroom stems are ignored.
- A tree is only felled if the right natural canopy touches it: non-persistent leaves
  for trees, wart blocks / shroomlight for huge fungi. Log cabins are left alone.
- The axe loses 1 durability per extra log (vanilla Unbreaking odds), charged before
  anything is broken. If that breaks the axe, only the logs it paid for are felled.
  Creative mode costs no durability and drops nothing.
- Every block is rechecked right before it is broken, and felling only happens in the
  world where the log was cut.
- The tree's natural leaves are broken right away (about 12 ticks after the felling,
  once Pumpkin has propagated their `distance`). Only leaves that really lost their
  tree (distance 7, not player-placed) are removed, so a neighbouring tree keeps its canopy.

The felling runs one tick after the player's break. That way it never runs when
another plugin cancelled the break, and it doesn't re-enter the event that started it.

## Requirements

ABI v0.2 is still behind a feature flag in Pumpkin: the server must be built with

```sh
cargo build --release --features plugin-v0_2
```

A stock server rejects the plugin with an "unsupported API version" error.
Because v0.2 is unfinished, a Pumpkin update can break this plugin until `wit/` is
refreshed and the plugin rebuilt.

`wit/` is a copy of `crates/pumpkin-plugin-wit/v0.2` from Pumpkin commit
`10f1fde9367c827f168eaabdc8f343b1ce6f676c` (MIT / Apache-2.0, licences included).

## Build

```sh
nix build            # -> result/lib/fallingtree.wasm
nix flake check      # unit tests
```

or in the dev shell (`nix develop`):

```sh
just build   # target/wasm32-wasip2/release/pumpkin_fallingtree.wasm
just test
just lint
```

Copy the `.wasm` into the server's `plugins/` directory.

## Known limits

- Drops are rolled without a tool (`world.break-block` has no tool argument), so
  Silk Touch / Fortune / shears don't affect leaf drops. Logs always drop themselves.
- Plugin-driven breaks reach other plugins as `block-break-event` without a player,
  so land-claim plugins can't attribute the extra logs and leaves to anyone.
- No config file; the limits are constants in `src/tree.rs` and `src/plugin.rs`.
