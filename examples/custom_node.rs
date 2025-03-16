//! This example demonstrates how to define and use a custom
//! Firehwel node.

use bevy::prelude::*;
use bevy_seedling::prelude::*;

// You'll need to depend on firewheel directly when defining
// custom nodes.
use firewheel::{
    channel_config::{ChannelConfig, NonZeroChannelCount},
    diff::{Diff, Patch},
    event::NodeEventList,
    node::{
        AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, ProcBuffers,
        ProcInfo, ProcessStatus,
    },
    Volume,
};

// A Firehwel node typically contains your audio
// processor's parameters. Firewheel's `Diff` and
// `Patch` traits allows this struct to send
// realtime-safe messages from the ECS to the
// audio thread.
#[derive(Diff, Patch, Debug, Clone, Component)]
pub struct CustomVolumeNode {
    // The volume we'll apply during audio processing.
    pub volume: Volume,
}

// Most nodes with have a configuration struct,
// which allows users to define additional parameters
// that are only required once during construction.
#[derive(Debug, Component, Clone)]
pub struct VolumeConfig {
    pub channels: NonZeroChannelCount,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self {
            // Stereo is a good default.
            channels: NonZeroChannelCount::STEREO,
        }
    }
}

impl AudioNode for CustomVolumeNode {
    // Here we specify the configuration.
    //
    // Even if no configuration is required, `bevy_seedling` will
    // expect this to implement `Component`. You should generally reach for
    // Firehweel's `EmptyConfig` type in such a scenario.
    type Configuration = VolumeConfig;

    fn info(&self, config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("custom volume")
            .channel_config(ChannelConfig {
                num_inputs: config.channels.get(),
                num_outputs: config.channels.get(),
            })
            .uses_events(true)
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        VolumeProcessor {
            params: self.clone(),
        }
    }
}

// You'll typically define a separate type for
// your audio processor calculations.
struct VolumeProcessor {
    // Here we keep a copy of the volume parameters to
    // receive patches from the ECS.
    params: CustomVolumeNode,
}

impl AudioNodeProcessor for VolumeProcessor {
    fn process(
        &mut self,
        ProcBuffers {
            inputs, outputs, ..
        }: ProcBuffers,
        proc_info: &ProcInfo,
        events: NodeEventList,
    ) -> ProcessStatus {
        // This will iterator over this node's events,
        // applying any patches sent from the ECS in a
        // realtime-safe way.
        self.params.patch_list(events);

        // Firewheel will inform you if an input channel is silent. If they're
        // all silent, we can simply skip processing and save CPU time.
        if proc_info.in_silence_mask.all_channels_silent(inputs.len()) {
            // All inputs are silent.
            return ProcessStatus::ClearAllOutputs;
        }

        // We only need to calculate this once.
        let gain = self.params.volume.amp();

        // Here we simply iterate over all samples in every channel and
        // apply out volume. Firewheel's nodes typically utilize more
        // optimization, but a node written like this should work well
        // in most scenarios.
        for (input, output) in inputs.iter().zip(outputs.iter_mut()) {
            for (input_sample, output_sample) in input.iter().zip(output.iter_mut()) {
                *output_sample = *input_sample * gain;
            }
        }

        ProcessStatus::outputs_not_silent()
    }
}

// Now, let's use this node in the ECS!
fn main() {
    App::new()
        .add_plugins((
            MinimalPlugins,
            bevy_log::LogPlugin::default(),
            AssetPlugin::default(),
            SeedlingPlugin {
                // We'll manually spawn the default pool so
                // we can chain our volume node.
                default_pool_size: None,
                ..Default::default()
            },
        ))
        // All you need to do to register your node is call
        // `RegisterNode::register_node`. This will automatically
        // handle parameter diffing, node connections, and audio
        // graph management.
        .register_node::<CustomVolumeNode>()
        .add_systems(Startup, startup)
        .add_systems(Update, update)
        .run();
}

fn startup(server: Res<AssetServer>, mut commands: Commands) {
    // Here we manually define the default sample pool
    // and connect it to our custom volume node.
    Pool::new(DefaultPool, 16)
        .spawn(&mut commands)
        .chain_node(CustomVolumeNode {
            volume: Volume::UNITY_GAIN,
        });

    // Let's spawn a looping sample.
    commands.spawn((
        SamplePlayer::new(server.load("snd_wobbler.wav")),
        PlaybackSettings::LOOP,
    ));
}

// Here we'll see how simply mutating the parameters
// will be automatically synchronized with the audio processor.
fn update(custom_node: Single<&mut CustomVolumeNode>, time: Res<Time>, mut angle: Local<f32>) {
    let mut custom_node = custom_node.into_inner();

    custom_node.volume = Volume::Linear(angle.cos() * 0.25 + 0.5);

    let period = 5.0;
    *angle += time.delta().as_secs_f32() * core::f32::consts::TAU / period;
}
