//! The audio-thread half of a hosted plugin.

use clack_host::events::event_types::ParamValueEvent;
use clack_host::events::spaces::CoreEventSpace;
use clack_host::prelude::*;
use clack_host::process::PluginAudioProcessor;
use confluence_core::mailbox::{Receiver, Sender};
use confluence_core::processor::{BusIo, ProcessError, Processor};

use crate::host::Host;

/// Parameter changes taken from the ring per block (the rest wait a block).
pub(crate) const EVENTS_PER_BLOCK: usize = 256;

/// A plugin running on an insert bus. Every buffer is allocated when it is
/// created (on the plugin thread); `process` only copies and calls the plugin.
pub(crate) struct ClapProcessor {
    /// The plugin's id on the plugin thread, to deactivate it there.
    pub plugin: u64,
    pub audio: Option<PluginAudioProcessor<Host>>,
    /// Per input port, per channel, `max_frames` samples.
    inputs: Vec<Vec<Vec<f32>>>,
    outputs: Vec<Vec<Vec<f32>>>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    events_in: EventBuffer,
    events_out: EventBuffer,
    params: Receiver<(u32, f64)>,
    reported: Sender<(u32, f64)>,
    max_frames: usize,
    steady: u64,
}

impl ClapProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        plugin: u64,
        audio: PluginAudioProcessor<Host>,
        in_channels: &[u32],
        out_channels: &[u32],
        max_frames: usize,
        params: Receiver<(u32, f64)>,
        reported: Sender<(u32, f64)>,
    ) -> Self {
        let bufs = |counts: &[u32]| -> Vec<Vec<Vec<f32>>> {
            counts.iter().map(|&n| (0..n).map(|_| vec![0.0; max_frames]).collect()).collect()
        };
        let total = |counts: &[u32]| counts.iter().map(|&n| n as usize).sum::<usize>();
        ClapProcessor {
            plugin,
            audio: Some(audio),
            inputs: bufs(in_channels),
            outputs: bufs(out_channels),
            in_ports: AudioPorts::with_capacity(total(in_channels), in_channels.len()),
            out_ports: AudioPorts::with_capacity(total(out_channels), out_channels.len()),
            events_in: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
            events_out: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
            params,
            reported,
            max_frames,
            steady: 0,
        }
    }
}

impl Processor for ClapProcessor {
    fn process(&mut self, mut io: BusIo<'_>) -> Result<(), ProcessError> {
        let frames = io.frames().min(self.max_frames);
        let Self { audio, inputs, outputs, in_ports, out_ports, events_in, events_out, params, reported, .. } = self;
        let Some(audio) = audio.as_mut() else { return Err(ProcessError) };

        // Bus sends → the plugin's first input port; anything else is silent.
        for (p, port) in inputs.iter_mut().enumerate() {
            for (c, ch) in port.iter_mut().enumerate() {
                let dst = &mut ch[..frames];
                if p == 0 && c < io.channels() {
                    dst.copy_from_slice(&io.send(c)[..frames]);
                } else {
                    dst.fill(0.0);
                }
            }
        }

        events_in.clear();
        for _ in 0..EVENTS_PER_BLOCK {
            let Some((id, value)) = params.try_recv() else { break };
            events_in.push(&ParamValueEvent::new(0, ClapId::new(id), Pckn::match_all(), value));
        }
        events_out.clear();

        let started = audio.ensure_processing_started().map_err(|_| ProcessError)?;
        let ins = in_ports.with_input_buffers(inputs.iter_mut().map(|port| AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                port.iter_mut().map(|ch| InputChannel::variable(&mut ch[..frames])),
            ),
        }));
        let mut outs = out_ports.with_output_buffers(outputs.iter_mut().map(|port| AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(port.iter_mut().map(|ch| &mut ch[..frames])),
        }));
        let result = started.process(
            &ins,
            &mut outs,
            &InputEvents::from_buffer(events_in),
            &mut OutputEvents::from_buffer(events_out),
            Some(self.steady),
            None,
        );
        self.steady += frames as u64;
        result.map_err(|_| ProcessError)?;

        // The plugin's first output port → the bus returns; returns past it are silent.
        let first = outputs.first();
        for c in 0..io.channels() {
            let ret = io.ret(c);
            match first.and_then(|port| port.get(c)) {
                Some(ch) => ret[..frames].copy_from_slice(&ch[..frames]),
                None => ret.fill(0.0),
            }
        }

        for e in events_out.iter() {
            if let Some(CoreEventSpace::ParamValue(v)) = e.as_core_event() {
                if let Some(id) = v.param_id() {
                    // A full ring drops the report; the next one carries the newer value.
                    let _ = reported.try_send((id.get(), v.value()));
                }
            }
        }
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(audio) = self.audio.as_mut() {
            audio.ensure_processing_stopped();
        }
    }
}
