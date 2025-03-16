use super::{SamplePoolDefaults, SamplePoolNode};
use crate::sample::{PlaybackSettings, QueuedSample, Sample, SamplePlayer};
use bevy_asset::Assets;
use bevy_ecs::{component::ComponentId, prelude::*, world::DeferredWorld};
use bevy_utils::{HashMap, HashSet};
use core::marker::PhantomData;
use firewheel::node::AudioNode;
use seedling_macros::PoolLabel;

#[derive(Component, Clone, Default, Debug, PartialEq, Eq, Hash)]
pub struct AutoPoolRegistry {
    effects: Vec<ComponentId>,
}

impl AutoPoolRegistry {
    pub fn insert(&mut self, value: ComponentId) -> bool {
        if !self.effects.iter().any(|v| *v == value) {
            self.effects.push(value);
            true
        } else {
            false
        }
    }
}

struct RegistryEntry {
    root: Entity,
    samples: HashSet<Entity>,
}

#[derive(Resource, Default)]
pub(super) struct Registries(HashMap<AutoPoolRegistry, RegistryEntry>);

// fn scan_registries(q: Query<(Entity, &AutoPoolRegistry)>, mut registries: ResMut<Registries>) {
//     for (entity, registry) in q.iter() {
//         match registries.0.get_mut(registry) {
//             Some(entry) => {
//                 entry.entities.insert(entity);
//             }
//             None => {
//                 registries
//                     .0
//                     .insert(registry.clone(), core::iter::once(entity).collect());
//             }
//         }
//     }
// }

#[derive(Resource, Clone, Debug)]
pub struct DynamicPoolRange(pub Option<core::ops::Range<usize>>);

#[derive(PoolLabel, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct DynamicPool(usize);

/// Scan through the set of pending sample players
/// and assign work to the most appropriate sampler node.
pub(super) fn update_auto_pools(
    queued_samples: Query<
        (
            Entity,
            &SamplePlayer,
            &PlaybackSettings,
            &AutoPoolRegistry,
            &SamplePoolDefaults,
        ),
        (With<QueuedSample>, Without<DynamicPool>),
    >,
    assets: Res<Assets<Sample>>,
    mut registries: ResMut<Registries>,
    mut commands: Commands,
    dynamic_range: Res<DynamicPoolRange>,
) {
    let Some(dynamic_range) = dynamic_range.0.clone() else {
        return;
    };

    for (sample, player, settings, registry, defaults) in queued_samples.iter() {
        let Some(asset) = assets.get(&player.0) else {
            continue;
        };

        match registries.0.get_mut(registry) {
            Some(entry) => {
                commands.entity(sample).insert(DynamicPool(0));
            }
            None => {
                let label = DynamicPool(0);

                let chain_spawner = {
                    let defaults = defaults.clone();

                    move |commands: &mut Commands| {
                        defaults
                            .0
                            .iter()
                            .map(|d| {
                                let mut commands = commands.spawn((label, SamplePoolNode));
                                d(&mut commands);

                                commands.id()
                            })
                            .collect()
                    }
                };

                // create the pool
                let pool =
                    super::spawn_pool(label, 4, chain_spawner, defaults.clone(), &mut commands);

                registries.0.insert(
                    registry.clone(),
                    RegistryEntry {
                        root: pool.id(),
                        samples: HashSet::default(),
                    },
                );

                commands.entity(sample).insert(DynamicPool(0));
            }
        }
    }
}

#[derive(Component)]
#[component(on_insert = Self::on_insert)]
pub(crate) struct AutoRegister<T: Component>(PhantomData<T>);

impl<T: Component> core::default::Default for AutoRegister<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T: Component> AutoRegister<T> {
    fn on_insert(mut world: DeferredWorld, entity: Entity, _: ComponentId) {
        let Some(id) = world.component_id::<T>() else {
            return;
        };

        let mut entity = world.entity_mut(entity);

        if let Some(mut pool) = entity.get_mut::<AutoPoolRegistry>() {
            pool.insert(id);
        }
    }
}

pub struct AutoPoolCommands<'a> {
    commands: EntityCommands<'a>,
}

impl core::fmt::Debug for AutoPoolCommands<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoPoolCommands").finish_non_exhaustive()
    }
}

pub trait AutoPool<'a> {
    type Output;

    fn effect<T: AudioNode + Component + Clone>(self, node: T) -> Self::Output;
}

impl<'a> AutoPool<'a> for EntityCommands<'a> {
    type Output = AutoPoolCommands<'a>;

    fn effect<T: AudioNode + Component + Clone>(mut self, node: T) -> Self::Output {
        let mut defaults = SamplePoolDefaults::default();

        defaults.push({
            let node = node.clone();
            move |commands: &mut EntityCommands| {
                commands.insert(node.clone());
            }
        });

        self.insert((AutoPoolRegistry::default(), defaults, node));

        AutoPoolCommands { commands: self }
    }
}

impl<'a> AutoPool<'a> for AutoPoolCommands<'a> {
    type Output = AutoPoolCommands<'a>;

    fn effect<T: AudioNode + Component + Clone>(mut self, node: T) -> Self::Output {
        self.commands
            .entry::<SamplePoolDefaults>()
            .or_default()
            .and_modify({
                let node = node.clone();
                |mut defaults| {
                    defaults.push({
                        move |commands: &mut EntityCommands| {
                            commands.insert(node.clone());
                        }
                    });
                }
            });
        self.commands.insert(node);

        self
    }
}

#[cfg(test)]
mod test {
    use crate::prelude::*;
    use bevy::prelude::*;

    fn auto(mut commands: Commands, server: Res<AssetServer>) {
        commands
            .spawn(SamplePlayer::new(server.load("my_sample.wav")))
            .auto_pool()
            .effect(LowPassNode::default());
    }
}
