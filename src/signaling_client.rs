//! Validated, dynamically linked signaling ABI. Hardware and clocks stay here.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::size_of;
use std::ptr;
use std::sync::OnceLock;
use std::time::Instant;

use crate::GPIO_INVALID_ARGUMENT;

#[repr(C)]
pub(crate) struct PulseState {
    _private: [u8; 0],
}

pub(crate) type Write = extern "C" fn(*mut c_void, u8) -> c_int;
pub(crate) type Delay = extern "C" fn(*mut c_void, u32);

#[repr(C)]
pub(crate) struct Descriptor {
    struct_size: u32,
    abi_version: u32,
    capability_name: *const c_char,
    create_pulse: extern "C" fn(*mut *mut PulseState) -> c_int,
    destroy_pulse: extern "C" fn(*mut PulseState),
    publish_timed: extern "C" fn(*const PulseState, u32, u32, u32) -> c_int,
    publish_scheduled: extern "C" fn(*const PulseState, u32, u32, u32) -> c_int,
    evaluate: extern "C" fn(*mut PulseState, u64, u32) -> u32,
    pub(crate) set_binary_channel: extern "C" fn(u8, u8, *mut c_void, Write) -> c_int,
    pub(crate) program_rtx:
        extern "C" fn(u8, u32, u32, u32, u32, *mut c_void, Write, Delay) -> c_int,
    pub(crate) clear_rtx_transmit: extern "C" fn(u8, *mut c_void, Write) -> c_int,
}

unsafe impl Sync for Descriptor {}

#[link(name = "rptadvsignaling")]
unsafe extern "C" {
    fn rptadv_signaling_descriptor() -> *const Descriptor;
}

static DESCRIPTOR: OnceLock<Option<&'static Descriptor>> = OnceLock::new();

/// Resolve and validate once during device setup, never in an audio callback.
pub(crate) fn descriptor() -> Result<&'static Descriptor, c_int> {
    DESCRIPTOR
        .get_or_init(|| {
            // SAFETY: the linked DSO owns this immutable process-lifetime table.
            let descriptor = unsafe { rptadv_signaling_descriptor().as_ref() }?;
            if descriptor.struct_size < size_of::<Descriptor>() as u32
                || descriptor.abi_version != 1
                || descriptor.capability_name.is_null()
                // SAFETY: the DSO ABI promises a NUL-terminated capability string.
                || unsafe { CStr::from_ptr(descriptor.capability_name) }.to_bytes()
                    != b"rptadv.signaling"
            {
                return None;
            }
            Some(descriptor)
        })
        .ok_or(GPIO_INVALID_ARGUMENT)
}

/// Per-device opaque pulse ownership and adapter monotonic origin.
pub(crate) struct Pulse {
    descriptor: &'static Descriptor,
    state: *mut PulseState,
    origin: Instant,
}

impl Pulse {
    pub(crate) fn new() -> Result<Self, c_int> {
        let descriptor = descriptor()?;
        let mut state = ptr::null_mut();
        let status = (descriptor.create_pulse)(&mut state);
        if status != 0 || state.is_null() {
            return Err(if status == 0 {
                GPIO_INVALID_ARGUMENT
            } else {
                status
            });
        }
        Ok(Self {
            descriptor,
            state,
            origin: Instant::now(),
        })
    }

    pub(crate) fn publish_timed(&self, mask: u32, duration: u32, invert: bool) -> c_int {
        (self.descriptor.publish_timed)(self.state, mask, duration, u32::from(invert))
    }

    pub(crate) fn publish_scheduled(&self, schedule: u32, cancel: u32, duration: u32) -> c_int {
        (self.descriptor.publish_scheduled)(self.state, schedule, cancel, duration)
    }

    pub(crate) fn evaluate(&self, now: Instant, baseline: u32) -> u32 {
        let nanoseconds = now.saturating_duration_since(self.origin).as_nanos();
        (self.descriptor.evaluate)(
            self.state,
            nanoseconds.min(u128::from(u64::MAX)) as u64,
            baseline,
        )
    }

    pub(crate) fn descriptor(&self) -> &'static Descriptor {
        self.descriptor
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        (self.descriptor.destroy_pulse)(self.state);
    }
}

// A handle has concurrent atomic publishers and exactly one service owner.
unsafe impl Send for Pulse {}
unsafe impl Sync for Pulse {}
