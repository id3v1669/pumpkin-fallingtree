//! FallingTree-style tree felling for the Pumpkin Minecraft server (plugin ABI v0.2).

pub mod tree;

#[cfg(target_family = "wasm")]
mod plugin;
