//! Host-independent tree logic: block classification, tree search and durability maths.
//!
//! Everything here talks to the world through [`BlockView`], so it can be unit-tested
//! without a server.

use std::collections::{HashSet, VecDeque};

/// Connected groups with more logs than this are not felled at all (like FallingTree's
/// `maxSize` with `maxSizeAction = ABORT`, default 100): big log structures are much
/// more likely builds.
pub const MAX_LOGS: usize = 120;
/// Upper bound on leaves considered for instant removal.
pub const MAX_LEAVES: usize = 2048;
/// Upper bound on fungal canopy blocks broken with a huge fungus.
pub const MAX_FUNGUS_CANOPY: usize = 512;
/// Fungal canopy is broken only within this horizontal distance of the cut
/// (FallingTree's `breakNetherTreeWarts` uses the same 4 blocks).
pub const FUNGUS_CANOPY_RADIUS: i32 = 4;
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
    /// Non-decaying canopy of huge fungi. `wart_of` names the fungus species a wart block
    /// belongs to (`crimson` for `nether_wart_block`, `warped` for `warped_wart_block`);
    /// it is `None` for shroomlight, which grows in both.
    FungusCanopy {
        wart_of: Option<&'static str>,
    },
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
        "nether_wart_block" => Kind::FungusCanopy {
            wart_of: Some("crimson"),
        },
        "warped_wart_block" => Kind::FungusCanopy {
            wart_of: Some("warped"),
        },
        "shroomlight" => Kind::FungusCanopy { wart_of: None },
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
    /// Huge-fungus canopy (wart blocks, shroomlight) to break with the stems; it never
    /// decays on its own.
    pub fungus_canopy: Vec<Pos>,
}

/// Does `kind` count as natural canopy of `species`, the way a top log must be touched?
fn is_canopy_of(kind: &Kind, species: &str) -> bool {
    match kind {
        Kind::Leaves {
            persistent: false, ..
        } => !is_fungus(species),
        // Shroomlight is a common light block in nether builds, so only the species' own
        // wart block proves a huge fungus.
        Kind::FungusCanopy { wart_of: Some(s) } => *s == species,
        _ => false,
    }
}

/// Can `kind` be broken as part of the canopy of a huge fungus of `species`?
fn is_fungus_canopy_of(kind: &Kind, species: &str) -> bool {
    match kind {
        Kind::FungusCanopy { wart_of: Some(s) } => *s == species,
        Kind::FungusCanopy { wart_of: None } => true,
        _ => false,
    }
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
/// cut are felled. Like FallingTree, the search gives up (`None`) when:
/// - the group has more than [`MAX_LOGS`] logs, or
/// - no natural canopy touches a top-most log: non-persistent leaves for trees, the
///   species' own wart block for huge fungi.
///
/// That leaves log cabins, beams and walls that merely brush a tree alone.
pub fn find_tree(view: &mut impl BlockView, origin: Pos, species: &str) -> Option<Tree> {
    let mut seen_logs = HashSet::from([origin]);
    let mut queue = VecDeque::from([origin]);
    let mut logs = Vec::new();

    while let Some(pos) = queue.pop_front() {
        for n in neighbours26(pos) {
            if n.1 < origin.1 || !seen_logs.insert(n) {
                continue;
            }
            if view.kind(n) == Kind::Log(species.to_owned()) {
                if logs.len() == MAX_LOGS {
                    return None;
                }
                logs.push(n);
                queue.push_back(n);
            }
        }
    }

    let top_y = logs.iter().map(|p| p.1).max()?;
    let top_has_canopy = logs.iter().filter(|p| p.1 == top_y).any(|&top| {
        neighbours6(top)
            .into_iter()
            .any(|n| is_canopy_of(&view.kind(n), species))
    });
    if !top_has_canopy {
        return None;
    }

    let tree_logs: HashSet<Pos> = logs.iter().copied().chain([origin]).collect();
    if is_fungus(species) {
        let fungus_canopy = find_fungus_canopy(view, &logs, origin, top_y, species);
        return Some(Tree {
            logs,
            leaves: Vec::new(),
            fungus_canopy,
        });
    }

    // Leaves reachable from the felled logs through leaves, within vanilla decay reach.
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

    Some(Tree {
        logs,
        leaves,
        fungus_canopy: Vec::new(),
    })
}

/// Wart blocks and shroomlight connected to the stems through canopy (diagonals
/// included). As in FallingTree, only within [`FUNGUS_CANOPY_RADIUS`] horizontally of the
/// cut and at most one block above the top stem.
fn find_fungus_canopy(
    view: &mut impl BlockView,
    stems: &[Pos],
    origin: Pos,
    top_y: i32,
    species: &str,
) -> Vec<Pos> {
    let in_bounds = |p: Pos| {
        (p.0 - origin.0).abs() <= FUNGUS_CANOPY_RADIUS
            && (p.2 - origin.2).abs() <= FUNGUS_CANOPY_RADIUS
            && p.1 >= origin.1
            && p.1 <= top_y + 1
    };
    let mut seen: HashSet<Pos> = stems.iter().copied().chain([origin]).collect();
    let mut queue: VecDeque<Pos> = seen.iter().copied().collect();
    let mut canopy = Vec::new();
    while let Some(pos) = queue.pop_front() {
        for n in neighbours26(pos) {
            if canopy.len() >= MAX_FUNGUS_CANOPY || !in_bounds(n) || !seen.insert(n) {
                continue;
            }
            if is_fungus_canopy_of(&view.kind(n), species) {
                canopy.push(n);
                queue.push_back(n);
            }
        }
    }
    canopy
}

/// True if the block at `pos` is still fungal canopy of `species` (recheck before breaking).
pub fn is_fungus_canopy_at(view: &mut impl BlockView, pos: Pos, species: &str) -> bool {
    is_fungus_canopy_of(&view.kind(pos), species)
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

/// Durability plan for felling, decided before any block is broken.
///
/// The plan is reserved on the axe up front, so moving the axe mid-felling can't dodge
/// the cost, and then settled against the logs that actually broke.
#[derive(Debug, PartialEq, Eq)]
pub struct Charge {
    damage: i32,
    max_damage: i32,
    /// Per planned log: does it cost a point? Ends at the log that breaks the axe.
    costs: Vec<bool>,
}

impl Charge {
    /// Plans the cost of up to `logs` extra logs for an axe at `damage` out of
    /// `max_damage`: one point per log that Unbreaking doesn't save, stopping at the log
    /// that would break the axe.
    pub fn plan(
        logs: usize,
        damage: i32,
        max_damage: i32,
        unbreaking_level: u32,
        rng: &mut Rng,
    ) -> Self {
        let mut costs = Vec::with_capacity(logs);
        let mut total = damage;
        for _ in 0..logs {
            if total >= max_damage {
                break;
            }
            let cost = consumes_durability(unbreaking_level, rng);
            total += i32::from(cost);
            costs.push(cost);
        }
        Self {
            damage,
            max_damage,
            costs,
        }
    }

    /// How many of the planned logs the axe can pay for.
    pub fn logs(&self) -> usize {
        self.costs.len()
    }

    /// Damage if every planned log breaks; reserved on the axe before felling.
    pub fn reserved_damage(&self) -> i32 {
        self.damage + self.cost_of(|_| true)
    }

    /// The plan uses the axe up. It is then taken away before felling (so it can't be
    /// moved out of reach), and given back only if failed breaks earn a refund.
    pub fn breaks_axe(&self) -> bool {
        self.reserved_damage() >= self.max_damage
    }

    pub fn max_damage(&self) -> i32 {
        self.max_damage
    }

    /// Final damage once felling is done, counting only the logs for which `broke(i)`
    /// is true. The axe breaks if this reaches `max_damage`.
    pub fn settled_damage(&self, broke: impl Fn(usize) -> bool) -> i32 {
        self.damage + self.cost_of(broke)
    }

    fn cost_of(&self, include: impl Fn(usize) -> bool) -> i32 {
        let points = self
            .costs
            .iter()
            .enumerate()
            .filter(|&(i, &cost)| cost && include(i))
            .count();
        i32::try_from(points).unwrap_or(i32::MAX)
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
        assert_eq!(classify("minecraft:shroomlight", &[]), SHROOMLIGHT);
        assert_eq!(classify("minecraft:nether_wart_block", &[]), CRIMSON_WART);
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
    fn aborts_above_log_limit() {
        let column = |height: i32| {
            let mut w = World::default();
            for y in 1..=height {
                w.log((0, y, 0), "spruce");
            }
            w.leaves((0, height + 1, 0), false);
            w
        };
        let max = i32::try_from(MAX_LOGS).unwrap();
        let tree = find_tree(&mut column(max), (0, 0, 0), "spruce").expect("at limit");
        assert_eq!(tree.logs.len(), MAX_LOGS);
        assert_eq!(find_tree(&mut column(max + 1), (0, 0, 0), "spruce"), None);
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
        w.0.insert((1, 2, 0), SHROOMLIGHT);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "oak"), None);
    }

    const SHROOMLIGHT: Kind = Kind::FungusCanopy { wart_of: None };
    const CRIMSON_WART: Kind = Kind::FungusCanopy {
        wart_of: Some("crimson"),
    };
    const WARPED_WART: Kind = Kind::FungusCanopy {
        wart_of: Some("warped"),
    };

    #[test]
    fn huge_fungus_needs_its_own_wart_on_top() {
        let mut w = World::default();
        w.log((0, 1, 0), "crimson");
        w.leaves((1, 1, 0), false);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "crimson"), None);
        // Shroomlight alone (a nether build's lamp) is not enough, nor is the other wart.
        w.0.insert((0, 2, 0), SHROOMLIGHT);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "crimson"), None);
        w.0.insert((0, 2, 0), WARPED_WART);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "crimson"), None);
        w.0.insert((0, 2, 0), CRIMSON_WART);
        let tree = find_tree(&mut w, (0, 0, 0), "crimson").expect("fungus");
        assert_eq!(tree.logs, vec![(0, 1, 0)]);
        assert_eq!(tree.fungus_canopy, vec![(0, 2, 0)]);
    }

    #[test]
    fn collects_fungus_canopy_within_bounds() {
        let mut w = World::default();
        for y in 1..=5 {
            w.log((0, y, 0), "warped");
        }
        // Cap: a 7x7 wart layer at y=6 plus a shroomlight hanging under it.
        for x in -3..=3 {
            for z in -3..=3 {
                w.0.insert((x, 6, z), WARPED_WART);
            }
        }
        w.0.insert((2, 5, 2), SHROOMLIGHT);
        // Out of bounds: too far sideways, and more than one above the top stem.
        w.0.insert((5, 6, 0), WARPED_WART);
        w.0.insert((0, 7, 0), WARPED_WART);
        // Another species' wart is not part of this fungus.
        w.0.insert(
            (-3, 5, -3),
            Kind::FungusCanopy {
                wart_of: Some("crimson"),
            },
        );
        let tree = find_tree(&mut w, (0, 0, 0), "warped").expect("fungus");
        assert_eq!(tree.logs.len(), 5);
        assert_eq!(tree.fungus_canopy.len(), 7 * 7 + 1);
        assert!(!tree.fungus_canopy.contains(&(5, 6, 0)));
        assert!(!tree.fungus_canopy.contains(&(0, 7, 0)));
        assert!(!tree.fungus_canopy.contains(&(-3, 5, -3)));
    }

    #[test]
    fn wall_touching_tree_leaves_on_the_side_is_not_felled() {
        // A 4-high oak wall whose lower logs brush a neighbouring tree's leaves, with
        // nothing around its top.
        let mut w = World::default();
        for y in 1..=4 {
            w.log((0, y, 0), "oak");
        }
        w.leaves((1, 1, 0), false);
        w.leaves((1, 2, 0), false);
        assert_eq!(find_tree(&mut w, (0, 0, 0), "oak"), None);
    }

    #[test]
    fn charge_stops_at_breaking_log() {
        let mut rng = Rng::new(1);
        // 4 points left: pays for 4 logs and uses the axe up.
        let charge = Charge::plan(10, 55, 59, 0, &mut rng);
        assert_eq!(charge.logs(), 4);
        assert_eq!(charge.reserved_damage(), 59);
        assert!(charge.breaks_axe());
        assert_eq!(charge.settled_damage(|_| true), 59);

        // The reviewer's case: 1 point left, one more log breaks the axe.
        let charge = Charge::plan(5, 58, 59, 0, &mut rng);
        assert_eq!(charge.logs(), 1);
        assert!(charge.breaks_axe());

        let charge = Charge::plan(3, 0, 59, 0, &mut rng);
        assert_eq!(charge.logs(), 3);
        assert_eq!(charge.reserved_damage(), 3);
        assert!(!charge.breaks_axe());

        assert_eq!(Charge::plan(0, 10, 59, 0, &mut rng).reserved_damage(), 10);
        // Already at the limit: nothing can be paid for.
        assert_eq!(Charge::plan(5, 59, 59, 0, &mut rng).logs(), 0);
    }

    #[test]
    fn charge_refunds_failed_breaks() {
        let mut rng = Rng::new(1);
        let charge = Charge::plan(10, 55, 59, 0, &mut rng);
        // A protection plugin rejected every extra break: no durability is used.
        assert_eq!(charge.settled_damage(|_| false), 55);
        // Only the first two broke.
        assert_eq!(charge.settled_damage(|i| i < 2), 57);
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
