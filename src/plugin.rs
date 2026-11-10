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
//!    remembered non-persistent leaf that reached distance 7.

// The generated bindings contain host calls with many parameters.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use exports::pumpkin::plugin::{gametest_callbacks, metadata};
use pumpkin::plugin::common::{BlockPos, GameMode, Hand};
use pumpkin::plugin::enchantments::Enchantment;
use pumpkin::plugin::event::{EventPriority, EventType};
use pumpkin::plugin::item_stack::ItemStack;
use pumpkin::plugin::logging::{Level, log};
use pumpkin::plugin::player::Player;
use pumpkin::plugin::scheduler::schedule_delayed_task;
use pumpkin::plugin::sounds::{Sound, SoundCategory};
use pumpkin::plugin::uuid::Uuid;
use pumpkin::plugin::world::{BlockFlags, World, block_state_to_info};

use crate::tree::{self, BlockView, Charge, Kind, Pos, Rng};

const BREAK_HANDLER: u32 = 1;
const FELL_TASK: u32 = 1;
const LEAF_TASK: u32 = 2;
/// Leaves update their `distance` one scheduled tick per step, at most 7 steps.
const LEAF_DELAY_TICKS: u64 = 12;

struct FellJob {
    player: Uuid,
    world_id: String,
    origin: Pos,
    species: String,
}

/// Where the reserved axe is while felling runs.
enum Reserved {
    /// Still in its slot with the reserved damage; this copy identifies it.
    InSlot(ItemStack),
    /// Taken out of the inventory because the plan uses it up.
    Taken(ItemStack),
}

struct LeafJob {
    world_id: String,
    player: Uuid,
    drop_items: bool,
    leaves: Vec<Pos>,
}

thread_local! {
    static FELL_JOBS: RefCell<VecDeque<FellJob>> = const { RefCell::new(VecDeque::new()) };
    static LEAF_JOBS: RefCell<VecDeque<LeafJob>> = const { RefCell::new(VecDeque::new()) };
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
        self.cache
            .entry(state)
            .or_insert_with(|| {
                block_state_to_info(state).map_or(Kind::Other, |info| {
                    tree::classify(&info.name, &info.properties)
                })
            })
            .clone()
    }
}

/// Block flags for plugin-driven breaks: notify neighbours (so leaves recompute their
/// distance) and clients.
fn break_flags() -> BlockFlags {
    BlockFlags::NOTIFY_NEIGHBORS | BlockFlags::NOTIFY_LISTENERS
}

fn uses_durability(mode: GameMode) -> bool {
    matches!(mode, GameMode::Survival | GameMode::Adventure)
}

/// Queues a felling job if this break should fell a tree.
async fn on_block_break(player: &Player, block: &str, pos: BlockPos) {
    let Some(species) = tree::log_species(block) else {
        return;
    };
    if !player.is_sneaking() || player.get_gamemode() == GameMode::Spectator {
        return;
    }
    let Some(item) = player.get_item_in_hand(Hand::Right) else {
        return;
    };
    if !tree::is_axe(&item.get_registry_key().await) {
        return;
    }
    let job = FellJob {
        player: player.get_id(),
        world_id: player.get_world().get_id(),
        origin: (pos.x, pos.y, pos.z),
        species: species.to_owned(),
    };
    FELL_JOBS.with_borrow_mut(|jobs| jobs.push_back(job));
    schedule_delayed_task(FELL_TASK, 1);
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
    let mut view = WorldView::new(&world);

    // Still a log: a protection plugin or the server cancelled the break.
    if matches!(view.kind(job.origin), Kind::Log(_)) {
        return;
    }
    let Some(mut found) = tree::find_tree(&mut view, job.origin, &job.species) else {
        return;
    };

    // The player may have switched items during the tick; re-check the axe. Address it by
    // slot so the write-back below cannot land in a different slot.
    let slot = player.get_inventory().get_selected_slot();
    let Some(axe) = player.get_inventory_item(slot) else {
        return;
    };
    if !tree::is_axe(&axe.get_registry_key().await) {
        return;
    }

    let mode = player.get_gamemode();
    let drop_items = mode != GameMode::Creative;
    let mut reservation = None;
    if uses_durability(mode) && axe.is_damageable().await && !axe.is_unbreakable().await {
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
        let charge = Charge::plan(
            found.logs.len(),
            axe.get_damage().await,
            axe.get_max_damage().await,
            unbreaking,
            &mut rng,
        );

        // Reserve the cost before breaking anything, so moving the axe mid-felling can't
        // dodge it. Only touch the slot if it still holds the exact axe we read;
        // otherwise fell nothing rather than overwrite another item.
        let Some(current) = player.get_inventory_item(slot) else {
            return;
        };
        if !current.is_similar(axe).await {
            return;
        }
        current.set_damage(charge.reserved_damage()).await;
        if charge.breaks_axe() {
            // The plan uses the axe up: take it now, so it can't be moved out of reach of
            // settlement. `current` is kept in case failed breaks earn it back.
            player.set_inventory_item(slot, None).await;
            reservation = Some((charge, Reserved::Taken(current)));
        } else {
            let written = current.clone().await;
            player.set_inventory_item(slot, Some(current)).await;
            reservation = Some((charge, Reserved::InSlot(written)));
        }
    }

    let planned = found.logs.len();
    if let Some((charge, _)) = &reservation {
        found.logs.truncate(charge.logs());
    }
    let mut broken = Vec::with_capacity(found.logs.len());
    for &pos in &found.logs {
        // Breaks are awaited one by one; a block may have been replaced meanwhile.
        let ok = view.kind(pos) == Kind::Log(job.species.clone())
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
            if tree::is_fungus_canopy_at(&mut view, pos, &job.species) {
                world
                    .break_block(block_pos(pos), drop_items, break_flags())
                    .await;
            }
        }
    }
    drop(view);

    // Settle the reservation against the logs that actually broke: refund those that
    // didn't (replaced, or rejected by a protection plugin), and break the axe only if
    // the real breaks used it up.
    if let Some((charge, reserved)) = reservation {
        let settled = charge.settled_damage(|i| broken[i]);
        match reserved {
            Reserved::Taken(axe) if settled >= charge.max_damage() => {
                drop(axe);
                world.play_sound(
                    Sound::EntityItemBreak,
                    SoundCategory::Players,
                    player.get_position(),
                    0.8,
                    1.0,
                );
            }
            Reserved::Taken(axe) => {
                // Refunded: give the axe back, to its slot if still free.
                axe.set_damage(settled).await;
                if player.get_inventory_item(slot).is_none() {
                    player.set_inventory_item(slot, Some(axe)).await;
                } else {
                    player.get_inventory().give_item(axe, true).await;
                }
            }
            // If the player moved the axe meanwhile they only forfeit a refund: the full
            // cost is already on it.
            Reserved::InSlot(written) => {
                if settled != charge.reserved_damage()
                    && let Some(current) = player.get_inventory_item(slot)
                    && current.is_similar(written).await
                {
                    current.set_damage(settled).await;
                    player.set_inventory_item(slot, Some(current)).await;
                }
            }
        }
    }

    if !found.leaves.is_empty() {
        let job = LeafJob {
            world_id: world.get_id(),
            player: job.player,
            drop_items,
            leaves: found.leaves,
        };
        LEAF_JOBS.with_borrow_mut(|jobs| jobs.push_back(job));
        schedule_delayed_task(LEAF_TASK, LEAF_DELAY_TICKS);
    }
}

async fn clear_leaves(server: &ServerInstance, job: LeafJob) {
    // Worlds are reached through the player. If they left or changed world, natural
    // decay takes care of the leaves instead.
    let Some(world) = server
        .get_player_by_uuid(job.player)
        .map(|p| p.get_world())
        .filter(|w| w.get_id() == job.world_id)
    else {
        return;
    };
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
            authors: vec!["id3v1669".into()],
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
