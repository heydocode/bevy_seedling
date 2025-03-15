//! Audio node registration and management.

use crate::connect::NodeMap;
use crate::{prelude::AudioContext, SeedlingSystems};
use bevy_app::Last;
use bevy_ecs::{prelude::*, world::DeferredWorld};
use bevy_log::error;
use firewheel::diff::PathBuilder;
use firewheel::{
    diff::{Diff, Patch},
    event::{NodeEvent, NodeEventType},
    node::{AudioNode, NodeID},
};

pub mod label;

use label::NodeLabels;

/// A node's baseline instance.
///
/// This is used as the baseline for diffing.
#[derive(Component)]
struct Baseline<T>(pub(crate) T);

/// An event queue.
///
/// When inserted into an entity that contains a [FirewheelNode],
/// these events will automatically be drained and sent
/// to the audio context in the [SeedlingSystems::Flush] set.
#[derive(Component, Default)]
pub struct Events(Vec<NodeEventType>);

// Not ideal, but we're waiting for Firewheel to implement debug.
impl core::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries((0..self.0.len()).map(|_| ()))
            .finish()
    }
}

impl Events {
    /// Push a new event.
    pub fn push(&mut self, event: NodeEventType) {
        self.0.push(event);
    }

    /// Push a custom event.
    ///
    /// `value` is boxed and wrapped in [NodeEventType::Custom].
    pub fn push_custom<T: Send + Sync + 'static>(&mut self, value: T) {
        self.0.push(NodeEventType::Custom(Box::new(value)));
    }
}

fn generate_param_events<T: Diff + Patch + Component + Clone>(
    mut nodes: Query<(&T, &mut Baseline<T>, &mut Events), (Changed<T>, Without<ExcludeNode>)>,
) {
    for (params, mut baseline, mut events) in nodes.iter_mut() {
        params.diff(&baseline.0, Default::default(), &mut events.0);

        // Patch the baseline.
        for event in &events.0 {
            baseline.0.patch_event(event);
        }
    }
}

fn acquire_id<T>(
    q: Query<
        (Entity, &T, Option<&T::Configuration>, Option<&NodeLabels>),
        (Without<FirewheelNode>, Without<ExcludeNode>),
    >,
    mut context: ResMut<AudioContext>,
    mut commands: Commands,
    mut node_map: ResMut<NodeMap>,
) where
    T: AudioNode<Configuration: Component + Clone> + Component + Clone,
{
    context.with(|context| {
        for (entity, container, config, labels) in q.iter() {
            let node = context.add_node(container.clone(), config.cloned());

            for label in labels.iter().flat_map(|l| l.iter()) {
                node_map.insert(*label, entity);
            }

            commands.entity(entity).insert(FirewheelNode(node));
        }
    });
}

/// Register audio nodes in the ECS.
pub trait RegisterNode {
    /// Register an audio node with automatic diffing.
    ///
    /// This will allow audio entities to automatically
    /// acquire IDs from the audio graph and perform
    /// parameter diffing.
    fn register_node<T>(&mut self) -> &mut Self
    where
        T: AudioNode<Configuration: Component + Clone> + Diff + Patch + Component + Clone;

    /// Register an audio node without automatic diffing.
    ///
    /// This will allow audio entities to automatically
    /// acquire IDs from the audio graph and perform
    /// parameter diffing.
    fn register_simple_node<T>(&mut self) -> &mut Self
    where
        T: AudioNode<Configuration: Component + Clone> + Component + Clone;
}

impl RegisterNode for bevy_app::App {
    fn register_node<T>(&mut self) -> &mut Self
    where
        T: AudioNode<Configuration: Component + Clone> + Diff + Patch + Component + Clone,
    {
        let world = self.world_mut();

        world.register_component_hooks::<T>().on_insert(
            |mut world: DeferredWorld, entity: Entity, _| {
                let value = world.get::<T>(entity).unwrap().clone();
                world.commands().entity(entity).insert(Baseline(value));
            },
        );
        world.register_required_components::<T, Events>();
        world.register_required_components::<T, T::Configuration>();

        self.add_systems(
            Last,
            (
                acquire_id::<T>.in_set(SeedlingSystems::Acquire),
                (param_follower::<T>, generate_param_events::<T>)
                    .chain()
                    .in_set(SeedlingSystems::Queue),
            ),
        )
    }

    fn register_simple_node<T>(&mut self) -> &mut Self
    where
        T: AudioNode<Configuration: Component + Clone> + Component + Clone,
    {
        let world = self.world_mut();
        world.register_required_components::<T, Events>();
        world.register_required_components::<T, T::Configuration>();

        self.add_systems(Last, acquire_id::<T>.in_set(SeedlingSystems::Acquire))
    }
}

/// An ECS handle for an audio node.
///
/// [`FirewheelNode`] may not necessarily be available immediately
/// upon spawning audio entities; [`FirewheelNode`]s are acquired
/// during the [`SeedlingSystems::Acquire`] set. Node
/// acquisition will also be deferred if the audio context
/// is disabled.
///
/// When this component is removed, the underlying
/// audio node is removed from the graph.
#[derive(Debug, Clone, Copy)]
pub struct FirewheelNode(pub NodeID);

impl Component for FirewheelNode {
    const STORAGE_TYPE: bevy_ecs::component::StorageType = bevy_ecs::component::StorageType::Table;

    fn register_component_hooks(hooks: &mut bevy_ecs::component::ComponentHooks) {
        hooks.on_remove(|mut world, entity, _| {
            let Some(node) = world.get::<FirewheelNode>(entity).copied() else {
                return;
            };

            let mut removals = world.resource_mut::<PendingRemovals>();
            removals.push(node.0);
        });
    }
}

/// Queued audio node removals.
///
/// This resource allows us to defer audio node removals
/// until the audio graph is ready.
#[derive(Debug, Default, Resource)]
pub(crate) struct PendingRemovals(Vec<NodeID>);

impl PendingRemovals {
    pub fn push(&mut self, node: NodeID) {
        self.0.push(node);
    }
}

pub(crate) fn process_removals(
    mut removals: ResMut<PendingRemovals>,
    mut context: ResMut<AudioContext>,
) {
    context.with(|context| {
        for node in removals.0.drain(..) {
            if context.remove_node(node).is_err() {
                error!("attempted to remove non-existent or invalid node from audio graph");
            }
        }
    });
}

pub(crate) fn flush_events(
    mut nodes: Query<(&FirewheelNode, &mut Events)>,
    mut context: ResMut<AudioContext>,
) {
    context.with(|context| {
        for (node, mut events) in nodes.iter_mut() {
            for event in events.0.drain(..) {
                context.queue_event(NodeEvent {
                    node_id: node.0,
                    event,
                });
            }
        }
    });
}

/// Exclude a node from the audio graph.
///
/// This component prevents audio node components
/// like [`VolumeNode`][crate::prelude::VolumeNode] from
/// automatically inserting themselves into the audio graph.
/// This allows you to treat nodes as plain old data,
/// facilitating the [`ParamFollower`] pattern.
///
/// ```
/// # use bevy_ecs::prelude::*;
/// # use bevy_seedling::{prelude::*, node::{ExcludeNode, ParamFollower}};
/// fn system(mut commands: Commands) {
///     let pod = commands.spawn((
///         VolumeNode { volume: Volume::UNITY_GAIN },
///         ExcludeNode,
///     )).head();
///
///     // This node will be inserted into the graph,
///     // and the volume will track any changes
///     // made to the `pod` entity.
///     commands.spawn((
///         VolumeNode { volume: Volume::UNITY_GAIN },
///         ParamFollower(pod),
///     ));
/// }
/// ```
#[derive(Debug, Default, Component)]
pub struct ExcludeNode;

/// A component that allows one entity's parameters to track another's.
///
/// This can only support a single rank; cascading
/// is not allowed.
///
/// Within `bevy_seedling`, this is used primarily by sampler
/// pools. When you define a pool with a set of effects,
/// those nodes will be automatically inserted on
/// [`SamplePlayer`][crate::prelude::SamplePlayer] entities
/// queued for that pool. Then, each effect node will
/// have a [`ParamFollower`] component inserted that
/// tracks the [`SamplePlayer`][crate::prelude::SamplePlayer].
///
/// ```
/// # use bevy::prelude::*;
/// # use bevy_seedling::prelude::*;
/// # #[derive(PoolLabel, Clone, Debug, PartialEq, Eq, Hash)]
/// # struct MyLabel;
/// # fn system(mut commands: Commands, server: Res<AssetServer>) {
/// Pool::new(MyLabel, 1)
///     .effect(SpatialBasicNode::default())
///     .spawn(&mut commands);
///
/// commands.spawn((MyLabel, SamplePlayer::new(server.load("my_sample.wav"))));
///
/// // Once spawned, these will look something like
/// // Pool: (SamplePlayer) -> (SpatialBasicNode, ParamFollower) -> (VolumeNode) -> (MainBus)
/// // SamplePlayer: (SamplePlayer, SpatialBasicNode, ExcludeNode)
/// # }
/// ```
#[derive(Debug, Component)]
pub struct ParamFollower(pub Entity);

/// Apply diffing and patching between two sets of parameters
/// in the ECS. This allows the engine-connected parameters
/// to follow another set of parameters that may be
/// closer to user code.
///
/// For example, it's much easier for users to set parameters
/// on a sample player entity directly rather than drilling
/// into the sample pool and node the sample is assigned to.
pub(crate) fn param_follower<T: Diff + Patch + Component>(
    sources: Query<&T, (Changed<T>, Without<ParamFollower>)>,
    mut followers: Query<(&ParamFollower, &mut T)>,
) {
    let mut event_queue = Vec::new();
    for (follower, mut params) in followers.iter_mut() {
        let Ok(source) = sources.get(follower.0) else {
            continue;
        };

        source.diff(&params, PathBuilder::default(), &mut event_queue);

        for event in event_queue.drain(..) {
            params.patch_event(&event);
        }
    }
}
