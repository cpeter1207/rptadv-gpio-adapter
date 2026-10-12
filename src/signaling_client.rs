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
struct DescriptorPrefix {
    struct_size: u32,
    abi_version: u32,
    capability_name: *const c_char,
}

/// ABI mirror: nullable C function pointers are not valid Rust function pointers.
#[repr(C)]
struct RawDescriptor {
    prefix: DescriptorPrefix,
    create_pulse: Option<extern "C" fn(*mut *mut PulseState) -> c_int>,
    destroy_pulse: Option<extern "C" fn(*mut PulseState)>,
    publish_timed: Option<extern "C" fn(*const PulseState, u32, u32, u32) -> c_int>,
    publish_scheduled: Option<extern "C" fn(*const PulseState, u32, u32, u32) -> c_int>,
    evaluate: Option<extern "C" fn(*mut PulseState, u64, u32) -> u32>,
    set_binary_channel: Option<extern "C" fn(u8, u8, *mut c_void, Write) -> c_int>,
    program_rtx: Option<extern "C" fn(u8, u32, u32, u32, u32, *mut c_void, Write, Delay) -> c_int>,
    clear_rtx_transmit: Option<extern "C" fn(u8, *mut c_void, Write) -> c_int>,
}

/// Cached operations have been checked for presence at setup.
#[derive(Clone, Copy)]
pub(crate) struct Descriptor {
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

#[link(name = "rptadvsignaling")]
unsafe extern "C" {
    fn rptadv_signaling_descriptor() -> *const DescriptorPrefix;
}

static DESCRIPTOR: OnceLock<Option<Descriptor>> = OnceLock::new();

/// Check only the fixed prefix before reading any trailing function pointers.
fn validate_descriptor(raw: *const DescriptorPrefix) -> Option<Descriptor> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: even a short provider table must advertise its first size word.
    let advertised_size = unsafe { *raw.cast::<u32>() };
    if advertised_size < size_of::<DescriptorPrefix>() as u32 {
        return None;
    }
    // SAFETY: the advertised size covers the entire fixed prefix.
    let prefix = unsafe { &*raw };
    if advertised_size < size_of::<RawDescriptor>() as u32 || prefix.abi_version != 1
        || prefix.capability_name.is_null()
        // SAFETY: a non-null provider identity is a terminated C string.
        || unsafe { CStr::from_ptr(prefix.capability_name) }.to_bytes() != b"rptadv.signaling"
    {
        return None;
    }
    // SAFETY: the validated size covers every field in the ABI mirror.
    let raw = unsafe { &*(raw as *const RawDescriptor) };
    Some(Descriptor {
        create_pulse: raw.create_pulse?,
        destroy_pulse: raw.destroy_pulse?,
        publish_timed: raw.publish_timed?,
        publish_scheduled: raw.publish_scheduled?,
        evaluate: raw.evaluate?,
        set_binary_channel: raw.set_binary_channel?,
        program_rtx: raw.program_rtx?,
        clear_rtx_transmit: raw.clear_rtx_transmit?,
    })
}

/// Resolve and validate once during device setup, never in an audio callback.
pub(crate) fn descriptor() -> Result<&'static Descriptor, c_int> {
    DESCRIPTOR
        .get_or_init(|| {
            // SAFETY: the linked DSO owns this immutable process-lifetime table.
            validate_descriptor(unsafe { rptadv_signaling_descriptor() })
        })
        .as_ref()
        .ok_or(GPIO_INVALID_ARGUMENT)
}

/// Per-device opaque pulse ownership and adapter monotonic origin.
pub(crate) struct Pulse {
    descriptor: Descriptor,
    state: *mut PulseState,
    origin: Instant,
}

impl Pulse {
    pub(crate) fn new() -> Result<Self, c_int> {
        Self::with_descriptor(*descriptor()?)
    }

    fn with_descriptor(descriptor: Descriptor) -> Result<Self, c_int> {
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

    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    extern "C" fn create_error(_out: *mut *mut PulseState) -> c_int {
        -7
    }

    extern "C" fn create_null(out: *mut *mut PulseState) -> c_int {
        // SAFETY: the test supplies writable output storage.
        unsafe { *out = ptr::null_mut() };
        0
    }

    #[test]
    fn validates_prefix_identity_and_every_required_operation() {
        let operations = *descriptor().expect("released ABI");
        let mut raw = RawDescriptor {
            prefix: DescriptorPrefix {
                struct_size: size_of::<RawDescriptor>() as u32,
                abi_version: 1,
                capability_name: c"rptadv.signaling".as_ptr(),
            },
            create_pulse: Some(operations.create_pulse),
            destroy_pulse: Some(operations.destroy_pulse),
            publish_timed: Some(operations.publish_timed),
            publish_scheduled: Some(operations.publish_scheduled),
            evaluate: Some(operations.evaluate),
            set_binary_channel: Some(operations.set_binary_channel),
            program_rtx: Some(operations.program_rtx),
            clear_rtx_transmit: Some(operations.clear_rtx_transmit),
        };
        let raw_ptr = (&raw as *const RawDescriptor).cast::<DescriptorPrefix>();
        assert!(validate_descriptor(ptr::null()).is_none());
        assert!(validate_descriptor(raw_ptr).is_some());

        let size_only = size_of::<u32>() as u32;
        assert!(validate_descriptor((&size_only as *const u32).cast()).is_none());

        raw.prefix.struct_size -= 1;
        assert!(validate_descriptor(raw_ptr).is_none());
        raw.prefix.struct_size += 1;
        raw.prefix.abi_version = 2;
        assert!(validate_descriptor(raw_ptr).is_none());
        raw.prefix.abi_version = 1;
        raw.prefix.capability_name = ptr::null();
        assert!(validate_descriptor(raw_ptr).is_none());
        raw.prefix.capability_name = c"wrong.signaling".as_ptr();
        assert!(validate_descriptor(raw_ptr).is_none());
        raw.prefix.capability_name = c"rptadv.signaling".as_ptr();

        macro_rules! missing {
            ($field:ident) => {{
                raw.$field = None;
                assert!(validate_descriptor(raw_ptr).is_none(), stringify!($field));
                raw.$field = Some(operations.$field);
            }};
        }
        missing!(create_pulse);
        missing!(destroy_pulse);
        missing!(publish_timed);
        missing!(publish_scheduled);
        missing!(evaluate);
        missing!(set_binary_channel);
        missing!(program_rtx);
        missing!(clear_rtx_transmit);
        assert!(validate_descriptor(raw_ptr).is_some());
    }

    #[test]
    fn pulse_creation_rejects_status_or_null_handle() {
        let mut operations = *descriptor().expect("released ABI");
        operations.create_pulse = create_error;
        assert!(matches!(Pulse::with_descriptor(operations), Err(-7)));
        operations.create_pulse = create_null;
        assert!(matches!(
            Pulse::with_descriptor(operations),
            Err(GPIO_INVALID_ARGUMENT)
        ));
    }
}
