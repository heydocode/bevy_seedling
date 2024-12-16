//! A sprouting implementation of the [Firewheel](https://github.com/BillyDM/firewheel) audio engine for Bevy.

#![allow(clippy::type_complexity)]

extern crate self as bevy_seedling;

use bevy_app::{Last, Plugin, Startup};
use bevy_asset::AssetApp;
use bevy_ecs::prelude::*;
use firewheel::FirewheelConfig;

pub mod context;
pub mod label;
pub mod node;
pub mod sample;
pub mod volume;

pub use context::AudioContext;
pub use label::{MainBus, NodeLabel};
use node::RegisterNode;
pub use node::{ConnectNode, ConnectTarget, Node};

pub use seedling_macros::{AudioParam, NodeLabel};

/// Sets for all `bevy_seedling` systems.
///
/// These are all inserted into the [Last] schedule.
#[derive(Debug, SystemSet, PartialEq, Eq, Hash, Clone)]
pub enum SeedlingSystems {
    /// Entities without audio nodes acquire them from the audio context.
    Acquire,
    /// Pending connections are made.
    Connect,
    /// Queue audio engine events.
    ///
    /// While it's not strictly necessary to separate this
    /// set from [SeedlingSystems::Connect], it's a nice
    /// semantic divide.
    Queue,
    /// The audio context is updated and flushed.
    Flush,
}

#[derive(Default)]
pub struct SeedlingPlugin {
    pub settings: FirewheelConfig,
}

impl Plugin for SeedlingPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        let mut context = AudioContext::new(self.settings);
        let sample_rate = context.with(|ctx| ctx.stream_info().unwrap().sample_rate);

        app.insert_resource(context)
            .init_resource::<node::ParamSystems>()
            .configure_sets(
                Last,
                (
                    SeedlingSystems::Connect.after(SeedlingSystems::Acquire),
                    SeedlingSystems::Queue.after(SeedlingSystems::Acquire),
                    SeedlingSystems::Flush
                        .after(SeedlingSystems::Connect)
                        .after(SeedlingSystems::Queue),
                ),
            )
            .init_resource::<node::NodeMap>()
            .init_resource::<node::PendingRemovals>()
            .register_asset_loader(sample::SampleLoader { sample_rate })
            .init_asset::<sample::Sample>()
            .register_node::<sample::SamplePlayer>()
            .register_node::<volume::Volume>()
            .add_systems(Startup, label::insert_main_bus)
            .add_systems(
                Last,
                (
                    sample::on_add.in_set(SeedlingSystems::Acquire),
                    node::auto_connect
                        .before(SeedlingSystems::Connect)
                        .after(SeedlingSystems::Acquire),
                    node::process_connections.in_set(SeedlingSystems::Connect),
                    sample::trigger_pending_samples.in_set(SeedlingSystems::Queue),
                    (
                        node::process_removals,
                        node::flush_events,
                        context::update_context,
                    )
                        .chain()
                        .in_set(SeedlingSystems::Flush),
                ),
            );
    }
}
