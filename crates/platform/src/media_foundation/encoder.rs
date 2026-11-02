//! Windows Media Foundation hardware H.264 encoding through installed GPU drivers.
//!
//! Enumerating hardware encoder MFTs through the operating system lets Windows choose an
//! installed AMD, Intel, or NVIDIA implementation. Unsupported MFTs and settings decline to the
//! next registered encoder; encoding never silently reports success after a software fallback.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::gpu::{Gpu, ensure_com};
use super::mft::{self, MfApi};
use filmcraft_export::{
    BitrateMode, ColorSignal, EncodedPacket, EncoderFrame, ExportError, ExportSettings, FieldOrder, Format, H264Pass, H264Profile, HardwareEncoding,
    Result as ExportResult, VideoEncoder,
};
use filmcraft_isobmff::{AvcConfig, SampleEntry};
use filmcraft_time::FrameRate;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MF_E_ATTRIBUTENOTFOUND, MF_E_NO_EVENTS_AVAILABLE, MF_E_NO_MORE_TYPES, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_COMPRESSED, MF_MT_DEFAULT_STRIDE, MF_MT_FIXED_SIZE_SAMPLES,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFNominalRange_16_235, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface};
const HNS_PER_SECOND: i128 = 10_000_000;
const MAX_FRAME_DIMENSION: u32 = 8192;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SEQUENCE_HEADER_BYTES: u32 = 1024 * 1024;
const MAX_NAL_UNITS: usize = 4096;
const MAX_DRAINED_PACKETS: usize = 4096;
const MAX_ASYNC_EVENTS: usize = 4096;
const MAX_ASYNC_INPUT_REQUESTS: usize = 64;
const ASYNC_INPUT_TIMEOUT: Duration = Duration::from_secs(10);
const ASYNC_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

struct MfEncoder {
    api: &'static MfApi,
    transform: IMFTransform,
    name: String,
    width: u32,
    height: u32,
    rate: FrameRate,
    output_buffer_size: u32,
    provides_samples: bool,
    config: Option<AvcConfig>,
    signal: ColorSignal,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    nv12: Vec<u8>,
    output_index: u64,
    events: Option<IMFMediaEventGenerator>,
    input_requests: usize,
    _gpu: Option<Arc<Gpu>>,
}

// SAFETY: the encoder owns its MFT and event generator exclusively and uses them only through
// `&mut self`. Media Foundation transforms are free-threaded; the D3D device/manager is shared
// through the existing multithread-protected `Gpu` wrapper.
unsafe impl Send for MfEncoder {}

/// Register with the export factory list. This factory only takes progressive SDR H.264 exports
/// when the user has enabled Hardware Encoding and Windows supplies a compatible hardware MFT.
pub fn factory(format: Format, width: u32, height: u32, rate: FrameRate, settings: &ExportSettings) -> Option<ExportResult<Box<dyn VideoEncoder>>> {
    if format != Format::H264 || settings.hardware_encoding != HardwareEncoding::Auto {
        return None;
    }
    let decline = |why: &str| {
        log::info!("Media Foundation hardware H.264 encoding declined: {why}");
        filmcraft_export::note_hw_encode_declined();
        None
    };
    match MfEncoder::new(width, height, rate, settings) {
        Ok(encoder) => {
            log::info!("hardware H.264 encoding through Media Foundation: {}", encoder.name);
            filmcraft_export::note_hw_encode_session();
            Some(Ok(Box::new(encoder)))
        }
        Err(why) => decline(&why),
    }
}

impl MfEncoder {
    fn new(width: u32, height: u32, rate: FrameRate, settings: &ExportSettings) -> std::result::Result<Self, String> {
        validate_settings(width, height, rate, settings)?;
        ensure_com()?;
        let api = mft::api()?;
        let candidates = api.hardware_h264_encoders()?;
        if candidates.is_empty() {
            return Err("Windows did not enumerate a hardware encoder for NV12 to H.264".into());
        }
        let mut reasons = Vec::new();
        for candidate in candidates {
            match Self::configure(api, width, height, rate, settings, &candidate) {
                Ok(encoder) => return Ok(encoder),
                Err(why) => reasons.push(why),
            }
        }
        Err(reasons.join("; "))
    }

    fn configure(
        api: &'static MfApi,
        width: u32,
        height: u32,
        rate: FrameRate,
        settings: &ExportSettings,
        activate: &IMFActivate,
    ) -> std::result::Result<Self, String> {
        let name = mft::friendly_name(activate).unwrap_or_else(|| "hardware H.264 encoder MFT".into());
        // SAFETY: this is a plain COM activation call on a live IMFActivate.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.map_err(|e| format!("{name}: activation failed: {e}"))?;

        // SAFETY: plain COM calls on a live transform.
        unsafe {
            let attrs = transform.GetAttributes().map_err(|e| format!("{name}: no MFT attributes: {e}"))?;
            let asynchronous = optional_attribute_flag(&attrs, &MF_TRANSFORM_ASYNC, &name)?;
            let events = if asynchronous {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| format!("{name}: cannot unlock asynchronous MFT: {e}"))?;
                Some(transform.cast::<IMFMediaEventGenerator>().map_err(|e| format!("{name}: no asynchronous event queue: {e}"))?)
            } else {
                None
            };
            let gpu = if optional_attribute_flag(&attrs, &MF_SA_D3D11_AWARE, &name)? {
                let gpu = Gpu::shared(api).map_err(|e| format!("{name}: no Direct3D device manager: {e}"))?;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager_param())
                    .map_err(|e| format!("{name}: refused the Direct3D device manager: {e}"))?;
                Some(gpu)
            } else {
                None
            };

            let input = api.media_type()?;
            set_video_type(&input, MFVideoFormat_NV12, width, height, rate, settings)?;
            let output = match transform.SetInputType(0, &input, 0) {
                Ok(()) => {
                    let output = choose_output_type(&transform, width, height, rate, settings, &name)?;
                    transform.SetOutputType(0, &output, 0).map_err(|e| format!("{name}: refused H.264 output: {e}"))?;
                    output
                }
                Err(input_error) => {
                    let output = choose_output_type(&transform, width, height, rate, settings, &name)
                        .map_err(|e| format!("{name}: input-first negotiation failed ({input_error}); {e}"))?;
                    transform
                        .SetOutputType(0, &output, 0)
                        .map_err(|e| format!("{name}: input-first negotiation failed ({input_error}); output-first negotiation failed: {e}"))?;
                    transform
                        .SetInputType(0, &input, 0)
                        .map_err(|e| format!("{name}: refused NV12 input after setting H.264 output (input-first error: {input_error}): {e}"))?;
                    transform
                        .SetOutputType(0, &output, 0)
                        .map_err(|e| format!("{name}: refused H.264 output after NV12 input (input-first error: {input_error}): {e}"))?;
                    output
                }
            };

            let info = transform.GetOutputStreamInfo(0).map_err(|e| format!("{name}: no output stream info: {e}"))?;
            let stream_flags = info.dwFlags as i32;
            let provides_samples = stream_flags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 != 0;
            let output_buffer_size = if provides_samples { 0 } else { info.cbSize };
            if !provides_samples && (output_buffer_size == 0 || output_buffer_size as usize > MAX_OUTPUT_BYTES) {
                return Err(format!("{name}: invalid output buffer size {}", info.cbSize));
            }

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| format!("{name}: begin-streaming failed: {e}"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| format!("{name}: start-of-stream failed: {e}"))?;

            let config = sequence_config(&output);
            Ok(Self {
                api,
                transform,
                name,
                width,
                height,
                rate,
                output_buffer_size,
                provides_samples,
                config,
                signal: settings.signal,
                y: Vec::new(),
                u: Vec::new(),
                v: Vec::new(),
                nv12: Vec::new(),
                output_index: 0,
                events,
                input_requests: 0,
                _gpu: gpu,
            })
        }
    }

    fn output_sample(&self) -> std::result::Result<IMFSample, String> {
        self.api.output_sample(self.output_buffer_size)
    }

    fn poll_output(&mut self, drain: bool) -> std::result::Result<Vec<EncodedPacket>, String> {
        let mut packets = Vec::new();
        let limit = if drain { MAX_DRAINED_PACKETS } else { 64 };
        for _ in 0..limit {
            let provided = if self.provides_samples { None } else { Some(self.output_sample()?) };
            let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(provided), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            // SAFETY: one output descriptor and a live status pointer; any returned COM references
            // are taken exactly once immediately after ProcessOutput, including on an error path.
            let result = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
            // SAFETY: `out` was initialized above, and MFT outputs transfer ownership of the
            // sample/event references through these fields.
            let (sample, _events) = unsafe { (ManuallyDrop::take(&mut out[0].pSample), ManuallyDrop::take(&mut out[0].pEvents)) };
            match result {
                Ok(()) => {
                    let sample = sample.ok_or_else(|| format!("{}: produced no encoded sample", self.name))?;
                    if let Some(packet) = self.packet_from_sample(&sample)? {
                        packets.push(packet);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(packets),
                Err(e) => return Err(format!("{}: ProcessOutput failed: {e}", self.name)),
            }
        }
        Err(format!("{}: exceeded the bounded output-packet limit", self.name))
    }

    fn async_event(&self) -> std::result::Result<Option<u32>, String> {
        let Some(events) = self.events.as_ref() else { return Ok(None) };
        // SAFETY: GetEvent is called on a live event generator with NO_WAIT, so it cannot block.
        let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
            Err(error) => return Err(format!("{}: asynchronous MFT event failed: {error}", self.name)),
        };
        // SAFETY: these are plain COM queries on the live event returned above.
        let status = unsafe { event.GetStatus() }.map_err(|e| format!("{}: cannot read MFT event status: {e}", self.name))?;
        if status.is_err() {
            return Err(format!("{}: asynchronous MFT event reported failure ({status:?})", self.name));
        }
        // SAFETY: plain COM query on a live event.
        unsafe { event.GetType() }.map(Some).map_err(|e| format!("{}: cannot read MFT event type: {e}", self.name))
    }

    fn poll_async_output(&mut self) -> std::result::Result<Option<EncodedPacket>, String> {
        let provided = if self.provides_samples { None } else { Some(self.output_sample()?) };
        let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(provided), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        // SAFETY: exactly one ProcessOutput call is made for this METransformHaveOutput event.
        let result = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
        // SAFETY: the MFT transfers returned sample/event references through these fields.
        let (sample, _events) = unsafe { (ManuallyDrop::take(&mut out[0].pSample), ManuallyDrop::take(&mut out[0].pEvents)) };
        result.map_err(|e| format!("{}: asynchronous ProcessOutput failed: {e}", self.name))?;
        let sample = sample.ok_or_else(|| format!("{}: asynchronous MFT produced no encoded sample", self.name))?;
        self.packet_from_sample(&sample)
    }

    fn encode_async(&mut self, sample: &IMFSample) -> std::result::Result<Vec<EncodedPacket>, String> {
        let deadline = Instant::now().checked_add(ASYNC_INPUT_TIMEOUT).ok_or_else(|| format!("{}: asynchronous MFT deadline is out of range", self.name))?;
        let mut accepted = false;
        let mut packets = Vec::new();
        for _ in 0..MAX_ASYNC_EVENTS {
            if !accepted && self.input_requests > 0 {
                // SAFETY: ProcessInput follows an METransformNeedInput event; the sample and
                // transform remain live for the duration of this call.
                match unsafe { self.transform.ProcessInput(0, sample, 0) } {
                    Ok(()) => {
                        accepted = true;
                        self.input_requests = self.input_requests.saturating_sub(1);
                    }
                    Err(error) if error.code() == MF_E_NOTACCEPTING => self.input_requests = 0,
                    Err(error) => return Err(format!("{}: asynchronous ProcessInput failed: {error}", self.name)),
                }
            }

            match self.async_event()? {
                Some(kind) if kind == METransformNeedInput.0 as u32 => {
                    self.input_requests = self.input_requests.saturating_add(1).min(MAX_ASYNC_INPUT_REQUESTS);
                }
                Some(kind) if kind == METransformHaveOutput.0 as u32 => {
                    if let Some(packet) = self.poll_async_output()? {
                        packets.push(packet);
                    }
                }
                Some(_) => {}
                None if accepted => return Ok(packets),
                None if Instant::now() >= deadline => return Err(format!("{}: timed out waiting for an input request", self.name)),
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        Err(format!("{}: exceeded the bounded asynchronous event limit", self.name))
    }

    fn drain_async(&mut self) -> std::result::Result<Vec<EncodedPacket>, String> {
        let deadline = Instant::now().checked_add(ASYNC_DRAIN_TIMEOUT).ok_or_else(|| format!("{}: asynchronous drain deadline is out of range", self.name))?;
        let mut packets = Vec::new();
        for _ in 0..MAX_ASYNC_EVENTS {
            match self.async_event()? {
                Some(kind) if kind == METransformHaveOutput.0 as u32 => {
                    if let Some(packet) = self.poll_async_output()? {
                        packets.push(packet);
                    }
                }
                Some(kind) if kind == METransformDrainComplete.0 as u32 => {
                    return Ok(packets);
                }
                Some(_) => {}
                None if Instant::now() >= deadline => return Err(format!("{}: timed out draining asynchronous encoder", self.name)),
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        Err(format!("{}: exceeded the bounded asynchronous drain-event limit", self.name))
    }

    fn packet_from_sample(&mut self, sample: &IMFSample) -> std::result::Result<Option<EncodedPacket>, String> {
        let data = sample_bytes(sample, &self.name)?;
        let units = nal_units(&data).map_err(|e| format!("{}: invalid H.264 output: {e}", self.name))?;
        let mut sps = Vec::new();
        let mut pps = Vec::new();
        let mut packet_data = Vec::new();
        let mut key = false;
        for nal in units {
            let kind = nal.first().map(|b| b & 0x1f).ok_or_else(|| format!("{}: empty NAL unit", self.name))?;
            match kind {
                7 if nal.len() <= u16::MAX as usize => sps.push(nal.to_vec()),
                8 if nal.len() <= u16::MAX as usize => pps.push(nal.to_vec()),
                _ => {
                    key |= kind == 5;
                    let length = u32::try_from(nal.len()).map_err(|_| format!("{}: NAL unit is too large", self.name))?;
                    packet_data.extend_from_slice(&length.to_be_bytes());
                    packet_data.extend_from_slice(nal);
                }
            }
        }
        if !sps.is_empty() && !pps.is_empty() {
            self.config = Some(AvcConfig::new(sps, pps, 4));
        }
        if packet_data.is_empty() {
            return Ok(None);
        }
        if self.config.is_none() {
            return Err(format!("{}: H.264 output did not provide SPS and PPS", self.name));
        }

        // Media Foundation timestamps are in 100-ns units; MP4 track timestamps use the rational
        // frame-rate numerator as their timescale. The MFT's decode order supplies DTS.
        // SAFETY: GetSampleTime is a plain COM call on a live sample.
        let pts_hns = unsafe { sample.GetSampleTime() }.map_err(|e| format!("{}: encoded sample has no timestamp: {e}", self.name))?;
        let pts =
            i128::from(pts_hns).checked_mul(i128::from(self.rate.num)).ok_or_else(|| format!("{}: encoded timestamp overflows", self.name))? / HNS_PER_SECOND;
        let dts = i128::from(self.output_index).checked_mul(i128::from(self.rate.den)).ok_or_else(|| format!("{}: decode timestamp overflows", self.name))?;
        let offset = i32::try_from(pts.saturating_sub(dts)).unwrap_or(if pts < dts { i32::MIN } else { i32::MAX });
        self.output_index = self.output_index.saturating_add(1);
        filmcraft_export::note_hw_encode_frame();
        Ok(Some(EncodedPacket {
            data: packet_data,
            key,
            duration: u32::try_from(self.rate.den).map_err(|_| format!("{}: frame duration is out of range", self.name))?,
            composition_offset: offset,
        }))
    }
}

impl VideoEncoder for MfEncoder {
    fn sample_entry(&self) -> SampleEntry {
        let config = self.config.clone().unwrap_or_else(|| AvcConfig::new(Vec::new(), Vec::new(), 4));
        let (width, height) = (u16::try_from(self.width).unwrap_or(u16::MAX), u16::try_from(self.height).unwrap_or(u16::MAX));
        let mut entry = SampleEntry::avc(config, width, height);
        self.signal.apply_to(&mut entry, false);
        entry
    }

    fn timescale(&self) -> u32 {
        u32::try_from(self.rate.num).unwrap_or(u32::MAX)
    }

    fn encode(&mut self, frame: &EncoderFrame<'_>) -> ExportResult<Vec<EncodedPacket>> {
        ensure_com().map_err(ExportError::Encode)?;
        if frame.width != self.width || frame.height != self.height {
            return Err(ExportError::Encode(format!(
                "{}: received {}x{} frame for {}x{} encoder",
                self.name, frame.width, frame.height, self.width, self.height
            )));
        }
        let frame_data = nv12(frame, &mut self.y, &mut self.u, &mut self.v, &mut self.nv12)?;
        let time = frame_time_hns(frame.index, self.rate).map_err(ExportError::Encode)?;
        let duration = frame_time_hns(1, self.rate).map_err(ExportError::Encode)?;
        let sample = if let Some(gpu) = self._gpu.as_ref() {
            gpu.encoder_sample(self.api, frame_data, self.width, self.height, time).map_err(ExportError::Encode)?
        } else {
            self.api.sample_of(frame_data, time).map_err(ExportError::Encode)?
        };
        // SAFETY: plain COM call on a live sample with a positive frame duration.
        unsafe { sample.SetSampleDuration(duration) }.map_err(|e| ExportError::Encode(format!("{}: set sample duration: {e}", self.name)))?;

        if self.events.is_some() {
            return self.encode_async(&sample).map_err(ExportError::Encode);
        }

        let mut packets = Vec::new();
        // A transform can ask for more output before it will accept the next input. Drain once and
        // retry; if it still refuses, report the exact MFT error instead of spinning.
        // SAFETY: ProcessInput is a plain COM call on live interfaces.
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                packets.extend(self.poll_output(false).map_err(ExportError::Encode)?);
                // SAFETY: same live sample and transform; a second refusal is a surfaced error.
                unsafe { self.transform.ProcessInput(0, &sample, 0) }
                    .map_err(|e| ExportError::Encode(format!("{}: ProcessInput still refuses input: {e}", self.name)))?;
            }
            Err(e) => return Err(ExportError::Encode(format!("{}: ProcessInput failed: {e}", self.name))),
        }
        packets.extend(self.poll_output(false).map_err(ExportError::Encode)?);
        Ok(packets)
    }

    fn flush(&mut self) -> ExportResult<Vec<EncodedPacket>> {
        ensure_com().map_err(ExportError::Encode)?;
        // SAFETY: a plain COM call on a live transform.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.map_err(|e| ExportError::Encode(format!("{}: drain failed: {e}", self.name)))?;
        if self.events.is_some() {
            return self.drain_async().map_err(ExportError::Encode);
        }
        self.poll_output(true).map_err(ExportError::Encode)
    }
}

fn validate_settings(width: u32, height: u32, rate: FrameRate, settings: &ExportSettings) -> std::result::Result<(), String> {
    if width == 0 || height == 0 || width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(format!("unsupported NV12 frame size {width}x{height}"));
    }
    let rgba_bytes = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "frame size overflows".to_string())?;
    if rgba_bytes > MAX_FRAME_BYTES {
        return Err(format!("frame exceeds the {} MiB limit", MAX_FRAME_BYTES / (1024 * 1024)));
    }
    if rate.num <= 0 || rate.den <= 0 || u32::try_from(rate.num).is_err() || u32::try_from(rate.den).is_err() {
        return Err(format!("invalid frame rate {rate:?}"));
    }
    if settings.signal != ColorSignal::default() {
        return Err("HDR and non-default colour signals are not supported by this encoder".into());
    }
    if settings.field_order != FieldOrder::Progressive {
        return Err("interlaced output is not supported".into());
    }
    if settings.pixel_aspect.is_some_and(|aspect| aspect != (1, 1)) {
        return Err("non-square pixels are not supported".into());
    }
    if !matches!(settings.bitrate_mode, BitrateMode::Cbr | BitrateMode::Vbr1Pass) || !matches!(settings.h264_pass, H264Pass::Single) {
        return Err("only single-pass CBR and VBR are supported".into());
    }
    if settings.bitrate_kbps == 0 || settings.bitrate_kbps.checked_mul(1000).is_none() {
        return Err("invalid H.264 target bitrate".into());
    }
    Ok(())
}

fn optional_attribute_flag(attrs: &IMFAttributes, key: &GUID, name: &str) -> std::result::Result<bool, String> {
    // SAFETY: this reads a documented optional flag from the live MFT attribute store.
    match unsafe { attrs.GetUINT32(key) } {
        Ok(value) => Ok(value != 0),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(false),
        Err(error) => Err(format!("{name}: cannot read optional Media Foundation attribute: {error}")),
    }
}

fn set_video_type(
    media_type: &IMFMediaType,
    subtype: windows::core::GUID,
    width: u32,
    height: u32,
    rate: FrameRate,
    settings: &ExportSettings,
) -> std::result::Result<(), String> {
    let frame_size = (u64::from(width) << 32) | u64::from(height);
    let frame_rate = (u64::try_from(rate.num).map_err(|_| "invalid frame rate")? << 32) | u64::try_from(rate.den).map_err(|_| "invalid frame rate")?;
    let bitrate = settings.bitrate_kbps.checked_mul(1000).ok_or("H.264 bitrate overflows")?;
    // SAFETY: the media type is live and receives validated frame dimensions/rate and a bitrate
    // validated before transform enumeration.
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &windows::Win32::Media::MediaFoundation::MFMediaType_Video).map_err(|e| e.to_string())?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(|e| e.to_string())?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size).map_err(|e| e.to_string())?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, frame_rate).map_err(|e| e.to_string())?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(|e| e.to_string())?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
        media_type.SetUINT32(&MF_MT_COMPRESSED, u32::from(subtype != MFVideoFormat_NV12)).map_err(|e| e.to_string())?;
        if subtype == MFVideoFormat_NV12 {
            let sample_size = u64::from(width)
                .checked_mul(u64::from(height))
                .and_then(|luma| luma.checked_add(luma / 2))
                .and_then(|size| u32::try_from(size).ok())
                .ok_or("NV12 sample size overflows")?;
            media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width).map_err(|e| e.to_string())?;
            media_type.SetUINT32(&MF_MT_SAMPLE_SIZE, sample_size).map_err(|e| e.to_string())?;
            media_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1).map_err(|e| e.to_string())?;
            media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1).map_err(|e| e.to_string())?;
        }
        media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(|e| e.to_string())?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32).map_err(|e| e.to_string())?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32).map_err(|e| e.to_string())?;
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32).map_err(|e| e.to_string())?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32).map_err(|e| e.to_string())?;
        if subtype == MFVideoFormat_H264
            && let Some(distance) = settings.keyframe_distance
        {
            media_type.SetUINT32(&windows::Win32::Media::MediaFoundation::MF_MT_MAX_KEYFRAME_SPACING, distance).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn choose_output_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    rate: FrameRate,
    settings: &ExportSettings,
    name: &str,
) -> std::result::Result<IMFMediaType, String> {
    for index in 0..64 {
        // SAFETY: plain COM call; indices are bounded to avoid looping on a malformed transform.
        let output = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(output) => output,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(format!("{name}: cannot enumerate output types: {e}")),
        };
        // SAFETY: plain COM call on a live type.
        if unsafe { output.GetGUID(&MF_MT_SUBTYPE) }.ok() != Some(MFVideoFormat_H264) {
            continue;
        }
        set_video_type(&output, MFVideoFormat_H264, width, height, rate, settings)?;
        let profile = match settings.h264_profile {
            H264Profile::Baseline => windows::Win32::Media::MediaFoundation::eAVEncH264VProfile_Base.0 as u32,
            H264Profile::Main => windows::Win32::Media::MediaFoundation::eAVEncH264VProfile_Main.0 as u32,
            H264Profile::High => windows::Win32::Media::MediaFoundation::eAVEncH264VProfile_High.0 as u32,
        };
        // SAFETY: plain COM call on a live output media type.
        unsafe { output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile) }.map_err(|e| format!("{name}: cannot set H.264 profile: {e}"))?;
        return Ok(output);
    }
    Err(format!("{name}: no H.264 output type"))
}

fn frame_time_hns(index: u64, rate: FrameRate) -> std::result::Result<i64, String> {
    let ticks = i128::from(index)
        .checked_mul(HNS_PER_SECOND)
        .and_then(|value| value.checked_mul(i128::from(rate.den)))
        .ok_or_else(|| "frame timestamp overflows".to_string())?
        / i128::from(rate.num);
    i64::try_from(ticks).map_err(|_| "frame timestamp exceeds Media Foundation range".into())
}

fn nv12<'a>(frame: &EncoderFrame<'_>, y: &'a mut Vec<u8>, u: &'a mut Vec<u8>, v: &'a mut Vec<u8>, nv12: &'a mut Vec<u8>) -> ExportResult<&'a [u8]> {
    let width = usize::try_from(frame.width).map_err(|_| ExportError::Encode("frame width out of range".into()))?;
    let height = usize::try_from(frame.height).map_err(|_| ExportError::Encode("frame height out of range".into()))?;
    let expected = width.checked_mul(height).and_then(|pixels| pixels.checked_mul(4)).ok_or_else(|| ExportError::Encode("RGBA frame size overflows".into()))?;
    if frame.rgba.len() != expected {
        return Err(ExportError::Encode(format!("RGBA frame has {} bytes; expected {expected}", frame.rgba.len())));
    }
    let uv_size = (width / 2).checked_mul(height / 2).ok_or_else(|| ExportError::Encode("NV12 chroma size overflows".into()))?;
    let output_size = width
        .checked_mul(height)
        .and_then(|luma| luma.checked_add(uv_size.checked_mul(2)?))
        .ok_or_else(|| ExportError::Encode("NV12 frame size overflows".into()))?;
    if output_size > MAX_FRAME_BYTES {
        return Err(ExportError::Encode("NV12 frame exceeds the allocation limit".into()));
    }
    filmcraft_export::timed(filmcraft_export::Stage::Convert, || {
        filmcraft_export::rgba_to_yuv420_8(frame.rgba, width, height, y, u, v);
        nv12.clear();
        nv12.reserve(output_size.saturating_sub(nv12.capacity()));
        nv12.extend_from_slice(y);
        for (u, v) in u.iter().zip(v.iter()) {
            nv12.push(*u);
            nv12.push(*v);
        }
    });
    if nv12.len() != output_size {
        return Err(ExportError::Encode(format!("NV12 conversion returned {} bytes; expected {output_size}", nv12.len())));
    }
    Ok(nv12)
}

fn sample_bytes(sample: &IMFSample, name: &str) -> std::result::Result<Vec<u8>, String> {
    // SAFETY: GetBufferByIndex returns a referenced interface owned by this scope.
    let buffer: IMFMediaBuffer = unsafe { sample.GetBufferByIndex(0) }.map_err(|e| format!("{name}: output sample has no buffer: {e}"))?;
    // SAFETY: GetCurrentLength is a plain COM call on a live buffer.
    let length = unsafe { buffer.GetCurrentLength() }.map_err(|e| format!("{name}: cannot size encoded output: {e}"))? as usize;
    if length == 0 || length > MAX_OUTPUT_BYTES {
        return Err(format!("{name}: encoded output size {length} is invalid"));
    }
    // SAFETY: Lock returns a buffer of at least the reported current length, valid until Unlock.
    unsafe {
        let (mut ptr, mut max) = (std::ptr::null_mut::<u8>(), 0u32);
        buffer.Lock(&mut ptr, Some(&mut max), None).map_err(|e| format!("{name}: cannot lock encoded output: {e}"))?;
        if ptr.is_null() || max < length as u32 {
            let _ = buffer.Unlock();
            return Err(format!("{name}: encoded output buffer is shorter than its reported length"));
        }
        let bytes = std::slice::from_raw_parts(ptr, length).to_vec();
        buffer.Unlock().map_err(|e| format!("{name}: cannot unlock encoded output: {e}"))?;
        Ok(bytes)
    }
}

fn sequence_config(media_type: &IMFMediaType) -> Option<AvcConfig> {
    // Optional on some transforms until their first frame has been encoded.
    // SAFETY: GetBlobSize is a plain COM call on a live media type.
    let size = unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    if size == 0 || size > MAX_SEQUENCE_HEADER_BYTES {
        return None;
    }
    let (mut ptr, mut actual) = (std::ptr::null_mut(), 0u32);
    // SAFETY: the API allocates the blob with CoTaskMemAlloc; the bounded output is freed below.
    if unsafe { media_type.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut actual) }.is_err() {
        return None;
    }
    if ptr.is_null() || actual == 0 || actual > MAX_SEQUENCE_HEADER_BYTES {
        if !ptr.is_null() {
            // SAFETY: `ptr` is the allocation returned by GetAllocatedBlob.
            unsafe { CoTaskMemFree(Some(ptr as *const c_void)) };
        }
        return None;
    }
    // SAFETY: the API returned `actual` readable bytes; copy before freeing the allocation.
    let data = unsafe { std::slice::from_raw_parts(ptr, actual as usize) }.to_vec();
    // SAFETY: `ptr` is the allocation returned by GetAllocatedBlob.
    unsafe { CoTaskMemFree(Some(ptr as *const c_void)) };
    config_from_header(&data).ok()
}

fn config_from_header(data: &[u8]) -> std::result::Result<AvcConfig, String> {
    let (sps, pps) = if data.first() == Some(&1) { parse_avcc_header(data)? } else { parameter_sets(&nal_units(data)?)? };
    if sps.is_empty() || pps.is_empty() {
        return Err("sequence header has no SPS or PPS".into());
    }
    Ok(AvcConfig::new(sps, pps, 4))
}

fn parse_avcc_header(data: &[u8]) -> std::result::Result<(Vec<Vec<u8>>, Vec<Vec<u8>>), String> {
    if data.len() < 7 || data.first() != Some(&1) {
        return Err("invalid AVCDecoderConfigurationRecord".into());
    }
    let mut pos = 5usize;
    let nsps = usize::from(data.get(pos).ok_or("truncated AVCDecoderConfigurationRecord")? & 0x1f);
    pos = pos.checked_add(1).ok_or("AVC configuration offset overflows")?;
    let mut sps = Vec::new();
    for _ in 0..nsps {
        sps.push(read_config_nal(data, &mut pos)?);
    }
    let npps = usize::from(*data.get(pos).ok_or("truncated AVCDecoderConfigurationRecord")?);
    pos = pos.checked_add(1).ok_or("AVC configuration offset overflows")?;
    let mut pps = Vec::new();
    for _ in 0..npps {
        pps.push(read_config_nal(data, &mut pos)?);
    }
    Ok((sps, pps))
}

fn read_config_nal(data: &[u8], pos: &mut usize) -> std::result::Result<Vec<u8>, String> {
    let len_end = pos.checked_add(2).ok_or("AVC configuration offset overflows")?;
    let len = data.get(*pos..len_end).ok_or("truncated AVC parameter-set length")?;
    let size = usize::from(u16::from_be_bytes([*len.first().ok_or("truncated parameter-set length")?, *len.get(1).ok_or("truncated parameter-set length")?]));
    *pos = len_end;
    if size == 0 || size > u16::MAX as usize {
        return Err("invalid AVC parameter-set size".into());
    }
    let end = pos.checked_add(size).ok_or("AVC parameter-set offset overflows")?;
    let nal = data.get(*pos..end).ok_or("truncated AVC parameter set")?.to_vec();
    *pos = end;
    Ok(nal)
}

fn parameter_sets(units: &[&[u8]]) -> std::result::Result<(Vec<Vec<u8>>, Vec<Vec<u8>>), String> {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for unit in units {
        match unit.first().map(|b| b & 0x1f) {
            Some(7) if unit.len() <= u16::MAX as usize => sps.push(unit.to_vec()),
            Some(8) if unit.len() <= u16::MAX as usize => pps.push(unit.to_vec()),
            _ => {}
        }
    }
    Ok((sps, pps))
}

fn nal_units(data: &[u8]) -> std::result::Result<Vec<&[u8]>, String> {
    let annex_b =
        find_start_code(data, 0).filter(|(at, end)| *at == 0 && data.get(*end).is_some_and(|header| header & 0x80 == 0 && (1..=23).contains(&(header & 0x1f))));
    if let Some((first_start, first_end)) = annex_b {
        if data.get(..first_start).is_some_and(|prefix| prefix.iter().any(|b| *b != 0)) {
            return Err("nonzero bytes before Annex B start code".into());
        }
        let mut units = Vec::new();
        let mut start = first_end;
        while units.len() < MAX_NAL_UNITS {
            let next = find_start_code(data, start);
            let mut end = next.map_or(data.len(), |(at, _)| at);
            while end > start && data.get(end - 1) == Some(&0) {
                end -= 1;
            }
            let unit = data.get(start..end).ok_or("invalid Annex B NAL range")?;
            if !unit.is_empty() {
                units.push(unit);
            }
            let Some((_, next_start)) = next else { break };
            start = next_start;
        }
        if units.len() == MAX_NAL_UNITS && find_start_code(data, start).is_some() {
            return Err("Annex B sample exceeds the NAL unit limit".into());
        }
        if units.is_empty() {
            return Err("Annex B sample contains no NAL units".into());
        }
        return Ok(units);
    }

    let mut units = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() && units.len() < MAX_NAL_UNITS {
        let header_end = pos.checked_add(4).ok_or("NAL length offset overflows")?;
        let header = data.get(pos..header_end).ok_or("truncated NAL length")?;
        let size = usize::try_from(u32::from_be_bytes([
            *header.first().ok_or("truncated NAL length")?,
            *header.get(1).ok_or("truncated NAL length")?,
            *header.get(2).ok_or("truncated NAL length")?,
            *header.get(3).ok_or("truncated NAL length")?,
        ]))
        .map_err(|_| "NAL length is out of range")?;
        if size == 0 {
            return Err("empty length-prefixed NAL unit".into());
        }
        let end = header_end.checked_add(size).ok_or("NAL length overflows")?;
        units.push(data.get(header_end..end).ok_or("truncated length-prefixed NAL")?);
        pos = end;
    }
    if pos != data.len() || units.is_empty() {
        return Err("invalid length-prefixed H.264 sample".into());
    }
    Ok(units)
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let last = data.len().checked_sub(3)?;
    for i in from..=last {
        if data.get(i..i + 4) == Some(&[0, 0, 0, 1]) {
            return Some((i, i + 4));
        }
        if data.get(i..i + 3) == Some(&[0, 0, 1]) {
            return Some((i, i + 3));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_annex_b_start_codes_and_length_prefixed_samples() {
        let annex_b = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
        let nals = nal_units(&annex_b).expect("well-formed Annex B sample");
        assert_eq!(nals, [&[0x67, 1][..], &[0x68, 2][..], &[0x65, 3][..]]);

        let avcc = [0, 0, 0, 2, 0x67, 1, 0, 0, 0, 2, 0x65, 3];
        let nals = nal_units(&avcc).expect("well-formed length-prefixed sample");
        assert_eq!(nals, [&[0x67, 1][..], &[0x65, 3][..]]);
    }

    #[test]
    fn rejects_truncated_and_unbounded_nal_sequences() {
        assert!(nal_units(&[0, 0, 0, 4, 0x65]).is_err());
        assert!(nal_units(&[0, 0, 1]).is_err());
        assert!(nal_units(&[]).is_err());
    }

    #[test]
    fn length_prefix_containing_start_code_bytes_is_not_mistaken_for_annex_b() {
        let mut sample = vec![0, 0, 1, 0];
        sample.extend(std::iter::repeat_n(0x41, 256));
        let units = nal_units(&sample).expect("a 256-byte length-prefixed NAL");
        assert_eq!(units, [sample.get(4..).expect("NAL payload")]);
    }

    #[test]
    fn parses_avc_decoder_configuration_record() {
        let header = [1, 100, 0, 40, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 2];
        let (sps, pps) = parse_avcc_header(&header).expect("valid AVC configuration record");
        assert_eq!(sps, [vec![0x67, 1]]);
        assert_eq!(pps, [vec![0x68, 2]]);
    }

    #[test]
    fn frame_timestamps_use_rational_rate_and_bound_hostile_inputs() {
        assert_eq!(frame_time_hns(24, FrameRate::FPS_24).expect("timestamp"), 10_000_000);
        assert!(frame_time_hns(u64::MAX, FrameRate::FPS_24).is_err());
    }

    #[test]
    fn hardware_encoder_declines_unsupported_settings_before_loading_media_foundation() {
        let settings = ExportSettings { hardware_encoding: HardwareEncoding::Off, ..Default::default() };
        assert!(factory(Format::H264, 1920, 1080, FrameRate::FPS_24, &settings).is_none());
    }

    #[test]
    fn hardware_encoder_round_trips_when_a_compatible_mft_is_available() {
        let settings = ExportSettings { hardware_encoding: HardwareEncoding::Auto, bitrate_kbps: 2_000, keyframe_distance: Some(12), ..Default::default() };
        let (width, height) = (640, 360);
        let mut encoder = match MfEncoder::new(width, height, FrameRate::FPS_24, &settings) {
            Ok(encoder) => encoder,
            Err(reason) => {
                eprintln!("SKIPPED: no compatible hardware H.264 MFT: {reason}");
                return;
            }
        };
        let mut packets = Vec::new();
        for index in 0..24 {
            let mut rgba = vec![0; width as usize * height as usize * 4];
            for pixel in rgba.as_chunks_mut::<4>().0 {
                pixel.copy_from_slice(&[index as u8 * 8, 80, 160, 255]);
            }
            packets.extend(encoder.encode(&EncoderFrame { width, height, rgba: &rgba, hdr: None, index }).expect("hardware encode"));
        }
        packets.extend(encoder.flush().expect("hardware encoder flush"));
        assert_eq!(packets.len(), 24, "one encoded access unit per input frame");
        assert!(packets.first().is_some_and(|packet| packet.key), "first frame is an IDR");

        let entry = encoder.sample_entry();
        let mut decoder = filmcraft_codecs::software_video_decoder(&entry).expect("valid H.264 sample entry");
        let mut decoded = Vec::new();
        for (index, packet) in packets.iter().enumerate() {
            decoded.extend(decoder.decode(&packet.data, index as i64).expect("decode hardware packet"));
        }
        decoded.extend(decoder.flush());
        assert_eq!(decoded.len(), packets.len(), "all hardware frames decode");
    }
}
