//! The built-in plugins and their registry.

pub mod drums;
pub mod dynamics;
pub mod eq;
pub mod harmonic;
pub mod modulation;
pub mod pitch;
pub mod reverb;
pub mod sampler;
pub mod strip;
pub mod synth;
pub mod utility;

use crate::{Plugin, PluginInfo};

// One table owns menu order and construction; IDs come from the descriptions themselves.
macro_rules! registry {
    ($($info:path => $make:expr),+ $(,)?) => {
        pub(crate) static REGISTRY: &[&PluginInfo] = &[$(&$info),+];

        pub(crate) fn instantiate(id: &str) -> Option<Box<dyn Plugin>> {
            $(if id == $info.id {
                return Some(Box::new($make));
            })+
            None
        }
    };
}

registry! {
    eq::EQ1_INFO => eq::Eq1::new(),
    eq::EQ7_INFO => eq::Eq7::new(),
    dynamics::COMP_INFO => dynamics::Compressor::new(),
    dynamics::GATE_INFO => dynamics::ExpanderGate::new(),
    dynamics::DEESS_INFO => dynamics::DeEsser::new(),
    dynamics::MAX_INFO => dynamics::Maximizer::new(),
    strip::STRIP_INFO => strip::ChannelStrip::new(),
    reverb::ROOM_INFO => reverb::Reverb::room(),
    reverb::PLATE_INFO => reverb::Reverb::plate(),
    modulation::DELAY_INFO => modulation::ModDelay::new(),
    modulation::CHORUS_INFO => modulation::ModFx::chorus(),
    modulation::FLANGER_INFO => modulation::ModFx::flanger(),
    modulation::PHASER_INFO => modulation::Phaser::new(),
    harmonic::SAT_INFO => harmonic::Saturator::new(),
    harmonic::LOFI_INFO => harmonic::LoFi::new(),
    harmonic::RECT_INFO => harmonic::Rectifier::new(),
    pitch::PITCH_INFO => pitch::PitchShifter::new(),
    utility::SHIFT_INFO => utility::TimeShift::new(),
    utility::GAIN_INFO => utility::Gain::new(),
    utility::TRIM_INFO => utility::Trim::new(),
    utility::INVERT_INFO => utility::Invert::default(),
    utility::DC_INFO => utility::DcRemoval::new(),
    utility::GEN_INFO => utility::SignalGenerator::new(),
    utility::DITHER_INFO => utility::Dither::new(),
    synth::SYNTH_INFO => synth::SubtractiveSynth::new(),
    drums::DRUM_INFO => drums::DrumSynth::new(),
    sampler::SAMPLER_INFO => sampler::Sampler::new(),
}
