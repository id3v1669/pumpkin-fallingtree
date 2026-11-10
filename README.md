# pumpkin-fallingtree

FallingTree-style tree felling for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin)
Minecraft server, written in Rust as a WebAssembly plugin against plugin **ABI v0.2**.

## Behaviour

- **Sneak** and break a natural log with an **axe** to fell the whole tree.
  Without sneaking, a log breaks normally.
- Connected logs of the same species at or above the cut are broken, diagonals
  included. Supported: `acacia`, `birch`, `cherry`, `dark_oak`, `jungle`, `mangrove`,
  `oak`, `pale_oak`, `poplar`, `spruce` (`_log` / `_wood`) and the huge fungi `crimson`,
  `warped` (`_stem` / `_hyphae`). Stripped logs, planks and crop or mushroom stems are
  never part of a tree.
- Build protection, modelled on FallingTree's defaults:
  - A group of more than 120 connected logs is not felled at all.
  - Natural canopy must touch a top-most log: non-persistent leaves for trees, the
    species' own wart block for huge fungi (shroomlight alone doesn't count). A wall or
    beam that only brushes a tree's leaves lower down is left alone.
- Huge fungi: their wart blocks and shroomlight (which never decay) are broken with the
  stems: those connected to the stems, within 4 blocks horizontally of the cut and at
  most 1 block above the top stem. This only happens once every stem is down; if any
  stem is left (break rejected, or the axe could not pay for it), the canopy stays.
- The axe loses 1 durability per extra log that actually breaks (vanilla Unbreaking odds).
  The cost is reserved before felling, so moving the axe mid-felling can't dodge it: if
  the tree uses the axe up, the axe is taken away before felling starts. Logs that don't
  break (for example, rejected by a protection plugin) are refunded, and a taken axe is
  given back if it was not used up after all. If the axe can't pay for the whole tree,
  only the logs it can pay for are felled.
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
