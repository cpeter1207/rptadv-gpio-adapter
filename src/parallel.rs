//! Parallel-port implementation of the GPIO adapter contract.
//!
//! A service owner performs all ppdev or direct-I/O access.  Native ticks only
//! publish a prepared output action and copy already-published input and status
//! snapshots. The adapter also owns the established parallel-port binary
//! channel and RTX serial programming protocols as bounded control-plane
//! operations; radio policy still remains above this hardware boundary.

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
use super::GPIO_UNSUPPORTED;
use super::{
    ABI_VERSION, GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK, ScheduledPulseState,
    TimedPulseState, ffi, publish_scheduled_pulse, publish_timed_pulse,
};
use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_int, c_ulong};
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
use std::fs::{File, OpenOptions};
use std::mem::size_of;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
use std::os::unix::fs::FileExt;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Explicit Linux ppdev transport selected by a configuration value.
pub(crate) const PARALLEL_TRANSPORT_PPDEV: u32 = 1;
/// Explicit raw x86 I/O-port transport selected by a configuration value.
pub(crate) const PARALLEL_TRANSPORT_RAW_IO: u32 = 2;
/// Try the configured ppdev node, then an explicitly configured raw I/O port.
pub(crate) const PARALLEL_TRANSPORT_AUTO: u32 = 0;

/// Number of serial bits in each established RTX programming register.
const RTX_REGISTER_BITS: u32 = 20;
/// Busy-spin iterations in one established RTX hardware-settling interval.
const RTX_BIT_TIME_ITERATIONS: usize = 100_000;
/// Parallel data bit used as the RTX serial clock.
const RTX_CLOCK_MASK: u8 = 0x01;
/// Parallel data bit carrying the RTX serial value.
const RTX_DATA_MASK: u8 = 0x02;
/// Parallel data bit that latches one shifted RTX word.
const RTX_ENABLE_MASK: u8 = 0x04;
/// Parallel data bit that keys the RTX transmitter.
const RTX_TRANSMIT_MASK: u8 = 0x08;
/// Parallel data bit historically reserved for RTX transmit power and held low.
const RTX_TRANSMIT_POWER_MASK: u8 = 0x10;
/// RTX protocol lines reset before every serial word.
const RTX_CONTROL_MASK: u8 =
    RTX_CLOCK_MASK | RTX_DATA_MASK | RTX_ENABLE_MASK | RTX_TRANSMIT_MASK | RTX_TRANSMIT_POWER_MASK;
/// Four active-low binary channel-select bits on the legacy parallel interface.
const BINARY_CHANNEL_MASK: u8 = 0xf0;
/// Process-wide settle state retained by the established RTX serial protocol.
static RTX_SERIAL_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Linux `O_RDWR` flag.  The adapter needs both ppdev status and data access.
#[cfg(target_os = "linux")]
const O_RDWR: c_int = 2;
/// Linux ppdev ioctl type.
#[cfg(target_os = "linux")]
const PP_IOCTL: c_ulong = 0x70;
/// Claim a ppdev port exclusively before manipulating its data pins.
#[cfg(target_os = "linux")]
const PPCLAIM: c_ulong = (PP_IOCTL << 8) | 0x8b;
/// Release a prior ppdev claim during teardown.
#[cfg(target_os = "linux")]
const PPRELEASE: c_ulong = (PP_IOCTL << 8) | 0x8c;
/// Read an eight-bit parallel status register through ppdev.
#[cfg(target_os = "linux")]
const PPRSTATUS: c_ulong =
    (2_u64 << 30) as c_ulong | (1_u64 << 16) as c_ulong | (PP_IOCTL << 8) | 0x81;
/// Write an eight-bit parallel data register through ppdev.
#[cfg(target_os = "linux")]
const PPWDATA: c_ulong =
    (1_u64 << 30) as c_ulong | (1_u64 << 16) as c_ulong | (PP_IOCTL << 8) | 0x86;

/// Opaque, exclusively owned parallel-port GPIO handle.
#[repr(C)]
pub struct ParallelDevice {
    /// One service owner exclusively mutates this transport.
    transport: UnsafeCell<Box<dyn ParallelTransport>>,
    /// Data-register bits available for callers to change.
    output_enable_mask: u8,
    /// Latest persistent data-register state published by a nonblocking caller.
    desired_outputs: AtomicU32,
    /// Latest data-register byte successfully applied by the service owner.
    flushed_outputs: AtomicU32,
    /// Latest parallel status-register byte sampled by the service owner.
    input_status: AtomicU32,
    /// Number of data writes attempted by the service owner.
    output_apply_count: AtomicU64,
    /// Number of status reads attempted by the service owner.
    input_read_count: AtomicU64,
    /// Number of observed ppdev or raw-I/O failures.
    io_error_count: AtomicU64,
    /// Nonzero only while the selected transport remains open.
    online: AtomicBool,
    /// Most recent transport error, reset after successful I/O.
    last_io_error: AtomicI32,
    /// Latest requested pulse mask and duration packed by the tick.
    pulse_request: AtomicU64,
    /// Generation paired with @ref pulse_request.
    pulse_generation: AtomicU64,
    /// Service-owner-only pulse timing and output state.
    service_state: UnsafeCell<TimedPulseState>,
    /// Per-bit durations for independently expiring baseline-XOR pulses.
    scheduled_pulse_durations: [AtomicU32; u32::BITS as usize],
    /// Per-bit publication generations for independently scheduled pulse bits.
    scheduled_pulse_generations: [AtomicU64; u32::BITS as usize],
    /// Service-owner-only state for independent pulse deadlines.
    scheduled_service_state: UnsafeCell<ScheduledPulseState>,
}

/// C-compatible selection of one parallel-port transport.
#[repr(C)]
pub(crate) struct ParallelConfig {
    pub(crate) struct_size: u32,
    pub(crate) abi_version: u32,
    pub(crate) transport: u32,
    pub(crate) ppdev_path: *const c_char,
    pub(crate) raw_io_base: u32,
    pub(crate) output_enable_mask: u32,
    pub(crate) output_initial_mask: u32,
}

/// C-compatible lock-free parallel input snapshot.
#[repr(C)]
pub(crate) struct ParallelInputSnapshot {
    pub(crate) struct_size: u32,
    pub(crate) abi_version: u32,
    pub(crate) online: u32,
    pub(crate) status_mask: u32,
}

/// C-compatible action prepared by a native tick or other nonblocking caller.
#[repr(C)]
pub(crate) struct ParallelOutputAction {
    pub(crate) struct_size: u32,
    pub(crate) abi_version: u32,
    pub(crate) output_mask: u32,
    pub(crate) pulse_mask: u32,
    pub(crate) pulse_duration_milliseconds: u32,
    pub(crate) cancel_pulse: u32,
}

/// C-compatible timed XOR pulse action matching legacy parallel-port behavior.
#[repr(C)]
pub(crate) struct ParallelInvertingPulseAction {
    pub(crate) struct_size: u32,
    pub(crate) abi_version: u32,
    pub(crate) invert_mask: u32,
    pub(crate) pulse_duration_milliseconds: u32,
    pub(crate) cancel_pulse: u32,
}

/// C-compatible independently scheduled parallel output inversion request.
#[repr(C)]
pub(crate) struct ParallelScheduledInvertingPulseAction {
    pub(crate) struct_size: u32,
    pub(crate) abi_version: u32,
    pub(crate) invert_mask: u32,
    pub(crate) pulse_duration_milliseconds: u32,
    pub(crate) cancel_mask: u32,
}

/// C-compatible best-effort parallel-port status snapshot.
#[repr(C)]
pub(crate) struct ParallelStats {
    pub(crate) struct_size: u32,
    pub(crate) abi_version: u32,
    pub(crate) input_read_count: u64,
    pub(crate) output_apply_count: u64,
    pub(crate) io_error_count: u64,
    pub(crate) online: u32,
    pub(crate) last_io_error: i32,
    pub(crate) applied_output_mask: u32,
}

/// The two established RTX serial words for one selected receive or transmit frequency.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RtxWords {
    /// Reference-divider word shifted before the synthesizer word.
    reference: u32,
    /// Synthesizer word for the selected receive or transmit frequency.
    synthesizer: u32,
}

/// Calculate the established RTX serial words without performing hardware I/O.
fn rtx_words(rx_frequency_hz: u32, tx_frequency_hz: u32, transmitting: bool) -> RtxWords {
    let (reference_frequency_hz, step_frequency_hz, receiver_if_frequency_hz) =
        if rx_frequency_hz > 200_000_000 {
            (16_012_500, 12_500, 21_400_000)
        } else {
            (16_000_000, 5_000, 10_700_000)
        };
    let synthesizer_frequency_hz = if transmitting {
        tx_frequency_hz
    } else {
        rx_frequency_hz.wrapping_sub(receiver_if_frequency_hz)
    };
    let word = (synthesizer_frequency_hz / step_frequency_hz).wrapping_shl(1);

    RtxWords {
        reference: (reference_frequency_hz / step_frequency_hz).wrapping_shl(1) | 1,
        synthesizer: (word & 0xffff_ff80)
            .wrapping_shl(1)
            .wrapping_add(word & 0x7f),
    }
}

/// Return the long initial or short subsequent serial-settling delay multiplier.
fn rtx_initial_delay_multiplier() -> u32 {
    rtx_initial_delay_multiplier_for(&RTX_SERIAL_INITIALIZED)
}

/// Return one serial-settling delay multiplier from a supplied protocol state.
fn rtx_initial_delay_multiplier_for(initialized: &AtomicBool) -> u32 {
    if initialized.swap(true, Ordering::AcqRel) {
        4
    } else {
        200
    }
}

/// Preserve the established RTX busy-spin settling interval outside real-time audio.
fn rtx_delay(multiplier: u32) {
    for _ in 0..RTX_BIT_TIME_ITERATIONS * multiplier as usize {
        std::hint::spin_loop();
    }
}

/// Shift and latch one established 20-bit RTX word, most-significant bit first.
///
/// The caller supplies the only hardware-write and delay operations so tests
/// can prove ordering and settling intent without contacting a parallel port.
fn rtx_shift_word(
    output: &mut u8,
    word: u32,
    initial_delay_multiplier: u32,
    write: &mut impl FnMut(u8) -> Result<(), c_int>,
    delay: &mut impl FnMut(u32),
) -> Result<(), c_int> {
    *output &= !RTX_CONTROL_MASK;
    write(*output)?;
    delay(initial_delay_multiplier);

    for bit_index in (0..RTX_REGISTER_BITS).rev() {
        let bit = 1_u32 << bit_index;
        if word & bit != 0 {
            *output |= RTX_DATA_MASK;
        } else {
            *output &= !RTX_DATA_MASK;
        }
        write(*output)?;
        delay(1);
        *output |= RTX_CLOCK_MASK;
        write(*output)?;
        delay(1);
        *output &= !RTX_CLOCK_MASK;
        write(*output)?;
        delay(1);
    }
    *output &= !(RTX_CLOCK_MASK | RTX_DATA_MASK);
    write(*output)?;
    *output |= RTX_ENABLE_MASK;
    write(*output)?;
    delay(1);
    *output &= !RTX_ENABLE_MASK;
    write(*output)
}

/// Strictly validated control-plane configuration.
pub(crate) struct ValidatedParallelConfig {
    /// Explicit transport choice.
    transport: u32,
    /// Optional configured ppdev node for ppdev or automatic selection.
    ppdev_path: Option<CString>,
    /// Explicit raw x86 I/O base for raw or automatic selection.
    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    raw_io_base: Option<u16>,
    /// Data bits that a caller may manipulate.
    output_enable_mask: u8,
    /// Initial persistent data-register byte.
    output_initial_mask: u8,
}

/// Parallel I/O surface hidden behind the stable adapter ABI.
///
/// Only one non-real-time service owner accesses this transport.  Test
/// transports implement the same narrow interface without physical hardware.
pub(crate) trait ParallelTransport: Send {
    /// Write one complete parallel data-register byte.
    fn write_data(&mut self, data: u8) -> Result<(), c_int>;
    /// Read one complete parallel status-register byte.
    fn read_status(&mut self) -> Result<u8, c_int>;
}

/// Native ppdev device with an exclusive claim held for its lifetime.
#[cfg(target_os = "linux")]
struct PpdevTransport {
    /// Claimed ppdev descriptor.
    file_descriptor: c_int,
}

/// Direct x86 I/O-port device held through Linux `/dev/port`.
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
struct RawIoTransport {
    /// Base data-register I/O port.
    base: u16,
    /// Privileged Linux raw-I/O device descriptor.
    device: File,
}

/// Placeholder preserving the unsupported-target failure contract at compile time.
#[cfg(not(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64"))))]
struct RawIoTransport;

impl ValidatedParallelConfig {
    /// Validate an ABI configuration without opening a host resource.
    pub(crate) unsafe fn from_ffi(config: *const ParallelConfig) -> Option<Self> {
        // SAFETY: caller validates the pointer before invoking this conversion.
        let config = unsafe { config.as_ref()? };
        if config.struct_size < size_of::<ParallelConfig>() as u32
            || config.abi_version != ABI_VERSION
            || !matches!(
                config.transport,
                PARALLEL_TRANSPORT_AUTO | PARALLEL_TRANSPORT_PPDEV | PARALLEL_TRANSPORT_RAW_IO
            )
            || config.output_enable_mask > u32::from(u8::MAX)
            || config.output_initial_mask > u32::from(u8::MAX)
        {
            return None;
        }
        let output_enable_mask = config.output_enable_mask as u8;
        let output_initial_mask = config.output_initial_mask as u8;
        if output_initial_mask & !output_enable_mask != 0 {
            return None;
        }
        let ppdev_path = if config.ppdev_path.is_null() {
            None
        } else {
            // SAFETY: nonnull ABI pointer is required to name a NUL-terminated path.
            let value = unsafe { CStr::from_ptr(config.ppdev_path) };
            let value = value.to_bytes();
            if value.is_empty() || !value.contains(&b'/') {
                return None;
            }
            CString::new(value).ok()
        };
        let raw_io_base = u16::try_from(config.raw_io_base)
            .ok()
            .filter(|base| *base != 0 && *base < u16::MAX);
        match config.transport {
            PARALLEL_TRANSPORT_PPDEV if ppdev_path.is_none() => None,
            PARALLEL_TRANSPORT_RAW_IO if raw_io_base.is_none() => None,
            PARALLEL_TRANSPORT_AUTO if ppdev_path.is_none() && raw_io_base.is_none() => None,
            _ => Some(Self {
                transport: config.transport,
                ppdev_path,
                #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
                raw_io_base,
                output_enable_mask,
                output_initial_mask,
            }),
        }
    }
}

impl ParallelDevice {
    /// Construct one open device around a control-plane transport.
    pub(crate) fn with_transport(
        config: ValidatedParallelConfig,
        transport: Box<dyn ParallelTransport>,
    ) -> Self {
        Self {
            transport: UnsafeCell::new(transport),
            output_enable_mask: config.output_enable_mask,
            desired_outputs: AtomicU32::new(u32::from(config.output_initial_mask)),
            flushed_outputs: AtomicU32::new(u32::MAX),
            input_status: AtomicU32::new(0),
            output_apply_count: AtomicU64::new(0),
            input_read_count: AtomicU64::new(0),
            io_error_count: AtomicU64::new(0),
            online: AtomicBool::new(true),
            last_io_error: AtomicI32::new(0),
            pulse_request: AtomicU64::new(0),
            pulse_generation: AtomicU64::new(0),
            service_state: UnsafeCell::new(TimedPulseState::new()),
            scheduled_pulse_durations: std::array::from_fn(|_| AtomicU32::new(0)),
            scheduled_pulse_generations: std::array::from_fn(|_| AtomicU64::new(0)),
            scheduled_service_state: UnsafeCell::new(ScheduledPulseState::new()),
        }
    }

    /// Record a transport result for lock-free observers.
    fn record_result(&self, result: Result<(), c_int>) -> Result<(), c_int> {
        match result {
            Ok(()) => {
                self.last_io_error.store(0, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.io_error_count.fetch_add(1, Ordering::Relaxed);
                self.last_io_error.store(error, Ordering::Release);
                Err(error)
            }
        }
    }

    /// Publish persistent data output and an optional timed high pulse without I/O.
    pub(crate) fn publish_outputs(&self, action: &ParallelOutputAction) -> c_int {
        if action.struct_size < size_of::<ParallelOutputAction>() as u32
            || action.abi_version != ABI_VERSION
            || action.output_mask > u32::from(u8::MAX)
            || action.pulse_mask > u32::from(u8::MAX)
            || action.cancel_pulse > 1
            || (action.cancel_pulse != 0
                && (action.pulse_duration_milliseconds != 0 || action.pulse_mask != 0))
            || (action.pulse_duration_milliseconds == 0 && action.pulse_mask != 0)
            || (action.pulse_duration_milliseconds != 0 && action.pulse_mask == 0)
        {
            return GPIO_INVALID_ARGUMENT;
        }
        let output = action.output_mask as u8;
        let pulse = action.pulse_mask as u8;
        if output & !self.output_enable_mask != 0 || pulse & !self.output_enable_mask != 0 {
            return GPIO_INVALID_ARGUMENT;
        }
        self.desired_outputs
            .store(u32::from(output), Ordering::Release);
        if action.pulse_duration_milliseconds != 0 || action.cancel_pulse != 0 {
            publish_timed_pulse(
                &self.pulse_request,
                &self.pulse_generation,
                u32::from(pulse),
                action.pulse_duration_milliseconds,
                false,
            );
        }
        GPIO_OK
    }

    /// Publish a timed baseline-XOR pulse without host I/O.
    pub(crate) fn publish_inverting_pulse(&self, action: &ParallelInvertingPulseAction) -> c_int {
        if action.struct_size < size_of::<ParallelInvertingPulseAction>() as u32
            || action.abi_version != ABI_VERSION
            || action.invert_mask > u32::from(u8::MAX)
            || action.cancel_pulse > 1
            || (action.cancel_pulse != 0
                && (action.pulse_duration_milliseconds != 0 || action.invert_mask != 0))
            || (action.cancel_pulse == 0
                && ((action.pulse_duration_milliseconds == 0) != (action.invert_mask == 0)))
        {
            return GPIO_INVALID_ARGUMENT;
        }
        let mask = action.invert_mask as u8;
        if mask & !self.output_enable_mask != 0 {
            return GPIO_INVALID_ARGUMENT;
        }
        if action.pulse_duration_milliseconds != 0 || action.cancel_pulse != 0 {
            publish_timed_pulse(
                &self.pulse_request,
                &self.pulse_generation,
                u32::from(mask),
                action.pulse_duration_milliseconds,
                true,
            );
        }
        GPIO_OK
    }

    /// Schedule independently expiring parallel baseline-XOR pulses without I/O.
    pub(crate) fn schedule_inverting_pulse(
        &self,
        action: &ParallelScheduledInvertingPulseAction,
    ) -> c_int {
        if action.struct_size < size_of::<ParallelScheduledInvertingPulseAction>() as u32
            || action.abi_version != ABI_VERSION
            || action.invert_mask > u32::from(u8::MAX)
            || action.cancel_mask > u32::from(u8::MAX)
        {
            return GPIO_INVALID_ARGUMENT;
        }
        let schedule = action.invert_mask as u8;
        let cancel = action.cancel_mask as u8;
        if schedule & cancel != 0
            || (action.pulse_duration_milliseconds == 0 && schedule != 0)
            || (action.pulse_duration_milliseconds != 0 && schedule == 0)
            || (schedule | cancel) & !self.output_enable_mask != 0
        {
            return GPIO_INVALID_ARGUMENT;
        }
        if schedule != 0 || cancel != 0 {
            publish_scheduled_pulse(
                &self.scheduled_pulse_durations,
                &self.scheduled_pulse_generations,
                u32::from(schedule),
                u32::from(cancel),
                action.pulse_duration_milliseconds,
            );
        }
        GPIO_OK
    }

    /// Apply one requested data byte through the sole service owner.
    fn write_data(&self, data: u8) -> Result<(), c_int> {
        self.output_apply_count.fetch_add(1, Ordering::Relaxed);
        // SAFETY: descriptor contract serializes the service owner; no tick mutates transport.
        let result = unsafe { (*self.transport.get()).write_data(data) };
        self.record_result(result)
    }

    /// Return whether every selected data-register bit belongs to this device.
    fn supports_output_bits(&self, mask: u8) -> bool {
        mask & !self.output_enable_mask == 0
    }

    /// Write one prevalidated control-plane byte and retain it as the next baseline.
    fn write_control_byte(&self, data: u8) -> Result<(), c_int> {
        debug_assert!(self.supports_output_bits(data));
        self.write_data(data)?;
        self.desired_outputs
            .store(u32::from(data), Ordering::Release);
        self.flushed_outputs
            .store(u32::from(data), Ordering::Release);
        Ok(())
    }

    /// Sample one status byte through the sole service owner.
    fn read_status(&self) -> Result<u8, c_int> {
        self.input_read_count.fetch_add(1, Ordering::Relaxed);
        // SAFETY: descriptor contract serializes the service owner; no tick mutates transport.
        let result = unsafe { (*self.transport.get()).read_status() };
        match result {
            Ok(status) => {
                self.last_io_error.store(0, Ordering::Release);
                Ok(status)
            }
            Err(error) => {
                self.io_error_count.fetch_add(1, Ordering::Relaxed);
                self.last_io_error.store(error, Ordering::Release);
                Err(error)
            }
        }
    }

    /// Run one non-real-time parallel service cycle.
    pub(crate) fn service_at(&self, now: Instant) -> c_int {
        let requested_generation = self.pulse_generation.load(Ordering::Acquire);
        let requested_pulse = self.pulse_request.load(Ordering::Acquire);
        // SAFETY: descriptor contract permits one exclusive service owner.
        let state = unsafe { &mut *self.service_state.get() };
        state.observe_request(now, requested_generation, requested_pulse);
        // SAFETY: that same contract reserves scheduled state to that owner.
        let scheduled_state = unsafe { &mut *self.scheduled_service_state.get() };
        scheduled_state.observe_requests(
            now,
            &self.scheduled_pulse_durations,
            &self.scheduled_pulse_generations,
        );
        let target = scheduled_state.apply_to(
            state.apply_to(self.desired_outputs.load(Ordering::Acquire), now),
            now,
        ) as u8;
        if u32::from(target) != self.flushed_outputs.load(Ordering::Acquire) {
            if self.write_data(target).is_err() {
                return GPIO_IO_ERROR;
            }
            self.flushed_outputs
                .store(u32::from(target), Ordering::Release);
        }
        match self.read_status() {
            Ok(status) => {
                self.input_status
                    .store(u32::from(status), Ordering::Release);
                GPIO_OK
            }
            Err(_) => GPIO_IO_ERROR,
        }
    }

    /// Run one non-real-time parallel service cycle using the current monotonic time.
    fn service(&self) -> c_int {
        self.service_at(Instant::now())
    }

    /// Control-plane byte write retained for legacy radio-programming sequences.
    pub(crate) fn control_write_data(&self, data: u32) -> c_int {
        let Ok(data) = u8::try_from(data) else {
            return GPIO_INVALID_ARGUMENT;
        };
        if !self.supports_output_bits(data) {
            return GPIO_INVALID_ARGUMENT;
        }
        if self.write_control_byte(data).is_ok() {
            GPIO_OK
        } else {
            GPIO_IO_ERROR
        }
    }

    /// Latch a four-bit active-low binary channel selection through the control plane.
    pub(crate) fn set_binary_channel(&self, channel: u8) -> c_int {
        if !self.supports_output_bits(BINARY_CHANNEL_MASK) {
            return GPIO_INVALID_ARGUMENT;
        }
        let mut output = self.desired_outputs.load(Ordering::Acquire) as u8;
        output |= BINARY_CHANNEL_MASK;
        if self.write_control_byte(output).is_err() {
            return GPIO_IO_ERROR;
        }
        output &= !channel.wrapping_shl(4);
        if self.write_control_byte(output).is_err() {
            return GPIO_IO_ERROR;
        }
        GPIO_OK
    }

    /// Program the established RTX reference and synthesizer registers.
    pub(crate) fn program_rtx(
        &self,
        rx_frequency_hz: u32,
        tx_frequency_hz: u32,
        transmitting: u32,
        high_power: u32,
    ) -> c_int {
        let mut delay = rtx_delay;
        self.program_rtx_with_delay(
            rx_frequency_hz,
            tx_frequency_hz,
            transmitting,
            high_power,
            &mut delay,
        )
    }

    /// Program RTX control bytes using an injected settling operation for deterministic tests.
    fn program_rtx_with_delay(
        &self,
        rx_frequency_hz: u32,
        tx_frequency_hz: u32,
        transmitting: u32,
        high_power: u32,
        delay: &mut impl FnMut(u32),
    ) -> c_int {
        if rx_frequency_hz == 0 {
            return GPIO_OK;
        }
        if !self.supports_output_bits(RTX_CONTROL_MASK) {
            return GPIO_INVALID_ARGUMENT;
        }
        // The legacy protocol accepts this request but deliberately holds TXPWR low.
        let _ = high_power;
        let words = rtx_words(rx_frequency_hz, tx_frequency_hz, transmitting != 0);
        let mut output = self.desired_outputs.load(Ordering::Acquire) as u8;
        let mut write = |data| self.write_control_byte(data);
        if rtx_shift_word(
            &mut output,
            words.reference,
            rtx_initial_delay_multiplier(),
            &mut write,
            delay,
        )
        .is_err()
            || rtx_shift_word(
                &mut output,
                words.synthesizer,
                rtx_initial_delay_multiplier(),
                &mut write,
                delay,
            )
            .is_err()
        {
            return GPIO_IO_ERROR;
        }
        output &= !(RTX_CLOCK_MASK | RTX_DATA_MASK | RTX_ENABLE_MASK);
        if transmitting != 0 {
            output &= !RTX_TRANSMIT_POWER_MASK;
            output |= RTX_TRANSMIT_MASK;
        } else {
            output &= !(RTX_TRANSMIT_MASK | RTX_TRANSMIT_POWER_MASK);
        }
        if self.write_control_byte(output).is_ok() {
            GPIO_OK
        } else {
            GPIO_IO_ERROR
        }
    }

    /// Clear only the RTX transmit and retained-low power bits without serial traffic.
    pub(crate) fn clear_rtx_transmit(&self) -> c_int {
        let transmit_mask = RTX_TRANSMIT_MASK | RTX_TRANSMIT_POWER_MASK;
        if !self.supports_output_bits(transmit_mask) {
            return GPIO_INVALID_ARGUMENT;
        }
        let output = self.desired_outputs.load(Ordering::Acquire) as u8 & !transmit_mask;
        if self.write_control_byte(output).is_ok() {
            GPIO_OK
        } else {
            GPIO_IO_ERROR
        }
    }

    /// Copy the latest status without touching ppdev or raw I/O.
    pub(crate) fn inputs(&self, snapshot: &mut ParallelInputSnapshot) -> c_int {
        if snapshot.struct_size < size_of::<ParallelInputSnapshot>() as u32 {
            return GPIO_INVALID_ARGUMENT;
        }
        snapshot.abi_version = ABI_VERSION;
        snapshot.online = u32::from(self.online.load(Ordering::Acquire));
        snapshot.status_mask = self.input_status.load(Ordering::Acquire);
        GPIO_OK
    }

    /// Copy lock-free I/O counters and latest data output.
    pub(crate) fn stats(&self, stats: &mut ParallelStats) -> c_int {
        if stats.struct_size < size_of::<ParallelStats>() as u32 {
            return GPIO_INVALID_ARGUMENT;
        }
        stats.abi_version = ABI_VERSION;
        stats.input_read_count = self.input_read_count.load(Ordering::Acquire);
        stats.output_apply_count = self.output_apply_count.load(Ordering::Acquire);
        stats.io_error_count = self.io_error_count.load(Ordering::Acquire);
        stats.online = u32::from(self.online.load(Ordering::Acquire));
        stats.last_io_error = self.last_io_error.load(Ordering::Acquire);
        stats.applied_output_mask = self.flushed_outputs.load(Ordering::Acquire);
        GPIO_OK
    }

    /// Deassert all enabled data pins before releasing the transport.
    pub(crate) fn close(&mut self) {
        publish_timed_pulse(&self.pulse_request, &self.pulse_generation, 0, 0, false);
        publish_scheduled_pulse(
            &self.scheduled_pulse_durations,
            &self.scheduled_pulse_generations,
            0,
            u32::MAX,
            0,
        );
        let _ = self.write_data(0);
        self.flushed_outputs.store(0, Ordering::Release);
        self.online.store(false, Ordering::Release);
    }
}

/// A handle can be shared with lock-free publishers but only its service owner touches transport.
unsafe impl Send for ParallelDevice {}
/// All shared observers use atomics; the contract serializes the mutable service owner.
unsafe impl Sync for ParallelDevice {}

/// Open the explicitly configured hardware transport without selecting arbitrary hardware.
fn open_transport(config: &ValidatedParallelConfig) -> Result<Box<dyn ParallelTransport>, c_int> {
    match config.transport {
        PARALLEL_TRANSPORT_PPDEV => box_parallel_transport(open_ppdev(config)),
        PARALLEL_TRANSPORT_RAW_IO => box_parallel_transport(open_raw_io(config)),
        PARALLEL_TRANSPORT_AUTO => {
            if let Ok(transport) = open_ppdev(config) {
                return box_parallel_transport(Ok(transport));
            }
            box_parallel_transport(open_raw_io(config))
        }
        _ => Err(GPIO_INVALID_ARGUMENT),
    }
}

/// Erase a concrete control-plane transport behind the adapter's internal handle.
fn box_parallel_transport<T: ParallelTransport + 'static>(
    transport: Result<T, c_int>,
) -> Result<Box<dyn ParallelTransport>, c_int> {
    transport.map(|value| Box::new(value) as _)
}

/// Open the configured ppdev node and retain an exclusive port claim.
#[cfg(target_os = "linux")]
fn open_ppdev(config: &ValidatedParallelConfig) -> Result<PpdevTransport, c_int> {
    let Some(path) = config.ppdev_path.as_ref() else {
        return Err(GPIO_INVALID_ARGUMENT);
    };
    // SAFETY: the validated path is NUL-terminated and the call has no aliasing requirements.
    let file_descriptor = unsafe { ffi::open(path.as_ptr(), O_RDWR, 0) };
    if file_descriptor < 0 {
        return Err(GPIO_IO_ERROR);
    }
    // SAFETY: this descriptor was opened above and ppdev ignores the third argument for PPCLAIM.
    if unsafe { ffi::ioctl(file_descriptor, PPCLAIM, 0) } < 0 {
        // SAFETY: this path still owns the descriptor.
        unsafe { ffi::close(file_descriptor) };
        return Err(GPIO_IO_ERROR);
    }
    Ok(PpdevTransport { file_descriptor })
}

/// Reject ppdev on non-Linux targets rather than selecting an unverified substitute.
#[cfg(not(target_os = "linux"))]
fn open_ppdev(_config: &ValidatedParallelConfig) -> Result<PpdevTransport, c_int> {
    Err(GPIO_UNSUPPORTED)
}

/// Open the configured raw I/O range where the Linux architecture supports it.
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
fn open_raw_io(config: &ValidatedParallelConfig) -> Result<RawIoTransport, c_int> {
    open_raw_io_path(config, Path::new("/dev/port"))
}

/// Open a bounded two-register raw-I/O transport at a supplied Linux device path.
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
fn open_raw_io_path(
    config: &ValidatedParallelConfig,
    path: &Path,
) -> Result<RawIoTransport, c_int> {
    let Some(base) = config.raw_io_base else {
        return Err(GPIO_INVALID_ARGUMENT);
    };
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| GPIO_IO_ERROR)?;
    Ok(RawIoTransport { base, device })
}

/// Convert a one-byte positional I/O result to the adapter's portable error code.
///
/// A successful one-byte register operation must transfer exactly one byte;
/// EOF and a kernel error are both unsafe for a GPIO state transition.
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
fn exact_raw_io_byte(result: std::io::Result<usize>) -> Result<(), c_int> {
    match result {
        Ok(1) => Ok(()),
        Ok(_) | Err(_) => Err(GPIO_IO_ERROR),
    }
}

/// Direct I/O is intentionally unsupported on targets without an `in`/`out` ABI.
#[cfg(not(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64"))))]
fn open_raw_io(_config: &ValidatedParallelConfig) -> Result<RawIoTransport, c_int> {
    Err(GPIO_UNSUPPORTED)
}

impl ParallelTransport for PpdevTransport {
    /// Submit one complete ppdev data byte.
    fn write_data(&mut self, data: u8) -> Result<(), c_int> {
        let mut data = data;
        // SAFETY: ppdev copies exactly one byte from the live local value.
        if unsafe {
            ffi::ioctl(
                self.file_descriptor,
                PPWDATA,
                (&mut data as *mut u8).cast::<()>() as usize,
            )
        } < 0
        {
            return Err(GPIO_IO_ERROR);
        }
        Ok(())
    }

    /// Sample one complete ppdev status byte.
    fn read_status(&mut self) -> Result<u8, c_int> {
        let mut status = 0_u8;
        // SAFETY: ppdev writes exactly one byte into the live local value.
        if unsafe {
            ffi::ioctl(
                self.file_descriptor,
                PPRSTATUS,
                (&mut status as *mut u8).cast::<()>() as usize,
            )
        } < 0
        {
            return Err(GPIO_IO_ERROR);
        }
        Ok(status)
    }
}

impl Drop for PpdevTransport {
    /// Release the ppdev claim before closing its descriptor.
    fn drop(&mut self) {
        // SAFETY: this transport uniquely owns the live descriptor.
        unsafe {
            ffi::ioctl(self.file_descriptor, PPRELEASE, 0);
            ffi::close(self.file_descriptor);
        }
        self.file_descriptor = -1;
    }
}

#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
impl ParallelTransport for RawIoTransport {
    /// Write the direct parallel data register.
    fn write_data(&mut self, data: u8) -> Result<(), c_int> {
        exact_raw_io_byte(self.device.write_at(&[data], u64::from(self.base)))
    }

    /// Read the direct parallel status register.
    fn read_status(&mut self) -> Result<u8, c_int> {
        let mut status = [0_u8];
        exact_raw_io_byte(self.device.read_at(&mut status, u64::from(self.base) + 1))?;
        Ok(status[0])
    }
}

/// Unsupported architectures never expose direct I/O through a usable handle.
#[cfg(not(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64"))))]
impl ParallelTransport for RawIoTransport {
    /// Report that no native direct-I/O operation exists on this architecture.
    fn write_data(&mut self, _data: u8) -> Result<(), c_int> {
        Err(GPIO_UNSUPPORTED)
    }

    /// Report that no native direct-I/O operation exists on this architecture.
    fn read_status(&mut self) -> Result<u8, c_int> {
        Err(GPIO_UNSUPPORTED)
    }
}

/// Open one configured parallel-port device.
pub(crate) extern "C" fn parallel_open(
    config: *const ParallelConfig,
    device: *mut *mut ParallelDevice,
) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: caller owns writable output storage.
    unsafe { *device = ptr::null_mut() };
    // SAFETY: public configuration is read-only and fully validated before opening host I/O.
    let Some(config) = (unsafe { ValidatedParallelConfig::from_ffi(config) }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let transport = match open_transport(&config) {
        Ok(transport) => transport,
        Err(error) => return error,
    };
    let device_value = Box::new(ParallelDevice::with_transport(config, transport));
    // SAFETY: ownership transfers to the caller until `parallel_close`.
    unsafe { *device = Box::into_raw(device_value) };
    GPIO_OK
}

/// Publish one parallel-port output action without touching host I/O.
pub(crate) extern "C" fn parallel_publish_outputs(
    device: *mut ParallelDevice,
    action: *const ParallelOutputAction,
) -> c_int {
    if device.is_null() || action.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: objects remain live for this call and publication uses only atomics.
    unsafe { (*device).publish_outputs(&*action) }
}

/// Publish one timed baseline-XOR parallel-port pulse without host I/O.
pub(crate) extern "C" fn parallel_publish_inverting_pulse(
    device: *mut ParallelDevice,
    action: *const ParallelInvertingPulseAction,
) -> c_int {
    if device.is_null() || action.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: pointers were checked and publication uses only lock-free atomics.
    unsafe { (*device).publish_inverting_pulse(&*action) }
}

/// Schedule independently expiring parallel-port inversions without host I/O.
pub(crate) extern "C" fn parallel_schedule_inverting_pulse(
    device: *mut ParallelDevice,
    action: *const ParallelScheduledInvertingPulseAction,
) -> c_int {
    if device.is_null() || action.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: pointers were checked and publication uses only lock-free atomics.
    unsafe { (*device).schedule_inverting_pulse(&*action) }
}

/// Service a parallel-port device from its sole non-real-time owner.
pub(crate) extern "C" fn parallel_service(device: *mut ParallelDevice) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: descriptor contract serializes this mutable service entry.
    unsafe { (*device).service() }
}

/// Apply an immediate control-plane data byte for a bounded programming sequence.
pub(crate) extern "C" fn parallel_control_write_data(
    device: *mut ParallelDevice,
    data: u32,
) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: control-plane callers serialize writes with the service owner.
    unsafe { (*device).control_write_data(data) }
}

/// Latch one legacy active-low binary channel selection through the parallel control plane.
pub(crate) extern "C" fn parallel_set_binary_channel(
    device: *mut ParallelDevice,
    channel: u8,
) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: control-plane callers serialize writes with the service owner.
    unsafe { (*device).set_binary_channel(channel) }
}

/// Program one established RTX radio through the serialized parallel control plane.
pub(crate) extern "C" fn parallel_program_rtx(
    device: *mut ParallelDevice,
    rx_frequency_hz: u32,
    tx_frequency_hz: u32,
    transmitting: u32,
    high_power: u32,
) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: control-plane callers serialize writes with the service owner.
    unsafe { (*device).program_rtx(rx_frequency_hz, tx_frequency_hz, transmitting, high_power) }
}

/// Clear RTX transmit immediately without scheduling a serial programming sequence.
pub(crate) extern "C" fn parallel_clear_rtx_transmit(device: *mut ParallelDevice) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: control-plane callers serialize writes with the service owner.
    unsafe { (*device).clear_rtx_transmit() }
}

/// Copy the latest parallel input snapshot without I/O.
pub(crate) extern "C" fn parallel_get_inputs(
    device: *const ParallelDevice,
    snapshot: *mut ParallelInputSnapshot,
) -> c_int {
    if device.is_null() || snapshot.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: output belongs to caller and input uses only atomics.
    unsafe { (*device).inputs(&mut *snapshot) }
}

/// Copy the latest parallel I/O counters without I/O.
pub(crate) extern "C" fn parallel_get_stats(
    device: *const ParallelDevice,
    stats: *mut ParallelStats,
) -> c_int {
    if device.is_null() || stats.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: output belongs to caller and statistics use only atomics.
    unsafe { (*device).stats(&mut *stats) }
}

/// Fail safe, close, and free one parallel-port handle.
pub(crate) extern "C" fn parallel_close(device: *mut ParallelDevice) {
    if device.is_null() {
        return;
    }
    // SAFETY: ownership transfers exactly once from C into this Box.
    let mut device = unsafe { Box::from_raw(device) };
    device.close();
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    use std::fs::{OpenOptions, remove_file};
    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    use std::os::unix::fs::FileExt;
    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Shared observations from one deterministic parallel transport.
    #[derive(Default)]
    struct FakeState {
        /// Data bytes accepted by the sole service owner.
        writes: Vec<u8>,
        /// Status results returned in service order.
        reads: VecDeque<Result<u8, c_int>>,
        /// Optional error returned by the next data write.
        next_write_error: Option<c_int>,
        /// Script individual writes to exercise failures after a partial protocol sequence.
        write_results: VecDeque<Result<(), c_int>>,
    }

    /// In-memory transport used to exercise the public lock-free boundary.
    struct FakeTransport {
        /// Test-owned state shared with assertions.
        state: Arc<Mutex<FakeState>>,
    }

    impl ParallelTransport for FakeTransport {
        /// Record one complete data byte or consume the scripted write failure.
        fn write_data(&mut self, data: u8) -> Result<(), c_int> {
            let mut state = self.state.lock().expect("fake transport state");
            state.write_results.pop_front().unwrap_or(Ok(()))?;
            if let Some(error) = state.next_write_error.take() {
                return Err(error);
            }
            state.writes.push(data);
            Ok(())
        }

        /// Consume one scripted status byte, defaulting to an idle byte.
        fn read_status(&mut self) -> Result<u8, c_int> {
            self.state
                .lock()
                .expect("fake transport state")
                .reads
                .pop_front()
                .unwrap_or(Ok(0xff))
        }
    }

    /// Shared observations from one deterministic ppdev/libc backend.
    #[cfg(target_os = "linux")]
    #[derive(Default)]
    struct FakePpdevState {
        /// Descriptor returned by the next synthetic open.
        open_result: c_int,
        /// Result returned while claiming the synthetic port.
        claim_result: c_int,
        /// Results returned by successive synthetic data writes.
        write_results: VecDeque<c_int>,
        /// Results returned by successive synthetic status reads.
        read_results: VecDeque<Result<u8, c_int>>,
        /// Flags and modes received by synthetic opens.
        opens: Vec<(c_int, c_int)>,
        /// Descriptors released by synthetic closes.
        closes: Vec<c_int>,
        /// Number of synthetic ppdev releases.
        release_count: u32,
        /// Data bytes written through the synthetic port.
        writes: Vec<u8>,
        /// Number of unexpected ioctl request values.
        unexpected_request_count: u32,
    }

    /// Deterministic ppdev implementation that never claims a host port.
    #[cfg(target_os = "linux")]
    struct FakePpdevBackend {
        /// Test-visible synthetic ppdev state.
        state: Arc<Mutex<FakePpdevState>>,
    }

    #[cfg(target_os = "linux")]
    impl ffi::TestParallelBackend for FakePpdevBackend {
        /// Record one synthetic open request.
        unsafe fn open(&mut self, _pathname: *const c_char, flags: c_int, mode: c_int) -> c_int {
            let mut state = self.state.lock().expect("fake ppdev state");
            state.opens.push((flags, mode));
            state.open_result
        }

        /// Record one synthetic descriptor close.
        unsafe fn close(&mut self, file_descriptor: c_int) -> c_int {
            self.state
                .lock()
                .expect("fake ppdev state")
                .closes
                .push(file_descriptor);
            0
        }

        /// Emulate the limited ppdev ioctl surface used by the adapter.
        unsafe fn ioctl(
            &mut self,
            _file_descriptor: c_int,
            request: c_ulong,
            argument: usize,
        ) -> c_int {
            let mut state = self.state.lock().expect("fake ppdev state");
            match request {
                PPCLAIM => state.claim_result,
                PPRELEASE => {
                    state.release_count += 1;
                    0
                }
                PPWDATA => {
                    let result = state.write_results.pop_front().unwrap_or(0);
                    if result < 0 {
                        return result;
                    }
                    // SAFETY: the adapter supplies a live pointer to one local byte.
                    let data = unsafe { (argument as *const u8).read() };
                    state.writes.push(data);
                    result
                }
                PPRSTATUS => match state.read_results.pop_front().unwrap_or(Ok(0xff)) {
                    Ok(status) => {
                        // SAFETY: the adapter supplies writable storage for one status byte.
                        unsafe { (argument as *mut u8).write(status) };
                        0
                    }
                    Err(error) => error,
                },
                _ => {
                    state.unexpected_request_count += 1;
                    -99
                }
            }
        }
    }

    /// Form one valid public configuration for an in-memory transport.
    fn ffi_config() -> ParallelConfig {
        ParallelConfig {
            struct_size: size_of::<ParallelConfig>() as u32,
            abi_version: ABI_VERSION,
            transport: PARALLEL_TRANSPORT_PPDEV,
            ppdev_path: c"/dev/parport0".as_ptr(),
            raw_io_base: 0,
            output_enable_mask: 0x0f,
            output_initial_mask: 0x01,
        }
    }

    /// Form a full eight-bit configuration for legacy binary and RTX protocols.
    fn protocol_config(initial_output_mask: u32) -> ParallelConfig {
        let mut config = ffi_config();
        config.output_enable_mask = u32::from(u8::MAX);
        config.output_initial_mask = initial_output_mask;
        config
    }

    /// Validate a test configuration without opening a kernel transport.
    fn validated(config: &ParallelConfig) -> ValidatedParallelConfig {
        // SAFETY: the test configuration stays live for the conversion.
        unsafe { ValidatedParallelConfig::from_ffi(config) }.expect("valid parallel config")
    }

    /// Construct a device with an observable fake transport.
    fn fake_device(config: &ParallelConfig) -> (ParallelDevice, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let transport = FakeTransport {
            state: Arc::clone(&state),
        };
        (
            ParallelDevice::with_transport(validated(config), Box::new(transport)),
            state,
        )
    }

    /// Install a successful ppdev backend with a nonzero synthetic descriptor.
    #[cfg(target_os = "linux")]
    fn install_fake_ppdev() -> (ffi::TestParallelBackendGuard, Arc<Mutex<FakePpdevState>>) {
        let state = Arc::new(Mutex::new(FakePpdevState {
            open_result: 42,
            ..FakePpdevState::default()
        }));
        let backend = FakePpdevBackend {
            state: Arc::clone(&state),
        };
        (ffi::install_test_parallel_backend(Box::new(backend)), state)
    }

    /// Form a valid tick-published action with no pulse by default.
    fn action(output_mask: u32) -> ParallelOutputAction {
        ParallelOutputAction {
            struct_size: size_of::<ParallelOutputAction>() as u32,
            abi_version: ABI_VERSION,
            output_mask,
            pulse_mask: 0,
            pulse_duration_milliseconds: 0,
            cancel_pulse: 0,
        }
    }

    /// Form a valid compatibility XOR pulse request.
    fn inverting_pulse(mask: u32, duration_milliseconds: u32) -> ParallelInvertingPulseAction {
        ParallelInvertingPulseAction {
            struct_size: size_of::<ParallelInvertingPulseAction>() as u32,
            abi_version: ABI_VERSION,
            invert_mask: mask,
            pulse_duration_milliseconds: duration_milliseconds,
            cancel_pulse: 0,
        }
    }

    /// Form an independently scheduled compatibility XOR pulse request.
    fn scheduled_inverting_pulse(
        invert_mask: u32,
        duration_milliseconds: u32,
        cancel_mask: u32,
    ) -> ParallelScheduledInvertingPulseAction {
        ParallelScheduledInvertingPulseAction {
            struct_size: size_of::<ParallelScheduledInvertingPulseAction>() as u32,
            abi_version: ABI_VERSION,
            invert_mask,
            pulse_duration_milliseconds: duration_milliseconds,
            cancel_mask,
        }
    }

    /// Exercise direct-I/O register offsets without accessing privileged hardware.
    #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn raw_io_transport_uses_the_configured_data_and_status_offsets() {
        let base = 0x378_u16;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rptadv-gpio-parallel-{}-{timestamp}",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("temporary raw-I/O image");
        file.set_len(u64::from(base) + 2)
            .expect("extend raw-I/O image");
        assert_eq!(
            file.write_at(&[0xa5], u64::from(base) + 1)
                .expect("seed status register"),
            1
        );
        let config = ValidatedParallelConfig {
            transport: PARALLEL_TRANSPORT_RAW_IO,
            ppdev_path: None,
            raw_io_base: Some(base),
            output_enable_mask: u8::MAX,
            output_initial_mask: 0,
        };
        let mut transport = open_raw_io_path(&config, &path).expect("open raw-I/O image");
        assert_eq!(transport.read_status(), Ok(0xa5));
        assert_eq!(transport.write_data(0x5a), Ok(()));
        let mut data = [0_u8];
        assert_eq!(
            file.read_at(&mut data, u64::from(base))
                .expect("read data register"),
            1
        );
        assert_eq!(data, [0x5a]);
        file.set_len(u64::from(base) + 1)
            .expect("truncate before status register");
        assert_eq!(transport.read_status(), Err(GPIO_IO_ERROR));
        assert_eq!(exact_raw_io_byte(Ok(0)), Err(GPIO_IO_ERROR));
        assert_eq!(
            exact_raw_io_byte(Err(std::io::Error::other("synthetic raw-I/O error"))),
            Err(GPIO_IO_ERROR)
        );
        let transport = box_parallel_transport(Ok(transport)).expect("box raw-I/O transport");
        drop(transport);
        drop(file);
        remove_file(path).expect("remove raw-I/O image");
    }

    /// Exercise ppdev open, service, immediate control write, and safe release.
    #[cfg(target_os = "linux")]
    #[test]
    fn ppdev_backend_opens_services_and_closes_through_the_descriptor() {
        let (_backend_guard, state) = install_fake_ppdev();
        let config = ffi_config();
        let mut device = ptr::null_mut();
        assert_eq!(parallel_open(&config, &mut device), GPIO_OK);
        assert!(!device.is_null());

        assert_eq!(parallel_publish_outputs(device, &action(0x02)), GPIO_OK);
        assert_eq!(parallel_service(device), GPIO_OK);
        assert_eq!(parallel_control_write_data(device, 0x03), GPIO_OK);
        parallel_close(device);

        let state = state.lock().expect("fake ppdev state");
        assert_eq!(state.opens, [(O_RDWR, 0)]);
        assert_eq!(state.writes, [0x02, 0x03, 0x00]);
        assert_eq!(state.release_count, 1);
        assert_eq!(state.closes, [42]);
        assert_eq!(state.unexpected_request_count, 0);
    }

    /// Preserve ppdev errors from open, claim, write, and status reads.
    #[cfg(target_os = "linux")]
    #[test]
    fn ppdev_backend_exposes_each_control_plane_and_service_error() {
        let (_backend_guard, state) = install_fake_ppdev();
        let config = ffi_config();
        let validated_config = validated(&config);
        state.lock().expect("fake ppdev state").open_result = -1;
        let mut device = ptr::null_mut();
        assert_eq!(parallel_open(&config, &mut device), GPIO_IO_ERROR);
        assert!(device.is_null());
        assert!(matches!(open_ppdev(&validated_config), Err(GPIO_IO_ERROR)));

        let mut state_guard = state.lock().expect("fake ppdev state");
        state_guard.open_result = 42;
        state_guard.claim_result = -1;
        drop(state_guard);
        assert!(matches!(open_ppdev(&validated_config), Err(GPIO_IO_ERROR)));

        let mut state_guard = state.lock().expect("fake ppdev state");
        state_guard.claim_result = 0;
        state_guard.write_results.push_back(-1);
        state_guard.read_results.push_back(Err(-1));
        drop(state_guard);
        let mut transport = open_ppdev(&validated_config).expect("claim synthetic ppdev");
        assert_eq!(transport.write_data(0x01), Err(GPIO_IO_ERROR));
        assert_eq!(transport.read_status(), Err(GPIO_IO_ERROR));
        drop(transport);

        let state = state.lock().expect("fake ppdev state");
        assert_eq!(state.closes, [42, 42]);
        assert_eq!(state.release_count, 1);
    }

    /// Reject malformed selection and unsafe data-bit assignments before host I/O.
    #[test]
    fn configuration_requires_an_explicit_safe_transport() {
        let mut config = ffi_config();
        config.abi_version = ABI_VERSION + 1;
        // SAFETY: this intentionally malformed test structure is readable.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.output_initial_mask = 0x10;
        // SAFETY: initial data pins must be enabled.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.output_enable_mask = u32::from(u8::MAX) + 1;
        // SAFETY: a raw data-register mask cannot exceed eight bits.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.output_initial_mask = u32::from(u8::MAX) + 1;
        // SAFETY: an initial data-register mask cannot exceed eight bits.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.ppdev_path = c"".as_ptr();
        // SAFETY: an empty device node is not a usable explicit selection.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.ppdev_path = c"parport0".as_ptr();
        // SAFETY: a device node must be an absolute path.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.transport = PARALLEL_TRANSPORT_RAW_IO;
        config.raw_io_base = 0;
        // SAFETY: raw I/O needs an explicit nonzero base.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config.raw_io_base = u32::from(u16::MAX);
        // SAFETY: the status register occupies the immediately following port.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config.raw_io_base = 0x378;
        // SAFETY: explicit raw-I/O selection accepts a complete two-register range.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_some());

        config = ffi_config();
        config.transport = PARALLEL_TRANSPORT_PPDEV;
        config.ppdev_path = ptr::null();
        // SAFETY: explicit ppdev selection requires a ppdev node.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.transport = 99;
        // SAFETY: unknown transport values are never interpreted heuristically.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.transport = PARALLEL_TRANSPORT_AUTO;
        config.ppdev_path = ptr::null();
        config.raw_io_base = 0x378;
        // SAFETY: automatic selection accepts a single explicit raw-I/O candidate.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_some());

        config.raw_io_base = 0;
        // SAFETY: automatic selection cannot use two absent explicit candidates.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_none());

        config = ffi_config();
        config.transport = PARALLEL_TRANSPORT_AUTO;
        // SAFETY: automatic selection may use the explicit ppdev candidate alone.
        assert!(unsafe { ValidatedParallelConfig::from_ffi(&config) }.is_some());

        let (device, _) = fake_device(&ffi_config());
        let mut malformed_action = action(0);
        malformed_action.struct_size = 0;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.abi_version = ABI_VERSION + 1;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.output_mask = u32::from(u8::MAX) + 1;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.pulse_mask = u32::from(u8::MAX) + 1;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.cancel_pulse = 2;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.pulse_mask = 1;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.pulse_duration_milliseconds = 1;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0x10);
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        malformed_action = action(0);
        malformed_action.pulse_mask = 0x10;
        malformed_action.pulse_duration_milliseconds = 1;
        assert_eq!(
            device.publish_outputs(&malformed_action),
            GPIO_INVALID_ARGUMENT
        );
        let mut conflicting_action = action(0);
        conflicting_action.cancel_pulse = 1;
        conflicting_action.pulse_mask = 1;
        assert_eq!(
            device.publish_outputs(&conflicting_action),
            GPIO_INVALID_ARGUMENT
        );
        conflicting_action.pulse_duration_milliseconds = 1;
        assert_eq!(
            device.publish_outputs(&conflicting_action),
            GPIO_INVALID_ARGUMENT
        );
    }

    /// Apply persistent output, pulse it deterministically, and publish input without a lock.
    #[test]
    fn service_applies_output_pulse_and_status_snapshots() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        state.lock().expect("fake transport state").reads.extend([
            Ok(0xa5),
            Ok(0x5a),
            Ok(0x3c),
            Ok(0x3c),
            Ok(0x3c),
        ]);
        let start = Instant::now();

        assert_eq!(device.service_at(start), GPIO_OK);
        let mut request = action(0x04);
        request.pulse_mask = 0x02;
        request.pulse_duration_milliseconds = 10;
        assert_eq!(device.publish_outputs(&request), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);
        let mut cancel = action(0x04);
        cancel.cancel_pulse = 1;
        assert_eq!(device.publish_outputs(&cancel), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(2)), GPIO_OK);
        assert_eq!(device.publish_outputs(&request), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(3)), GPIO_OK);
        assert_eq!(
            device.service_at(start + Duration::from_millis(14)),
            GPIO_OK
        );

        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0x01, 0x06, 0x04, 0x06, 0x04]
        );
        let mut inputs = ParallelInputSnapshot {
            struct_size: size_of::<ParallelInputSnapshot>() as u32,
            abi_version: 0,
            online: 0,
            status_mask: 0,
        };
        assert_eq!(device.inputs(&mut inputs), GPIO_OK);
        assert_eq!(inputs.abi_version, ABI_VERSION);
        assert_eq!(inputs.online, 1);
        assert_eq!(inputs.status_mask, 0x3c);
    }

    /// Preserve the legacy XOR pulse behavior through cancellation and expiry.
    #[test]
    fn inverting_pulse_flips_the_parallel_baseline_and_restores_it() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        let start = Instant::now();

        assert_eq!(device.service_at(start), GPIO_OK);
        let request = inverting_pulse(0x03, 10);
        assert_eq!(device.publish_inverting_pulse(&request), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);
        let cancel = ParallelInvertingPulseAction {
            struct_size: size_of::<ParallelInvertingPulseAction>() as u32,
            abi_version: ABI_VERSION,
            invert_mask: 0,
            pulse_duration_milliseconds: 0,
            cancel_pulse: 1,
        };
        assert_eq!(device.publish_inverting_pulse(&cancel), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(2)), GPIO_OK);
        assert_eq!(device.publish_inverting_pulse(&request), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(3)), GPIO_OK);
        assert_eq!(
            device.service_at(start + Duration::from_millis(14)),
            GPIO_OK
        );
        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0x01, 0x02, 0x01, 0x02, 0x01]
        );
    }

    /// Keep per-bit parallel XOR deadlines independent through overlap and cancellation.
    #[test]
    fn scheduled_inverting_pulses_overlap_and_expire_independently() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        let start = Instant::now();

        assert_eq!(device.service_at(start), GPIO_OK);
        assert_eq!(
            device.schedule_inverting_pulse(&scheduled_inverting_pulse(0x02, 10, 0)),
            GPIO_OK
        );
        assert_eq!(
            device.schedule_inverting_pulse(&scheduled_inverting_pulse(0x04, 20, 0)),
            GPIO_OK
        );
        assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);
        assert_eq!(
            device.schedule_inverting_pulse(&scheduled_inverting_pulse(0, 0, 0x02)),
            GPIO_OK
        );
        assert_eq!(device.service_at(start + Duration::from_millis(2)), GPIO_OK);
        assert_eq!(
            device.service_at(start + Duration::from_millis(21)),
            GPIO_OK
        );

        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0x01, 0x07, 0x05, 0x01]
        );
    }

    /// Reject malformed per-bit parallel schedules before the service owner sees them.
    #[test]
    fn scheduled_inverting_pulse_validates_selected_bits_and_deadlines() {
        let config = ffi_config();
        let (device, _) = fake_device(&config);
        let mut request = scheduled_inverting_pulse(0x02, 10, 0);
        request.struct_size = 0;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.struct_size = size_of::<ParallelScheduledInvertingPulseAction>() as u32;
        request.abi_version = ABI_VERSION + 1;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.abi_version = ABI_VERSION;
        request.invert_mask = u32::from(u8::MAX) + 1;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.invert_mask = 0x10;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.invert_mask = 0x02;
        request.cancel_mask = 0x02;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.cancel_mask = 0;
        request.pulse_duration_milliseconds = 0;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.invert_mask = 0;
        request.cancel_mask = 0x10;
        assert_eq!(
            device.schedule_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.cancel_mask = 0;
        assert_eq!(device.schedule_inverting_pulse(&request), GPIO_OK);
    }

    /// Reject malformed XOR requests before they reach the service owner.
    #[test]
    fn inverting_pulse_validates_mask_duration_and_cancel_contract() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        let mut request = inverting_pulse(0x02, 10);
        request.struct_size = 0;
        assert_eq!(
            device.publish_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.struct_size = size_of::<ParallelInvertingPulseAction>() as u32;
        request.abi_version = ABI_VERSION + 1;
        assert_eq!(
            device.publish_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.abi_version = ABI_VERSION;
        request.invert_mask = u32::from(u8::MAX) + 1;
        assert_eq!(
            device.publish_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.invert_mask = 0x10;
        assert_eq!(
            device.publish_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.invert_mask = 0x02;
        request.pulse_duration_milliseconds = 0;
        assert_eq!(
            device.publish_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.pulse_duration_milliseconds = 10;
        request.cancel_pulse = 1;
        assert_eq!(
            device.publish_inverting_pulse(&request),
            GPIO_INVALID_ARGUMENT
        );
        request.invert_mask = 0;
        request.pulse_duration_milliseconds = 0;
        assert_eq!(device.publish_inverting_pulse(&request), GPIO_OK);

        assert_eq!(device.service(), GPIO_OK);
        assert_eq!(state.lock().expect("fake transport state").writes, [0x01]);
    }

    /// Preserve the newest action after an I/O fault and expose all error state atomically.
    #[test]
    fn service_retries_after_io_failure_and_reports_statistics() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        state.lock().expect("fake transport state").next_write_error = Some(-71);
        let start = Instant::now();
        assert_eq!(device.service_at(start), GPIO_IO_ERROR);
        assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);
        state
            .lock()
            .expect("fake transport state")
            .reads
            .push_back(Err(-72));
        assert_eq!(
            device.service_at(start + Duration::from_millis(2)),
            GPIO_IO_ERROR
        );

        let mut stats = ParallelStats {
            struct_size: size_of::<ParallelStats>() as u32,
            abi_version: 0,
            input_read_count: 0,
            output_apply_count: 0,
            io_error_count: 0,
            online: 0,
            last_io_error: 0,
            applied_output_mask: 0,
        };
        assert_eq!(device.stats(&mut stats), GPIO_OK);
        assert_eq!(stats.input_read_count, 2);
        assert_eq!(stats.output_apply_count, 2);
        assert_eq!(stats.io_error_count, 2);
        assert_eq!(stats.last_io_error, -72);
        assert_eq!(stats.applied_output_mask, 0x01);
    }

    /// Retain direct control-plane writes while never allowing a native tick to use them.
    #[test]
    fn control_write_updates_the_next_service_baseline_and_close_is_safe() {
        let config = ffi_config();
        let (mut device, state) = fake_device(&config);
        let start = Instant::now();
        assert_eq!(device.service_at(start), GPIO_OK);
        assert_eq!(
            device.control_write_data(u32::from(u8::MAX) + 1),
            GPIO_INVALID_ARGUMENT
        );
        state.lock().expect("fake transport state").next_write_error = Some(-71);
        assert_eq!(device.control_write_data(0x08), GPIO_IO_ERROR);
        assert_eq!(device.control_write_data(0x08), GPIO_OK);
        assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);
        assert_eq!(device.control_write_data(0x10), GPIO_INVALID_ARGUMENT);
        device.close();

        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0x01, 0x08, 0x00]
        );
        let mut inputs = ParallelInputSnapshot {
            struct_size: size_of::<ParallelInputSnapshot>() as u32,
            abi_version: 0,
            online: 1,
            status_mask: 0,
        };
        assert_eq!(device.inputs(&mut inputs), GPIO_OK);
        assert_eq!(inputs.online, 0);
    }

    /// Preserve the established RTX frequency-word calculation for both radio bands.
    #[test]
    fn rtx_word_calculation_matches_the_legacy_protocol() {
        assert_eq!(
            rtx_words(146_940_000, 146_340_000, false),
            RtxWords {
                reference: 6_401,
                synthesizer: 108_896,
            }
        );
        assert_eq!(
            rtx_words(146_940_000, 146_340_000, true),
            RtxWords {
                reference: 6_401,
                synthesizer: 117_032,
            }
        );
        assert_eq!(
            rtx_words(444_500_000, 449_500_000, false),
            RtxWords {
                reference: 2_563,
                synthesizer: 135_280,
            }
        );
    }

    /// Keep the first serial transfer's long settle and later transfers' short settle.
    #[test]
    fn rtx_settle_state_and_busy_spin_interval_match_the_legacy_protocol() {
        let initialized = AtomicBool::new(false);
        assert_eq!(rtx_initial_delay_multiplier_for(&initialized), 200);
        assert_eq!(rtx_initial_delay_multiplier_for(&initialized), 4);
        rtx_delay(0);
        rtx_delay(1);
    }

    /// Shift each RTX word most-significant-bit first with the established latch timing.
    #[test]
    fn rtx_serial_word_preserves_bit_order_and_settling_intervals() {
        let word = 0x5_4321;
        let mut output = 0xe0;
        let mut writes = Vec::new();
        let mut delays = Vec::new();
        let mut write = |data| {
            writes.push(data);
            Ok(())
        };
        let mut delay = |multiplier| delays.push(multiplier);

        assert_eq!(
            rtx_shift_word(&mut output, word, 200, &mut write, &mut delay),
            Ok(())
        );
        assert_eq!(writes.len(), 64);
        assert_eq!(delays.len(), 62);
        assert_eq!(delays[0], 200);
        assert!(delays[1..].iter().all(|&multiplier| multiplier == 1));
        assert_eq!(writes[0], 0xe0);
        for (index, bit_index) in (0..RTX_REGISTER_BITS).rev().enumerate() {
            let data = if word & (1_u32 << bit_index) != 0 {
                0xe2
            } else {
                0xe0
            };
            let offset = 1 + index * 3;
            assert_eq!(writes[offset], data);
            assert_eq!(writes[offset + 1], data | RTX_CLOCK_MASK);
            assert_eq!(writes[offset + 2], data);
        }
        assert_eq!(&writes[61..], &[0xe0, 0xe4, 0xe0]);
        assert_eq!(output, 0xe0);

        let mut output = 0;
        let mut write = |_| Err(-91);
        let mut delay = |_| panic!("failed writes cannot advance serial timing");
        assert_eq!(
            rtx_shift_word(&mut output, word, 4, &mut write, &mut delay),
            Err(-91)
        );
    }

    /// Preserve active-low channel selection, RTX transmit polarity, and immediate TX clear.
    #[test]
    fn legacy_binary_and_rtx_operations_preserve_parallel_output_behavior() {
        let config = protocol_config(0x0f);
        let (device, state) = fake_device(&config);
        assert_eq!(device.set_binary_channel(5), GPIO_OK);
        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0xff, 0xaf]
        );

        let config = protocol_config(0xe0);
        let (device, state) = fake_device(&config);
        let mut delay = |_| {};
        assert_eq!(
            device.program_rtx_with_delay(146_940_000, 146_340_000, 1, 1, &mut delay),
            GPIO_OK
        );
        let state = state.lock().expect("fake transport state");
        let writes = &state.writes;
        assert_eq!(writes.len(), 129);
        assert_eq!(writes[0], 0xe0);
        assert_eq!(writes[128], 0xe8);
        drop(state);

        let config = protocol_config(0xff);
        let (device, state) = fake_device(&config);
        let mut delay = |_| {};
        assert_eq!(
            device.program_rtx_with_delay(444_500_000, 449_500_000, 0, 1, &mut delay),
            GPIO_OK
        );
        assert_eq!(
            state.lock().expect("fake transport state").writes[128] & 0x18,
            0
        );

        let config = protocol_config(0xff);
        let (device, state) = fake_device(&config);
        assert_eq!(device.clear_rtx_transmit(), GPIO_OK);
        assert_eq!(state.lock().expect("fake transport state").writes, [0xe7]);
    }

    /// Reject unavailable protocol pins but preserve the legacy zero-receive no-op.
    #[test]
    fn protocol_write_failures_stop_before_later_control_bytes() {
        for failing_write in [0, 1] {
            let (device, state) = fake_device(&protocol_config(0));
            state.lock().expect("fake transport state").write_results =
                std::iter::repeat_n(Ok(()), failing_write)
                    .chain([Err(-5)])
                    .collect();
            assert_eq!(device.set_binary_channel(5), GPIO_IO_ERROR);
            assert_eq!(
                state.lock().expect("fake transport state").writes.len(),
                failing_write
            );
        }
        for failing_write in [0, 64, 128] {
            let (device, state) = fake_device(&protocol_config(0));
            state.lock().expect("fake transport state").write_results =
                std::iter::repeat_n(Ok(()), failing_write)
                    .chain([Err(-5)])
                    .collect();
            assert_eq!(
                device.program_rtx(146_940_000, 146_340_000, 0, 0),
                GPIO_IO_ERROR
            );
            assert_eq!(
                state.lock().expect("fake transport state").writes.len(),
                failing_write
            );
        }
        for transmitting in [0, 1] {
            let (device, state) = fake_device(&protocol_config(0));
            assert_eq!(
                device.program_rtx(146_940_000, 146_340_000, transmitting, 0),
                GPIO_OK
            );
            let state = state.lock().expect("fake transport state");
            assert_eq!(state.writes.len(), 129);
            assert_eq!(
                state.writes[128] & RTX_TRANSMIT_MASK != 0,
                transmitting != 0
            );
        }
        let (device, state) = fake_device(&protocol_config(0xff));
        state.lock().expect("fake transport state").next_write_error = Some(-5);
        assert_eq!(device.clear_rtx_transmit(), GPIO_IO_ERROR);
        assert!(
            state
                .lock()
                .expect("fake transport state")
                .writes
                .is_empty()
        );
    }

    /// Validate pulse widths and null arguments without publishing any transport operation.
    #[test]
    fn pulse_noops_and_remaining_invalid_fields_do_not_publish_io() {
        let (mut device, state) = fake_device(&ffi_config());
        let mut pulse = inverting_pulse(0, 0);
        pulse.cancel_pulse = 2;
        assert_eq!(
            device.publish_inverting_pulse(&pulse),
            GPIO_INVALID_ARGUMENT
        );
        pulse.cancel_pulse = 1;
        pulse.invert_mask = 1;
        assert_eq!(
            device.publish_inverting_pulse(&pulse),
            GPIO_INVALID_ARGUMENT
        );
        pulse.cancel_pulse = 0;
        pulse.invert_mask = 0;
        assert_eq!(device.publish_inverting_pulse(&pulse), GPIO_OK);
        assert_eq!(device.pulse_generation.load(Ordering::Acquire), 0);
        assert_eq!(
            parallel_publish_inverting_pulse(ptr::null_mut(), &pulse),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_publish_inverting_pulse(&mut device, ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        let mut scheduled = scheduled_inverting_pulse(0, 0, 256);
        assert_eq!(
            device.schedule_inverting_pulse(&scheduled),
            GPIO_INVALID_ARGUMENT
        );
        scheduled.cancel_mask = 0;
        scheduled.pulse_duration_milliseconds = 1;
        assert_eq!(
            device.schedule_inverting_pulse(&scheduled),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_schedule_inverting_pulse(ptr::null_mut(), &scheduled),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_schedule_inverting_pulse(&mut device, ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        assert!(
            state
                .lock()
                .expect("fake transport state")
                .writes
                .is_empty()
        );
    }

    /// Reject unavailable protocol pins but preserve the legacy zero-receive no-op.
    #[test]
    fn legacy_binary_and_rtx_operations_validate_protocol_pin_ownership() {
        let config = ffi_config();
        let (device, _) = fake_device(&config);
        assert_eq!(device.set_binary_channel(5), GPIO_INVALID_ARGUMENT);
        assert_eq!(device.program_rtx(0, 146_340_000, 0, 0), GPIO_OK);
        assert_eq!(
            device.program_rtx(146_940_000, 146_340_000, 0, 0),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(device.clear_rtx_transmit(), GPIO_INVALID_ARGUMENT);
    }

    /// Exercise C descriptor paths with a fake handle without entering host I/O.
    #[test]
    fn ffi_operates_a_fake_parallel_device_and_transfers_ownership() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        let device = Box::into_raw(Box::new(device));
        let request = action(0x02);
        assert_eq!(
            parallel_publish_outputs(device, ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(parallel_publish_outputs(device, &request), GPIO_OK);
        assert_eq!(parallel_service(device), GPIO_OK);
        assert_eq!(
            parallel_set_binary_channel(device, 5),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(parallel_program_rtx(device, 0, 146_340_000, 0, 0), GPIO_OK);
        assert_eq!(parallel_clear_rtx_transmit(device), GPIO_INVALID_ARGUMENT);
        let pulse = inverting_pulse(0x02, 1);
        assert_eq!(parallel_publish_inverting_pulse(device, &pulse), GPIO_OK);
        assert_eq!(parallel_service(device), GPIO_OK);
        let scheduled = scheduled_inverting_pulse(0, 0, 0);
        assert_eq!(
            parallel_schedule_inverting_pulse(device, &scheduled),
            GPIO_OK
        );

        let mut inputs = ParallelInputSnapshot {
            struct_size: size_of::<ParallelInputSnapshot>() as u32,
            abi_version: 0,
            online: 0,
            status_mask: 0,
        };
        let mut stats = ParallelStats {
            struct_size: size_of::<ParallelStats>() as u32,
            abi_version: 0,
            input_read_count: 0,
            output_apply_count: 0,
            io_error_count: 0,
            online: 0,
            last_io_error: 0,
            applied_output_mask: 0,
        };
        assert_eq!(
            parallel_get_inputs(device, ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_get_stats(device, ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(parallel_get_inputs(device, &mut inputs), GPIO_OK);
        assert_eq!(parallel_get_stats(device, &mut stats), GPIO_OK);
        assert_eq!(inputs.online, 1);
        assert_eq!(stats.applied_output_mask, 0x00);

        parallel_close(device);
        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0x02, 0x00, 0x00]
        );
    }

    /// Exercise per-bit parallel pulse publication through the exported C ABI.
    #[test]
    fn ffi_schedules_a_parallel_baseline_xor_pulse() {
        let config = ffi_config();
        let (device, state) = fake_device(&config);
        let device = Box::into_raw(Box::new(device));
        let request = scheduled_inverting_pulse(0x02, 10, 0);

        assert_eq!(parallel_schedule_inverting_pulse(device, &request), GPIO_OK);
        assert_eq!(parallel_service(device), GPIO_OK);
        parallel_close(device);
        assert_eq!(
            state.lock().expect("fake transport state").writes,
            [0x03, 0x00]
        );
    }

    /// Reject undersized snapshots and cover OS-selection failures without real GPIO hardware.
    #[test]
    fn accessors_and_transport_selection_report_invalid_or_unavailable_resources() {
        let config = ffi_config();
        let (device, _) = fake_device(&config);
        let mut inputs = ParallelInputSnapshot {
            struct_size: 0,
            abi_version: 0,
            online: 0,
            status_mask: 0,
        };
        let mut stats = ParallelStats {
            struct_size: 0,
            abi_version: 0,
            input_read_count: 0,
            output_apply_count: 0,
            io_error_count: 0,
            online: 0,
            last_io_error: 0,
            applied_output_mask: 0,
        };
        assert_eq!(device.inputs(&mut inputs), GPIO_INVALID_ARGUMENT);
        assert_eq!(device.stats(&mut stats), GPIO_INVALID_ARGUMENT);

        let ppdev = validated(&config);
        let ppdev_without_path = ValidatedParallelConfig {
            transport: PARALLEL_TRANSPORT_PPDEV,
            ppdev_path: None,
            #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
            raw_io_base: None,
            output_enable_mask: 0xff,
            output_initial_mask: 0,
        };
        assert!(matches!(
            open_ppdev(&ppdev_without_path),
            Err(GPIO_INVALID_ARGUMENT)
        ));
        #[cfg(target_os = "linux")]
        let (_backend_guard, backend_state) = install_fake_ppdev();
        #[cfg(target_os = "linux")]
        assert!(open_transport(&ppdev).is_ok());
        let raw = ValidatedParallelConfig {
            transport: PARALLEL_TRANSPORT_RAW_IO,
            ppdev_path: None,
            #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
            raw_io_base: Some(0x378),
            output_enable_mask: 0xff,
            output_initial_mask: 0,
        };
        let _ = open_transport(&raw);
        let automatic = ValidatedParallelConfig {
            transport: PARALLEL_TRANSPORT_AUTO,
            ppdev_path: Some(c"/dev/null".to_owned()),
            #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
            raw_io_base: Some(0x378),
            output_enable_mask: 0xff,
            output_initial_mask: 0,
        };
        #[cfg(target_os = "linux")]
        assert!(open_transport(&automatic).is_ok());
        #[cfg(target_os = "linux")]
        {
            backend_state.lock().expect("fake ppdev state").open_result = -1;
            let _ = open_transport(&automatic);
        }
        let unsupported = ValidatedParallelConfig {
            transport: 99,
            ppdev_path: None,
            #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
            raw_io_base: None,
            output_enable_mask: 0,
            output_initial_mask: 0,
        };
        assert!(matches!(
            open_transport(&unsupported),
            Err(GPIO_INVALID_ARGUMENT)
        ));

        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        let no_base = ValidatedParallelConfig {
            transport: PARALLEL_TRANSPORT_RAW_IO,
            ppdev_path: None,
            raw_io_base: None,
            output_enable_mask: 0xff,
            output_initial_mask: 0,
        };
        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        assert!(matches!(
            open_raw_io_path(&no_base, Path::new("/definitely-missing-rptadv-port")),
            Err(GPIO_INVALID_ARGUMENT)
        ));
    }

    /// Validate C-facing null and malformed inputs without opening real hardware.
    #[test]
    fn ffi_rejects_invalid_handles_and_configurations() {
        let config = ffi_config();
        let mut device = ptr::null_mut();
        assert_eq!(
            parallel_open(ptr::null(), &mut device),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_open(&config, ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        let mut invalid_config = config;
        invalid_config.struct_size = 0;
        assert_eq!(
            parallel_open(&invalid_config, &mut device),
            GPIO_INVALID_ARGUMENT
        );
        assert!(device.is_null());
        assert_eq!(
            parallel_publish_outputs(ptr::null_mut(), ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_publish_inverting_pulse(ptr::null_mut(), ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_schedule_inverting_pulse(ptr::null_mut(), ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(parallel_service(ptr::null_mut()), GPIO_INVALID_ARGUMENT);
        assert_eq!(
            parallel_control_write_data(ptr::null_mut(), 0),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_set_binary_channel(ptr::null_mut(), 0),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_program_rtx(ptr::null_mut(), 0, 0, 0, 0),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_clear_rtx_transmit(ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_get_inputs(ptr::null(), ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            parallel_get_stats(ptr::null(), ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        parallel_close(ptr::null_mut());
    }
}
