//! Pumpkin plugin ABI v0.2 glue: event and task handlers that drive [`crate::tree`].
//!
//! Flow:
//! 1. `block-break-event` (non-blocking, so it runs after every blocking handler has had
//!    the chance to cancel): a sneaking player broke a natural log with an axe. Queue a
//!    job and schedule [`FELL_TASK`] one tick later.
//! 2. [`FELL_TASK`]: if the log is really gone (the break was not cancelled), fell the
//!    connected logs with `world.break-block`, charging the axe per extra log. Remember
//!    the tree's leaves and schedule [`LEAF_TASK`].
//! 3. [`LEAF_TASK`]: by now Pumpkin has propagated the leaves' `distance`; break every
//!    remembered non-persistent leaf that reached distance 7, a batch per tick
//!    ([`LEAF_MORE_TASK`]).
//!
//! Each task pops the oldest job of its own queue. That pairing only holds while every
//! task of one handler id is scheduled with the same delay, hence one queue per delay.

// The generated bindings contain host calls with many parameters.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

use exports::pumpkin::plugin::{gametest_callbacks, metadata};
use pumpkin::plugin::common::{BlockPos, GameMode, Hand, NbtTag, NbtTree};
use pumpkin::plugin::enchantments::Enchantment;
use pumpkin::plugin::event::{EventPriority, EventType};
use pumpkin::plugin::item_stack::ItemStack;
use pumpkin::plugin::logging::{Level, log};
use pumpkin::plugin::permission::{Permission, PermissionDefault, PermissionLevel};
use pumpkin::plugin::player::Player;
use pumpkin::plugin::scheduler::schedule_delayed_task;
use pumpkin::plugin::sounds::{Sound, SoundCategory};
use pumpkin::plugin::uuid::Uuid;
use pumpkin::plugin::world::{BlockFlags, World, block_state_to_info};

use crate::tree::{self, BlockView, Charge, Kind, Pos, Rng};

const BREAK_HANDLER: u32 = 1;
const FELL_TASK: u32 = 1;
const LEAF_TASK: u32 = 2;
const LEAF_MORE_TASK: u32 = 3;
/// Leaves update their `distance` one scheduled tick per step, at most 7 steps.
const LEAF_DELAY_TICKS: u64 = 12;
/// Leaves broken per tick, so a big canopy does not land in a single tick.
const LEAF_BATCH: usize = 256;
/// Players without this permission never fell trees. The host requires the plugin
/// name as namespace.
const USE_PERMISSION: &str = "fallingtree:use";

struct FellJob {
    player: Uuid,
    world_id: String,
    origin: Pos,
    species: &'static str,
}

struct LeafJob {
    world_id: String,
    drop_items: bool,
    leaves: Vec<Pos>,
}

thread_local! {
    static FELL_JOBS: RefCell<VecDeque<FellJob>> = const { RefCell::new(VecDeque::new()) };
    static LEAF_JOBS: RefCell<VecDeque<LeafJob>> = const { RefCell::new(VecDeque::new()) };
    static LEAF_MORE_JOBS: RefCell<VecDeque<LeafJob>> = const { RefCell::new(VecDeque::new()) };
}

fn block_pos((x, y, z): Pos) -> BlockPos {
    BlockPos { x, y, z }
}

/// Reads blocks from a world, caching classification per block state id.
struct WorldView<'a> {
    world: &'a World,
    cache: HashMap<u16, Kind>,
}

impl<'a> WorldView<'a> {
    fn new(world: &'a World) -> Self {
        Self {
            world,
            cache: HashMap::new(),
        }
    }
}

impl BlockView for WorldView<'_> {
    fn kind(&mut self, pos: Pos) -> Kind {
        let state = self.world.get_block_state_id(block_pos(pos));
        *self.cache.entry(state).or_insert_with(|| {
            block_state_to_info(state).map_or(Kind::Other, |info| {
                tree::classify(&info.name, &info.properties)
            })
        })
    }
}

/// Block flags for plugin-driven breaks: notify neighbours (so leaves recompute their
/// distance) and clients.
fn break_flags() -> BlockFlags {
    BlockFlags::NOTIFY_NEIGHBORS | BlockFlags::NOTIFY_LISTENERS
}

/// Felling is for the modes that break blocks freely. Adventure players may only break
/// what their tool allows, which the extra breaks would not check.
fn can_fell(mode: GameMode) -> bool {
    matches!(mode, GameMode::Survival | GameMode::Creative)
}

/// Queues a felling job if this break should fell a tree.
async fn on_block_break(player: &Player, block: &str, pos: BlockPos) {
    let Some(species) = tree::log_species(block) else {
        return;
    };
    if !player.is_sneaking() || !can_fell(player.get_gamemode()) {
        return;
    }
    let Some(item) = player.get_item_in_hand(Hand::Right) else {
        return;
    };
    if !tree::is_axe(&item.get_registry_key().await)
        || !player.has_permission(USE_PERMISSION.into()).await
    {
        return;
    }
    let job = FellJob {
        player: player.get_id(),
        world_id: player.get_world().get_id(),
        origin: (pos.x, pos.y, pos.z),
        species,
    };
    FELL_JOBS.with_borrow_mut(|jobs| jobs.push_back(job));
    schedule_delayed_task(FELL_TASK, 1);
}

/// Replaces the stack in `slot` with `new`, but only if the slot still holds `expected`.
///
/// The host has no compare-and-set, and the player's own inventory actions run in
/// parallel with this task, so the check and the write cannot be made atomic from a
/// plugin. Everything is prepared beforehand to keep a single host call between them.
async fn replace_in_slot(
    player: &Player,
    slot: u8,
    expected: ItemStack,
    new: Option<ItemStack>,
) -> bool {
    let Some(current) = player.get_inventory_item(slot) else {
        return false;
    };
    if !current.is_similar(expected).await {
        return false;
    }
    player.set_inventory_item(slot, new).await;
    true
}

/// Custom-data location of the marker that identifies a charged axe.
const MARKER_NAMESPACE: &str = "fallingtree";
const MARKER_KEY: &str = "reservation";

/// A marker value no other reservation uses: the world age keeps values from earlier
/// server runs apart, the counter those within a run.
fn new_marker(world: &World) -> i64 {
    thread_local!(static COUNTER: Cell<u64> = const { Cell::new(0) });
    let count = COUNTER.replace(COUNTER.get().wrapping_add(1));
    (world.get_world_age().wrapping_shl(20) ^ count) as i64
}

async fn marker_of(stack: &ItemStack) -> Option<i64> {
    let tree = stack
        .get_custom_data(MARKER_NAMESPACE.into(), MARKER_KEY.into())
        .await?;
    match tree.tags.get(tree.root as usize)? {
        NbtTag::Long(marker) => Some(*marker),
        _ => None,
    }
}

/// Where the charged axe is while felling runs.
enum Reserved {
    /// In its slot with the full cost on it, carrying this marker.
    InSlot(i64),
    /// The plan uses the axe up, so it was taken out of the inventory: left there, it
    /// could be moved or dropped and escape breaking. Carries the full cost.
    Taken(ItemStack),
}

/// Settles the charged axe if it is still in `slot`: removes its marker and gives
/// `refund` durability back.
///
/// Only the stack carrying `marker` is the charged axe, whatever wear it has taken
/// since. If the player moved it away they forfeit the refund (the full cost is already
/// on it) and the marker stays until that axe fells again.
async fn settle_in_slot(player: &Player, slot: u8, marker: i64, refund: i32) {
    // Retried because the axe may change between the read and the write.
    for _ in 0..3 {
        let Some(current) = player.get_inventory_item(slot) else {
            return;
        };
        if marker_of(&current).await != Some(marker) {
            return;
        }
        let updated = current.clone().await;
        updated
            .remove_custom_data(MARKER_NAMESPACE.into(), MARKER_KEY.into())
            .await;
        updated
            .set_damage((current.get_damage().await - refund).max(0))
            .await;
        if replace_in_slot(player, slot, current, Some(updated)).await {
            return;
        }
    }
}

async fn fell(server: &ServerInstance, job: FellJob) {
    let Some(player) = server.get_player_by_uuid(job.player) else {
        return;
    };
    // The coordinates belong to the world the log was broken in; if the player has
    // changed world since, they would point at unrelated blocks.
    let world = player.get_world();
    if world.get_id() != job.world_id {
        return;
    }
    let mode = player.get_gamemode();
    if !can_fell(mode) {
        return;
    }

    // The player may have switched items during the tick; re-check the axe before the
    // (much more expensive) tree search. Address it by slot so the writes below cannot
    // land in a different slot.
    let slot = player.get_inventory().get_selected_slot();
    let Some(axe) = player.get_inventory_item(slot) else {
        return;
    };
    if !tree::is_axe(&axe.get_registry_key().await) {
        return;
    }

    let mut view = WorldView::new(&world);
    // Still a log: a protection plugin or the server cancelled the break.
    if matches!(view.kind(job.origin), Kind::Log(_)) {
        return;
    }
    let Some(mut found) = tree::find_tree(&mut view, job.origin, job.species) else {
        return;
    };
    let planned = found.logs.len();

    // Plugin-driven breaks carry no player, so the server's spawn protection would not
    // stop them. Like the server, exempt only permission level 4.
    if player.get_permission_level() != PermissionLevel::Four {
        let spawn = world.get_spawn_location().pos;
        let spawn = (spawn.x, spawn.y, spawn.z);
        let open = |pos: &Pos| !tree::in_spawn_protection(*pos, spawn);
        found.logs.retain(open);
        found.fungus_canopy.retain(open);
        found.leaves.retain(open);
    }

    let drop_items = mode != GameMode::Creative;
    let mut reservation = None;
    if mode == GameMode::Survival && axe.is_damageable().await && !axe.is_unbreakable().await {
        let unbreaking = axe
            .get_enchantments()
            .await
            .iter()
            .find(|e| e.enchantment == Enchantment::Unbreaking)
            .map_or(0, |e| e.level);
        let (ox, oy, oz) = job.origin;
        let mut rng = Rng::new(
            world.get_world_age()
                ^ (ox as u64).rotate_left(40)
                ^ (oy as u64).rotate_left(20)
                ^ oz as u64,
        );
        let max_damage = axe.get_max_damage().await;
        let charge = Charge::plan(
            found.logs.len(),
            axe.get_damage().await,
            max_damage,
            unbreaking,
            &mut rng,
        );

        // Charge the whole plan before breaking anything, so moving the axe mid-felling
        // can't dodge it. If the slot no longer holds the exact axe we read, fell
        // nothing rather than overwrite another item.
        let reserved = charge.reserved_damage();
        let charged = axe.clone().await;
        // A marker left over from a felling this axe was moved out of.
        charged
            .remove_custom_data(MARKER_NAMESPACE.into(), MARKER_KEY.into())
            .await;
        charged.set_damage(reserved).await;
        let held = if reserved >= max_damage {
            if !replace_in_slot(&player, slot, axe, None).await {
                return;
            }
            Reserved::Taken(charged)
        } else {
            let marker = new_marker(&world);
            let value = NbtTree {
                root: 0,
                tags: vec![NbtTag::Long(marker)],
            };
            charged
                .set_custom_data(MARKER_NAMESPACE.into(), MARKER_KEY.into(), value)
                .await;
            if !replace_in_slot(&player, slot, axe, Some(charged)).await {
                return;
            }
            Reserved::InSlot(marker)
        };
        found.logs.truncate(charge.logs());
        reservation = Some((charge, held));
    }

    let mut broken = Vec::with_capacity(found.logs.len());
    for &pos in &found.logs {
        // Breaks are awaited one by one; a block may have been replaced meanwhile.
        let ok = tree::is_log_of(view.kind(pos), job.species)
            && world
                .break_block(block_pos(pos), drop_items, break_flags())
                .await;
        broken.push(ok);
    }
    // Fungal canopy never decays, so it goes with the stems (free, like leaves), but only
    // once every stem is down: canopy still held up by remaining stems stays.
    let fully_felled = broken.len() == planned && broken.iter().all(|&ok| ok);
    if fully_felled {
        for &pos in &found.fungus_canopy {
            if tree::is_fungus_canopy_at(&mut view, pos, job.species) {
                world
                    .break_block(block_pos(pos), drop_items, break_flags())
                    .await;
            }
        }
    }
    drop(view);

    // Refund the logs that didn't break (replaced, or rejected by a protection plugin).
    if let Some((charge, held)) = reservation {
        let reserved = charge.reserved_damage();
        let refund = charge.refund(|i| broken[i]);
        match held {
            Reserved::InSlot(marker) => settle_in_slot(&player, slot, marker, refund).await,
            // The real breaks used the axe up.
            Reserved::Taken(axe) if refund == 0 => {
                drop(axe);
                world.play_sound(
                    Sound::EntityItemBreak,
                    SoundCategory::Players,
                    player.get_position(),
                    0.8,
                    1.0,
                );
            }
            // Not used up after all: give it back, to its slot if that is still free.
            // If the player left meanwhile, leave it where the tree stood.
            Reserved::Taken(axe) => {
                axe.set_damage(reserved - refund).await;
                if server.get_player_by_uuid(job.player).is_none() {
                    let (x, y, z) = job.origin;
                    let pos = (f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5);
                    world.drop_item(pos, axe).await;
                } else if player.get_inventory_item(slot).is_none() {
                    player.set_inventory_item(slot, Some(axe)).await;
                } else {
                    player.get_inventory().give_item(axe, true).await;
                }
            }
        }
    }

    if !found.leaves.is_empty() {
        let job = LeafJob {
            world_id: job.world_id,
            drop_items,
            leaves: found.leaves,
        };
        LEAF_JOBS.with_borrow_mut(|jobs| jobs.push_back(job));
        schedule_delayed_task(LEAF_TASK, LEAF_DELAY_TICKS);
    }
}

async fn clear_leaves(server: &ServerInstance, mut job: LeafJob) {
    // If the world is gone, there is nothing to clean up.
    let Some(world) = server.get_world_by_name(&job.world_id) else {
        return;
    };
    let rest = job.leaves.split_off(job.leaves.len().min(LEAF_BATCH));
    let mut view = WorldView::new(&world);
    for pos in job.leaves {
        // Checked right before each break; earlier breaks are awaited and the block may
        // have changed meanwhile.
        if tree::leaf_is_orphaned(&mut view, pos) {
            world
                .break_block(block_pos(pos), job.drop_items, break_flags())
                .await;
        }
    }
    if !rest.is_empty() {
        let job = LeafJob {
            leaves: rest,
            ..job
        };
        LEAF_MORE_JOBS.with_borrow_mut(|jobs| jobs.push_back(job));
        schedule_delayed_task(LEAF_MORE_TASK, 1);
    }
}

struct FallingTree;

impl Guest for FallingTree {
    fn init_plugin() {}

    async fn on_load(context: Context) -> Result<(), String> {
        context.register_event(
            BREAK_HANDLER,
            EventType::BlockBreakEvent,
            EventPriority::Normal,
            false,
        );
        let permission = Permission {
            node: USE_PERMISSION.into(),
            description: "Fell whole trees by sneaking and breaking a log with an axe".into(),
            default: PermissionDefault::Allow,
            children: Vec::new(),
        };
        // Not fatal: the node is still checked, it just has no registered default.
        if let Err(error) = context.register_permission(&permission) {
            log(
                Level::Warn,
                &format!("could not register {USE_PERMISSION}: {error}"),
            );
        }
        log(
            Level::Info,
            "FallingTree loaded: sneak and break a log with an axe to fell the tree",
        );
        Ok(())
    }

    async fn on_unload(_context: Context) -> Result<(), String> {
        Ok(())
    }

    async fn handle_event(event_id: u32, _server: ServerInstance, event: Event) -> Event {
        if event_id == BREAK_HANDLER
            && let Event::BlockBreakEvent(data) = &event
            && !data.cancelled
            && let Some(player) = &data.player
        {
            on_block_break(player, &data.block, data.block_pos).await;
        }
        event
    }

    async fn handle_task(handler_id: u32, server: ServerInstance) {
        match handler_id {
            FELL_TASK => {
                if let Some(job) = FELL_JOBS.with_borrow_mut(VecDeque::pop_front) {
                    fell(&server, job).await;
                }
            }
            LEAF_TASK => {
                if let Some(job) = LEAF_JOBS.with_borrow_mut(VecDeque::pop_front) {
                    clear_leaves(&server, job).await;
                }
            }
            LEAF_MORE_TASK => {
                if let Some(job) = LEAF_MORE_JOBS.with_borrow_mut(VecDeque::pop_front) {
                    clear_leaves(&server, job).await;
                }
            }
            _ => {}
        }
    }

    async fn handle_command(
        _command_id: u32,
        _sender: CommandSender,
        _server: ServerInstance,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        Err(CommandError::InvalidRequirement)
    }

    async fn handle_command_suggestion(
        _handler_id: u32,
        _sender: CommandSender,
        _server: ServerInstance,
        _request: SuggestionRequest,
    ) -> CommandSuggestions {
        CommandSuggestions {
            start: 0,
            length: 0,
            values: Vec::new(),
        }
    }

    async fn handle_ipc_message(
        _sender: PluginId,
        _message: IpcMessage,
    ) -> Result<IpcMessage, String> {
        Err("fallingtree does not handle IPC messages".into())
    }

    async fn handle_ai_goal_can_start(
        _goal_id: u32,
        _server: ServerInstance,
        _entity: Entity,
    ) -> bool {
        false
    }

    async fn handle_ai_goal_should_continue(
        _goal_id: u32,
        _server: ServerInstance,
        _entity: Entity,
    ) -> bool {
        false
    }

    async fn handle_ai_goal_start(_goal_id: u32, _server: ServerInstance, _entity: Entity) {}

    async fn handle_ai_goal_tick(_goal_id: u32, _server: ServerInstance, _entity: Entity) {}

    async fn handle_ai_goal_stop(_goal_id: u32, _server: ServerInstance, _entity: Entity) {}

    async fn handle_generate_phase(
        _generator_id: u32,
        _phase: GenerationPhase,
        _chunk: ChunkBuffer,
    ) {
    }
}

impl metadata::Guest for FallingTree {
    fn get_metadata() -> metadata::PluginMetadata {
        metadata::PluginMetadata {
            name: "fallingtree".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: env!("CARGO_PKG_AUTHORS")
                .split(':')
                .map(Into::into)
                .collect(),
            description: env!("CARGO_PKG_DESCRIPTION").into(),
            dependencies: Vec::new(),
            permissions: Vec::new(),
        }
    }
}

impl gametest_callbacks::Guest for FallingTree {
    fn invoke_void(_callback: gametest_callbacks::VoidCallbackId) {}

    fn invoke_test(_callback: gametest_callbacks::TestCallbackId, _test: gametest_callbacks::Test) {
    }

    async fn invoke_async_test(
        _callback: gametest_callbacks::AsyncTestCallbackId,
        _test: gametest_callbacks::Test,
    ) {
    }

    fn invoke_block_predicate(
        _callback: gametest_callbacks::BlockPredicateCallbackId,
        _permutation: gametest_callbacks::BlockPermutation,
    ) -> bool {
        false
    }

    fn invoke_entity_predicate(
        _callback: gametest_callbacks::EntityPredicateCallbackId,
        _entity: &gametest_callbacks::Entity,
    ) -> bool {
        false
    }
}

export!(FallingTree);
