//! Sampler pools, which represent primary sampler player mechanism.

use crate::node::ParamFollower;
use crate::prelude::{AudioContext, Connect, DefaultPool, FirewheelNode, PoolLabel, VolumeNode};
use crate::sample::{OnComplete, PlaybackSettings, QueuedSample, Sample, SamplePlayer};
use crate::{node::Events, SeedlingSystems};
use bevy_app::{Last, Plugin, PostUpdate};
use bevy_asset::Assets;
use bevy_ecs::{component::ComponentId, prelude::*, world::DeferredWorld};
use bevy_hierarchy::DespawnRecursiveExt;
use bevy_utils::HashSet;
use dynamic::DynamicPoolRegistry;
use firewheel::{
    event::{NodeEventType, SequenceCommand},
    nodes::sampler::{SamplerNode, SamplerState},
    Volume,
};
use std::any::TypeId;
use std::sync::Arc;

pub mod builder;
pub mod dynamic;
pub mod label;

use label::PoolLabelContainer;

pub(crate) struct SamplePoolPlugin;

impl Plugin for SamplePoolPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.init_resource::<dynamic::Registries>()
            .add_systems(
                Last,
                (
                    (remove_finished, assign_default)
                        .before(SeedlingSystems::Queue)
                        .after(SeedlingSystems::Acquire),
                    monitor_active
                        .before(SeedlingSystems::Flush)
                        .after(SeedlingSystems::Queue),
                ),
            )
            .add_systems(PostUpdate, dynamic::update_auto_pools);
    }
}

/// Spawn an effects chain, connecting all nodes and
/// returning the root sampler node.
#[cfg_attr(debug_assertions, track_caller)]
fn spawn_chain<L: Component + Clone>(
    bus: Entity,
    defaults: &SamplePoolTypes,
    label: L,
    commands: &mut Commands,
) -> Entity {
    let chain = defaults.spawn_nodes(label.clone(), commands);

    let source = commands
        .spawn((
            SamplerNode::default(),
            SamplePoolNode,
            label,
            EffectsChain(chain.clone()),
            PoolRoot(bus),
        ))
        .id();

    let mut chain = chain;
    chain.push(bus);

    commands.entity(source).connect(chain[0]);

    for pair in chain.windows(2) {
        commands.entity(pair[0]).connect(pair[1]);
    }

    source
}

/// A resource to keep track of which label types
/// have already been registered.
#[derive(Resource, Default)]
pub(crate) struct RegisteredPools(HashSet<TypeId>);

/// Spawn a sampler pool with an initial size.
#[cfg_attr(debug_assertions, track_caller)]
fn spawn_pool<'a, L: PoolLabel + Component + Clone>(
    label: L,
    size: core::ops::RangeInclusive<usize>,
    defaults: SamplePoolTypes,
    commands: &'a mut Commands,
) -> EntityCommands<'a> {
    commands.queue(|world: &mut World| {
        let mut resource = world.get_resource_or_init::<RegisteredPools>();

        if resource.0.insert(TypeId::of::<L>()) {
            world.schedule_scope(Last, |_, schedule| {
                schedule.add_systems(
                    (rank_nodes::<L>, assign_work::<L>)
                        .chain()
                        .in_set(SeedlingSystems::Queue),
                );
            });
        }
    });

    commands.despawn_pool(label.clone());

    let bus = commands
        .spawn((
            VolumeNode {
                volume: Volume::Linear(1.0),
            },
            SamplePoolNode,
            label.clone(),
            NodeRank::default(),
            PoolRange(size.clone()),
        ))
        .id();

    let mut nodes = Vec::new();
    nodes.reserve_exact(*size.start());
    for _ in 0..*size.start() {
        let node = spawn_chain(bus, &defaults, label.clone(), commands);
        nodes.push(node);
    }

    let mut bus = commands.entity(bus);
    bus.insert((SamplerNodes(nodes), defaults));

    bus
}

/// The root pool node, analogous to `Parent`.
#[derive(Component)]
struct PoolRoot(Entity);

/// A collection of each sampler node in the pool.
#[derive(Component)]
#[component(on_remove = on_remove_sampler_nodes)]
struct SamplerNodes(Vec<Entity>);

impl core::ops::Deref for SamplerNodes {
    type Target = [Entity];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn on_remove_sampler_nodes(mut world: DeferredWorld, entity: Entity, _: ComponentId) {
    let Some(mut nodes) = world.get_mut::<SamplerNodes>(entity) else {
        return;
    };

    let nodes = core::mem::take(&mut nodes.0);

    let mut commands = world.commands();
    for node in nodes {
        commands.entity(node).try_despawn();
    }
}

#[derive(Component)]
struct SamplePoolNode;

#[derive(Component)]
#[component(on_remove = on_remove_effects_chain)]
struct EffectsChain(Vec<Entity>);

fn on_remove_effects_chain(mut world: DeferredWorld, entity: Entity, _: ComponentId) {
    let Some(mut nodes) = world.get_mut::<EffectsChain>(entity) else {
        return;
    };

    let nodes = core::mem::take(&mut nodes.0);

    let mut commands = world.commands();
    for node in nodes {
        commands.entity(node).try_despawn();
    }
}

trait SamplePoolType {
    /// Insert the pool's default value if the component isn't already present.
    fn insert_default(&self, commands: &mut EntityCommands);

    /// Remove this type and all required types from an entity.
    fn remove(&self, commands: &mut EntityCommands);
}

impl<T: Component + Clone> SamplePoolType for T {
    fn insert_default(&self, commands: &mut EntityCommands) {
        commands.entry::<T>().or_insert_with(|| self.clone());
    }

    fn remove(&self, commands: &mut EntityCommands) {
        commands.remove_with_requires::<T>();
        // TODO: this might panic for non-diffing nodes
        commands.remove_with_requires::<crate::node::Baseline<T>>();
    }
}

/// A collections of types that manage insertion and removal of remote nodes.
#[derive(Component, Default, Clone)]
struct SamplePoolTypes(Vec<Arc<dyn SamplePoolType + Send + Sync + 'static>>);

impl core::fmt::Debug for SamplePoolTypes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SamplePoolTypes").finish_non_exhaustive()
    }
}

impl SamplePoolTypes {
    /// Push a default node value.
    pub fn push<T: Component + Clone>(&mut self, node: T) {
        self.0.push(Arc::new(node));
    }

    /// Spawn a set of unconnected audio nodes.
    pub fn spawn_nodes<L: Component + Clone>(
        &self,
        label: L,
        commands: &mut Commands,
    ) -> Vec<Entity> {
        self.0
            .iter()
            .map(|ty| {
                let mut commands = commands.spawn((label.clone(), SamplePoolNode));
                ty.insert_default(&mut commands);

                commands.id()
            })
            .collect()
    }

    /// Remove nodes.
    pub fn remove_nodes(&self, commands: &mut EntityCommands) {
        for ty in &self.0 {
            ty.remove(commands);
        }
    }
}

/// Sets the range for the number of pool sampler nodes.
#[derive(Component, Clone, Debug)]
struct PoolRange(pub core::ops::RangeInclusive<usize>);

/// Sampler node ranking for playback.
#[derive(Default, Component)]
struct NodeRank(Vec<(Entity, u64)>);

fn rank_nodes<T: Component>(
    q: Query<
        (Entity, &SamplerNode, &FirewheelNode, &PoolLabelContainer),
        (With<SamplePoolNode>, With<T>),
    >,
    mut rank: Query<(&mut NodeRank, &PoolLabelContainer), With<T>>,
    mut context: ResMut<AudioContext>,
) {
    for (mut rank, label) in rank.iter_mut() {
        rank.0.clear();

        context.with(|c| {
            for (e, params, node, node_label) in q.iter() {
                if node_label.label != label.label {
                    continue;
                }

                let Some(state) = c.node_state::<SamplerState>(node.0) else {
                    continue;
                };

                let score = state.worker_score(params);

                rank.0.push((e, score));
            }
        });

        rank.0
            .sort_unstable_by_key(|pair| std::cmp::Reverse(pair.1));
    }
}

#[derive(Component, Clone, Copy)]
struct ActiveSample {
    sample_entity: Entity,
}

/// Automatically remove or despawn sample players when their
/// sample has finished playing.
fn remove_finished(
    nodes: Query<
        (
            Entity,
            &EffectsChain,
            &FirewheelNode,
            &ActiveSample,
            &PoolRoot,
        ),
        With<SamplerNode>,
    >,
    samples: Query<&PlaybackSettings>,
    roots: Query<&SamplePoolTypes>,
    mut commands: Commands,
    mut context: ResMut<AudioContext>,
) {
    context.with(|context| {
        for (entity, effects_chain, node, active, pool_root) in nodes.iter() {
            let Some(state) = context.node_state::<SamplerState>(node.0) else {
                continue;
            };

            let state = state.playback_state();

            // TODO: this will remove samples when paused
            if !state.is_playing() {
                commands.entity(entity).remove::<ActiveSample>();

                for effect in effects_chain.0.iter() {
                    commands.entity(*effect).remove::<ParamFollower>();
                }

                let Ok(settings) = samples.get(active.sample_entity) else {
                    continue;
                };

                match settings.on_complete {
                    OnComplete::Preserve => {}
                    OnComplete::Remove => {
                        let Ok(root) = roots.get(pool_root.0) else {
                            continue;
                        };

                        let mut entity_commands = commands.entity(active.sample_entity);
                        root.remove_nodes(&mut entity_commands);
                        entity_commands.remove_with_requires::<(
                            SamplePoolTypes,
                            SamplePlayer,
                            PoolLabelContainer,
                            DynamicPoolRegistry,
                        )>();
                    }
                    OnComplete::Despawn => {
                        commands.entity(active.sample_entity).despawn_recursive();
                    }
                }
            }
        }
    });
}

/// Scan through the set of pending sample players
/// and assign work to the most appropriate sampler node.
fn assign_work<T: Component + Clone>(
    mut nodes: Query<
        (
            Entity,
            &mut SamplerNode,
            &mut Events,
            &EffectsChain,
            &FirewheelNode,
        ),
        (With<SamplePoolNode>, With<T>),
    >,
    queued_samples: Query<
        (
            Entity,
            &SamplePlayer,
            &PlaybackSettings,
            &PoolLabelContainer,
        ),
        (With<QueuedSample>, With<T>),
    >,
    mut pools: Query<(
        Entity,
        &mut NodeRank,
        &SamplePoolTypes,
        &T,
        &PoolLabelContainer,
        &PoolRange,
        &mut SamplerNodes,
    )>,
    assets: Res<Assets<Sample>>,
    mut commands: Commands,
    mut context: ResMut<AudioContext>,
) {
    context.with(|context| {
        for (sample, player, settings, label) in queued_samples.iter() {
            let Some(asset) = assets.get(&player.0) else {
                continue;
            };

            let Some((pool_entity, mut rank, defaults, pool_label, _, pool_range, mut pool_nodes)) =
                pools.iter_mut().find(|pool| pool.4.label == label.label)
            else {
                continue;
            };

            // get the best candidate
            let Some((node_entity, _)) = rank.0.first() else {
                // Try to grow the pool if it's reached max capacity.
                // TODO: find a decent way to do this eagerly.
                let current_size = pool_nodes.len();
                let max_size = *pool_range.0.end();

                if current_size < max_size {
                    let new_size = (current_size * 2).min(max_size);

                    for _ in 0..new_size - current_size {
                        let new_sampler =
                            spawn_chain(pool_entity, defaults, pool_label.clone(), &mut commands);
                        pool_nodes.0.push(new_sampler);
                    }
                }

                continue;
            };

            let Ok((node_entity, mut params, mut events, effects_chain, sampler_id)) =
                nodes.get_mut(*node_entity)
            else {
                continue;
            };

            let Some(sampler_state) = context.node_state::<SamplerState>(sampler_id.0) else {
                continue;
            };

            params.set_sample(asset.get(), settings.volume, settings.repeat_mode);
            let event = sampler_state.sync_params_event(&params, true);
            events.push(event);

            // redirect all parameters to follow the sample source
            for effect in effects_chain.0.iter() {
                commands.entity(*effect).insert(ParamFollower(sample));
            }

            // Insert default pool parameters if not present.
            for ty in defaults.0.iter() {
                ty.insert_default(&mut commands.entity(sample));
            }

            rank.0.remove(0);
            commands.entity(sample).remove::<QueuedSample>();
            commands.entity(node_entity).insert(ActiveSample {
                sample_entity: sample,
            });
        }
    });
}

// Stop playback if the source entity no longer exists.
fn monitor_active(
    mut nodes: Query<(Entity, &ActiveSample, &mut Events, &EffectsChain)>,
    samples: Query<&SamplePlayer>,
    mut commands: Commands,
) {
    for (node_entity, active, mut events, effects_chain) in nodes.iter_mut() {
        if samples.get(active.sample_entity).is_err() {
            events.push(NodeEventType::SequenceCommand(SequenceCommand::Stop));

            commands.entity(node_entity).remove::<ActiveSample>();

            for effect in effects_chain.0.iter() {
                commands.entity(*effect).remove::<ParamFollower>();
            }
        }
    }
}

/// Assign the default pool label to a sample player that has no label.
fn assign_default(
    samples: Query<
        Entity,
        (
            With<SamplePlayer>,
            Without<PoolLabelContainer>,
            Without<DynamicPoolRegistry>,
        ),
    >,
    mut commands: Commands,
) {
    for sample in samples.iter() {
        commands.entity(sample).insert(DefaultPool);
    }
}

/// A pool despawner command.
///
/// Despawn a sample pool, cleaning up its resources
/// in the ECS and audio graph.
///
/// Despawning the terminal volume node recursively
/// will produce the same effect.
///
/// This can be used directly or via the [`PoolCommands`] trait.
///
/// ```
/// # use bevy_ecs::prelude::*;
/// # use bevy_seedling::prelude::*;
/// #[derive(PoolLabel, Debug, Clone, PartialEq, Eq, Hash)]
/// struct MyLabel;
///
/// fn system(mut commands: Commands) {
///     commands.queue(PoolDespawn::new(MyLabel));
/// }
/// ```
#[derive(Debug)]
pub struct PoolDespawn<T>(T);

impl<T: PoolLabel + Component> PoolDespawn<T> {
    /// Construct a new [`PoolDespawn`] with the provided label.
    pub fn new(label: T) -> Self {
        Self(label)
    }
}

impl<T: PoolLabel + Component> Command for PoolDespawn<T> {
    fn apply(self, world: &mut World) {
        let mut roots =
            world.query_filtered::<(Entity, &PoolLabelContainer), (With<T>, With<SamplePoolNode>, With<VolumeNode>)>();

        let roots: Vec<_> = roots
            .iter(world)
            .map(|(root, label)| (root, label.clone()))
            .collect();

        let mut commands = world.commands();

        let interned = self.0.intern();
        for (root, label) in roots {
            if label.label == interned {
                commands.entity(root).despawn_recursive();
            }
        }
    }
}

/// Provides methods on [`Commands`] to manage sample pools.
pub trait PoolCommands {
    /// Despawn a sample pool, cleaning up its resources
    /// in the ECS and audio graph.
    ///
    /// Despawning the terminal volume node recursively
    /// will produce the same effect.
    fn despawn_pool<T: PoolLabel + Component>(&mut self, label: T);
}

impl PoolCommands for Commands<'_, '_> {
    fn despawn_pool<T: PoolLabel + Component>(&mut self, label: T) {
        self.queue(PoolDespawn::new(label));
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{pool::NodeRank, prelude::*, profiling::ProfilingBackend};
    use bevy::prelude::*;
    use bevy_ecs::system::RunSystemOnce;

    fn prepare_app<F: IntoSystem<(), (), M>, M>(startup: F) -> App {
        let mut app = App::new();

        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            SeedlingPlugin::<ProfilingBackend> {
                default_pool_size: None,
                ..SeedlingPlugin::<ProfilingBackend>::new()
            },
            HierarchyPlugin,
        ))
        .add_systems(Startup, startup);

        app.finish();
        app.cleanup();
        app.update();

        app
    }

    fn run<F: IntoSystem<(), O, M>, O, M>(app: &mut App, system: F) -> O {
        let world = app.world_mut();
        world.run_system_once(system).unwrap()
    }

    #[test]
    fn test_despawn_static() {
        #[derive(PoolLabel, Clone, Debug, PartialEq, Eq, Hash)]
        struct TestPool;

        let mut app = prepare_app(|mut commands: Commands| {
            Pool::new(TestPool, 4)
                .effect(LowPassNode::default())
                .spawn(&mut commands);
        });

        run(&mut app, |pool_nodes: Query<&FirewheelNode>| {
            // 2 * 4 (sampler and low pass nodes) + 1 (pool volume) + 1 (global volume)
            assert_eq!(pool_nodes.iter().count(), 10);
        });

        run(&mut app, |mut commands: Commands| {
            commands.despawn_pool(TestPool);
        });

        app.update();

        run(&mut app, |pool_nodes: Query<&FirewheelNode>| {
            // 1 (global volume)
            assert_eq!(pool_nodes.iter().count(), 1);
        });
    }

    #[test]
    fn test_despawn_dynamic() {
        let mut app = prepare_app(|mut commands: Commands, server: Res<AssetServer>| {
            commands
                .spawn(SamplePlayer::new(server.load("caw.ogg")))
                .effect(LowPassNode::default());
        });

        run(&mut app, |pool_nodes: Query<&FirewheelNode>| {
            // 2 * 4 (sampler and low pass nodes) + 1 (pool volume) + 1 (global volume)
            assert_eq!(pool_nodes.iter().count(), 10);
        });

        run(
            &mut app,
            |q: Query<Entity, With<NodeRank>>, mut commands: Commands| {
                let pool = q.single();

                commands.entity(pool).despawn();
            },
        );

        app.update();

        run(&mut app, |pool_nodes: Query<&FirewheelNode>| {
            // 1 (global volume)
            assert_eq!(pool_nodes.iter().count(), 1);
        });
    }

    #[derive(Component)]
    struct EmptyComponent;

    #[test]
    fn test_remove_in_dynamic() {
        let mut app = prepare_app(|mut commands: Commands, server: Res<AssetServer>| {
            // We'll play a short sample
            commands
                .spawn((
                    SamplePlayer::new(server.load("sine_440hz_1ms.wav")),
                    EmptyComponent,
                    PlaybackSettings::REMOVE,
                ))
                .effect(LowPassNode::default());
        });

        // Then wait until the sample player is removed.
        loop {
            let players = run(
                &mut app,
                |q: Query<Entity, (With<SamplePlayer>, With<EmptyComponent>)>| q.iter().len(),
            );

            if players == 0 {
                break;
            }

            app.update();
        }

        // Once removed, we'll verify that _all_ audio-related components are removed.
        let world = app.world_mut();
        let mut q = world.query_filtered::<EntityRef, With<EmptyComponent>>();
        let entity = q.single(world);

        let archetype = entity.archetype();

        assert_eq!(archetype.components().count(), 1);
        assert!(entity.contains::<EmptyComponent>());
    }

    #[test]
    fn test_remove_in_pool() {
        #[derive(PoolLabel, Debug, Clone, PartialEq, Eq, Hash)]
        struct BespokeLabel;

        let mut app = prepare_app(|mut commands: Commands, server: Res<AssetServer>| {
            Pool::new(BespokeLabel, 4)
                .effect(LowPassNode::default())
                .spawn(&mut commands);

            commands.spawn((
                BespokeLabel,
                SamplePlayer::new(server.load("sine_440hz_1ms.wav")),
                EmptyComponent,
                PlaybackSettings::REMOVE,
            ));
        });

        // Then wait until the sample player is removed.
        loop {
            let players = run(
                &mut app,
                |q: Query<Entity, (With<SamplePlayer>, With<EmptyComponent>)>| q.iter().len(),
            );

            if players == 0 {
                break;
            }

            app.update();
        }

        // Once removed, we'll verify that _all_ audio-related components are removed.
        let world = app.world_mut();
        let mut q = world.query_filtered::<EntityRef, With<EmptyComponent>>();
        let entity = q.single(world);

        let archetype = entity.archetype();

        assert_eq!(archetype.components().count(), 1);
        assert!(entity.contains::<EmptyComponent>());
    }
}
