//! Host-independent tree logic: block classification, tree search and durability maths.
//!
//! Everything here talks to the world through [`BlockView`], so it can be unit-tested
//! without a server.

use std::collections::{HashSet, VecDeque};

/// Upper bound on logs felled in one go, to keep giant builds and mega trees bounded.
pub const MAX_LOGS: usize = 256;
/// Upper bound on leaves considered for instant removal.
pub const MAX_LEAVES: usize = 2048;
/// Vanilla leaves decay at distance 7, so no leaf further than 6 steps from a log
/// can belong to the tree.
pub const LEAF_REACH: u8 = 6;
/// Vanilla `distance` value of leaves that are cut off from every log.
pub const DECAYED_DISTANCE: u8 = 7;

pub type Pos = (i32, i32, i32);

/// What the tree logic needs to know about one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A natural log of the given species (`oak`, `spruce`, `crimson`, ...).
    Log(String),
    /// Leaves. `persistent` is true for player-placed leaves.
    Leaves {
        persistent: bool,
        distance: u8,
    },
    /// Non-decaying canopy of huge fungi (wart blocks, shroomlight).
    FungusCanopy,
    Other,
}

pub trait BlockView {
    fn kind(&mut self, pos: Pos) -> Kind;
}

/// Classifies a block from its id (with or without the `minecraft:` namespace) and properties.
pub fn classify(name: &str, properties: &[(String, String)]) -> Kind {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    if let Some(species) = log_species(name) {
        return Kind::Log(species.to_owned());
    }
    if name.ends_with("_leaves") {
        let prop = |key: &str| {
            properties
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        return Kind::Leaves {
            persistent: prop("persistent") == Some("true"),
            distance: prop("distance")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DECAYED_DISTANCE),
        };
    }
    match name {
        "nether_wart_block" | "warped_wart_block" | "shroomlight" => Kind::FungusCanopy,
        _ => Kind::Other,
    }
}

/// Tree species whose `<species>_log` / `<species>_wood` blocks can be felled
/// (all of them in Pumpkin's block data, minus stripped variants).
const TREE_SPECIES: &[&str] = &[
    "acacia", "birch", "cherry", "dark_oak", "jungle", "mangrove", "oak", "pale_oak", "poplar",
    "spruce",
];
/// Huge-fungus species, whose `<species>_stem` / `<species>_hyphae` blocks can be felled.
const FUNGUS_SPECIES: &[&str] = &["crimson", "warped"];

/// Species of a natural (unstripped) log, wood, stem or hyphae block.
///
/// Only explicitly listed species qualify, so crop stems (`pumpkin_stem`,
/// `attached_melon_stem`), `mushroom_stem` and `big_dripleaf_stem` are never logs.
pub fn log_species(name: &str) -> Option<&str> {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    let find = |suffixes: [&str; 2], species: &[&'static str]| {
        suffixes
            .iter()
            .find_map(|suffix| name.strip_suffix(suffix))
            .and_then(|s| species.iter().copied().find(|&known| known == s))
    };
    find(["_log", "_wood"], TREE_SPECIES).or_else(|| find(["_stem", "_hyphae"], FUNGUS_SPECIES))
}

fn is_fungus(species: &str) -> bool {
    FUNGUS_SPECIES.contains(&species)
}

/// True for any axe item id (`minecraft:iron_axe`, ...), but not pickaxes.
pub fn is_axe(item_id: &str) -> bool {
    item_id.ends_with("_axe") && !item_id.ends_with("_pickaxe")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// Logs to fell, nearest to the cut first. Excludes the log the player broke.
    pub logs: Vec<Pos>,
    /// Natural leaves that may belong to the tree; recheck them after the logs are gone.
    pub leaves: Vec<Pos>,
}

fn neighbours26(p: Pos) -> impl Iterator<Item = Pos> {
    (-1..=1).flat_map(move |dx| {
        (-1..=1).flat_map(move |dy| {
            (-1..=1)
                .filter(move |&dz| (dx, dy, dz) != (0, 0, 0))
                .map(move |dz| (p.0 + dx, p.1 + dy, p.2 + dz))
        })
    })
}

fn neighbours6(p: Pos) -> [Pos; 6] {
    let (x, y, z) = p;
    [
        (x + 1, y, z),
        (x - 1, y, z),
        (x, y + 1, z),
        (x, y - 1, z),
        (x, y, z + 1),
        (x, y, z - 1),
    ]
}

/// Finds the tree above a log of `species` that was just broken at `origin`.
///
/// Logs of the same species connected to `origin` (diagonals included) at or above the
/// cut are felled. Returns `None` when no natural canopy of the right kind touches those
/// logs (non-persistent leaves for trees, wart blocks / shroomlight for huge fungi), so
/// log cabins and other builds are left alone.
pub fn find_tree(view: &mut impl BlockView, origin: Pos, species: &str) -> Option<Tree> {
    let fungus = is_fungus(species);
    let mut seen_logs = HashSet::from([origin]);
    let mut queue = VecDeque::from([origin]);
    let mut logs = Vec::new();
    let mut has_canopy = false;

    while let Some(pos) = queue.pop_front() {
        for n in neighbours26(pos) {
            if n.1 < origin.1 || !seen_logs.insert(n) {
                continue;
            }
            match view.kind(n) {
                Kind::Log(s) if s == species && logs.len() < MAX_LOGS => {
                    logs.push(n);
                    queue.push_back(n);
                }
                Kind::Leaves {
                    persistent: false, ..
                } if !fungus => has_canopy = true,
                Kind::FungusCanopy if fungus => has_canopy = true,
                _ => {}
            }
        }
    }

    if !has_canopy {
        return None;
    }

    // Leaves reachable from the felled logs through leaves, within vanilla decay reach.
    let tree_logs: HashSet<Pos> = logs.iter().copied().chain([origin]).collect();
    let mut seen_leaves = HashSet::new();
    let mut frontier: VecDeque<(Pos, u8)> = logs
        .iter()
        .chain(std::iter::once(&origin))
        .map(|&p| (p, 0))
        .collect();
    let mut leaves = Vec::new();
    while let Some((pos, depth)) = frontier.pop_front() {
        if depth >= LEAF_REACH || leaves.len() >= MAX_LEAVES {
            continue;
        }
        for n in neighbours6(pos) {
            if tree_logs.contains(&n) || !seen_leaves.insert(n) {
                continue;
            }
            if let Kind::Leaves {
                persistent: false, ..
            } = view.kind(n)
            {
                leaves.push(n);
                frontier.push_back((n, depth + 1));
            }
        }
    }

    Some(Tree { logs, leaves })
}

/// True if a leaf at `pos` has lost its tree and should be broken now.
pub fn leaf_is_orphaned(view: &mut impl BlockView, pos: Pos) -> bool {
    matches!(
        view.kind(pos),
        Kind::Leaves { persistent: false, distance } if distance >= DECAYED_DISTANCE
    )
}

/// Vanilla Unbreaking for tools: a use costs durability with chance 1 / (level + 1).
pub fn consumes_durability(unbreaking_level: u32, rng: &mut Rng) -> bool {
    unbreaking_level == 0 || rng.below(unbreaking_level + 1) == 0
}

/// Outcome of charging an axe for felling, decided before any block is broken.
#[derive(Debug, PartialEq, Eq)]
pub struct Charge {
    /// How many of the planned logs the axe pays for.
    pub logs: usize,
    /// The axe's damage after paying for them.
    pub damage: i32,
    /// The axe broke on the last paid log.
    pub broke: bool,
}

/// Charges `damage` (out of `max_damage`) for up to `logs` extra logs, one point per log
/// that Unbreaking doesn't save. Stops at the log that breaks the axe.
pub fn charge_axe(
    logs: usize,
    mut damage: i32,
    max_damage: i32,
    unbreaking_level: u32,
    rng: &mut Rng,
) -> Charge {
    for paid in 1..=logs {
        if consumes_durability(unbreaking_level, rng) {
            damage += 1;
            if damage >= max_damage {
                return Charge {
                    logs: paid,
                    damage,
                    broke: true,
                };
            }
        }
    }
    Charge {
        logs,
        damage,
        broke: false,
    }
}

/// Small xorshift64* generator; WASI randomness is not needed for durability rolls.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift, so mix the seed and force a set bit.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct World(HashMap<Pos, Kind>);

    impl World {
        fn log(&mut self, p: Pos, species: &str) {
            self.0.insert(p, Kind::Log(species.into()));
        }
        fn leaves(&mut self, p: Pos, persistent: bool) {
            self.0.insert(
                p,
                Kind::Leaves {
                    persistent,
                    distance: 1,
                },
            );
        }
    }

    impl BlockView for World {
        fn kind(&mut self, pos: Pos) -> Kind {
            self.0.get(&pos).cloned().unwrap_or(Kind::Other)
        }
    }

    /// A 5-log oak trunk at x=z=0 from y=0 with a leaf blob on top; origin (y=0) already broken.
    fn oak() -> World {
        let mut w = World::default();
        for y in 1..5 {
            w.log((0, y, 0), "oak");
        }
        for x in -2..=2 {
            for z in -2..=2 {
                for y in 3..6 {
                    if (x, z) != (0, 0) || y == 5 {
                        w.leaves((x, y, z), false);
                    }
                }
            }
        }
        w
    }

    #[test]
    fn classify_logs_and_leaves() {
        assert_eq!(classify("minecraft:oak_log", &[]), Kind::Log("oak".into()));
        assert_eq!(classify("dark_oak_wood", &[]), Kind::Log("dark_oak".into()));
        assert_eq!(
            classify("minecraft:crimson_stem", &[]),
            Kind::Log("crimson".into())
        );
        assert_eq!(classify("minecraft:stripped_oak_log", &[]), Kind::Other);
        assert_eq!(classify("minecraft:mushroom_stem", &[]), Kind::Other);
        let props = vec![
            ("distance".to_string(), "7".to_string()),
            ("persistent".to_string(), "false".to_string()),
        ];
        assert_eq!(
            classify("minecraft:birch_leaves", &props),
            Kind::Leaves {
                persistent: false,
                distance: 7
            }
        );
        assert_eq!(classify("minecraft:shroomlight", &[]), Kind::FungusCanopy);
    }

    #[test]
    fn axes_only() {
        assert!(is_axe("minecraft:netherite_axe"));
        assert!(!is_axe("minecraft:diamond_pickaxe"));
        assert!(!is_axe("minecraft:stick"));
    }

    #[test]
    fn fells_trunk_and_collects_leaves() {
        let mut w = oak();
        let tree = find_tree(&mut w, (0, 0, 0), "oak").expect("tree");
        assert_eq!(tree.logs, vec![(0, 1, 0), (0, 2, 0), (0, 3, 0), (0, 4, 0)]);
        assert_eq!(tree.leaves.len(), 5 * 5 * 3 - 2);
    }

    #[test]
    fn leaves_cabin_alone() {
        let mut w = World::default();
        for y in 1..4 {
            w.log((0, y, 0), "oak");
            w.log((1, y, 0), "oak");
        }
        assert_eq!(find_tree(&mut w, (0, 0, 0), "oak"), None);
    }

    #[test]
    fn player_placed_leaves_do_not_count() {
        let mut w = World::default();
        w.log((0, 1, 0), "oak");
        w.leaves((0, 2, 0), true);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "oak"), None);
    }

    #[test]
    fn ignores_logs_below_cut_and_other_species() {
        let mut w = oak();
        w.log((0, -1, 0), "oak");
        w.log((1, 1, 0), "birch");
        let tree = find_tree(&mut w, (0, 0, 0), "oak").expect("tree");
        assert!(!tree.logs.contains(&(0, -1, 0)));
        assert!(!tree.logs.contains(&(1, 1, 0)));
    }

    #[test]
    fn follows_diagonal_branches() {
        let mut w = oak();
        w.log((1, 5, 1), "oak");
        w.log((2, 6, 2), "oak");
        let tree = find_tree(&mut w, (0, 0, 0), "oak").expect("tree");
        assert!(tree.logs.contains(&(2, 6, 2)));
    }

    #[test]
    fn caps_log_count() {
        let mut w = World::default();
        for y in 1..1000 {
            w.log((0, y, 0), "spruce");
        }
        w.leaves((1, 1, 0), false);
        let tree = find_tree(&mut w, (0, 0, 0), "spruce").expect("tree");
        assert_eq!(tree.logs.len(), MAX_LOGS);
    }

    #[test]
    fn leaves_beyond_reach_are_not_collected() {
        let mut w = World::default();
        w.log((0, 1, 0), "oak");
        for x in 1..=10 {
            w.leaves((x, 1, 0), false);
        }
        let tree = find_tree(&mut w, (0, 0, 0), "oak").expect("tree");
        assert_eq!(tree.leaves.len(), usize::from(LEAF_REACH));
    }

    #[test]
    fn orphaned_leaf_detection() {
        let mut w = World::default();
        w.0.insert(
            (0, 0, 0),
            Kind::Leaves {
                persistent: false,
                distance: 7,
            },
        );
        w.0.insert(
            (1, 0, 0),
            Kind::Leaves {
                persistent: false,
                distance: 3,
            },
        );
        w.0.insert(
            (2, 0, 0),
            Kind::Leaves {
                persistent: true,
                distance: 7,
            },
        );
        assert!(leaf_is_orphaned(&mut w, (0, 0, 0)));
        assert!(!leaf_is_orphaned(&mut w, (1, 0, 0)));
        assert!(!leaf_is_orphaned(&mut w, (2, 0, 0)));
    }

    #[test]
    fn only_known_species_are_logs() {
        for crop in [
            "minecraft:pumpkin_stem",
            "minecraft:melon_stem",
            "minecraft:attached_pumpkin_stem",
            "minecraft:attached_melon_stem",
            "minecraft:big_dripleaf_stem",
            "minecraft:mushroom_stem",
            "minecraft:oak_stem",
            "minecraft:crimson_log",
        ] {
            assert_eq!(log_species(crop), None, "{crop}");
        }
        assert_eq!(log_species("minecraft:warped_hyphae"), Some("warped"));
        assert_eq!(log_species("minecraft:poplar_wood"), Some("poplar"));
    }

    #[test]
    fn shroomlight_does_not_make_oak_build_a_tree() {
        let mut w = World::default();
        w.log((0, 1, 0), "oak");
        w.log((0, 2, 0), "oak");
        w.0.insert((1, 2, 0), Kind::FungusCanopy);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "oak"), None);
    }

    #[test]
    fn huge_fungus_needs_fungal_canopy() {
        let mut w = World::default();
        w.log((0, 1, 0), "crimson");
        w.leaves((1, 1, 0), false);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "crimson"), None);
        w.0.insert((0, 2, 0), Kind::FungusCanopy);
        let tree = find_tree(&mut w, (0, 0, 0), "crimson").expect("fungus");
        assert_eq!(tree.logs, vec![(0, 1, 0)]);
    }

    #[test]
    fn charge_stops_at_breaking_log() {
        let mut rng = Rng::new(1);
        assert_eq!(
            charge_axe(10, 55, 59, 0, &mut rng),
            Charge {
                logs: 4,
                damage: 59,
                broke: true
            }
        );
        assert_eq!(
            charge_axe(3, 0, 59, 0, &mut rng),
            Charge {
                logs: 3,
                damage: 3,
                broke: false
            }
        );
        assert_eq!(
            charge_axe(0, 10, 59, 0, &mut rng),
            Charge {
                logs: 0,
                damage: 10,
                broke: false
            }
        );
    }

    #[test]
    fn unbreaking_odds() {
        let mut rng = Rng::new(42);
        assert!((0..100).all(|_| consumes_durability(0, &mut rng)));
        let used = (0..30_000)
            .filter(|_| consumes_durability(2, &mut rng))
            .count();
        // Expected 1/3 of uses.
        assert!((9_000..11_000).contains(&used), "{used}");
    }
}
