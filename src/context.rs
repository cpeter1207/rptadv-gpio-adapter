//! Hardware-neutral, setup-sized GPIO contexts. Transport I/O is control-only.
//!
//! Port handles borrow their context. Stop all audio users before retirement or
//! destruction. Neither successful submission nor software gating proves that
//! a physical interlock has released an output.

use crate::{GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK, GPIO_UNSUPPORTED};
use std::{
    ffi::{c_int, c_void},
    mem::size_of,
    ptr, slice,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

/// Normalized input signal bits, not port indices or physical pad numbers.
pub(crate) const INPUT_COR: u32 = 1;
pub(crate) const INPUT_CTCSS: u32 = 2;
/// Prepared output bits.
pub(crate) const OUTPUT_PTT: u32 = 1;
pub(crate) const OUTPUT_CTCSS: u32 = 2;
pub(crate) const OUTPUT_MUTES: u32 = 12;
pub(crate) const OUTPUT_SITE: u32 = 16;
/// Arm feedback, watchdog, TX-A mute, TX-B mute, and site-output gate.
pub(crate) const SAFETY_ALL: u32 = 31;
/// Input power, main power, reset release, watchdog, and transport health.
pub(crate) const HEALTH_ALL: u32 = 31;
/// No outputs admitted; initialization and explicit arming remain necessary.
pub(crate) const DISARMED: u32 = 0;

/// Per-endpoint actual facilities. Raw pin masks do not imply normalized signals.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct PortCapabilities {
    pub(crate) struct_size: u32,
    pub(crate) inputs: u32,
    pub(crate) outputs: u32,
    pub(crate) input_pin_mask: u64,
    pub(crate) output_pin_mask: u64,
    pub(crate) readback: u32,
    pub(crate) safety: u32,
}

/// Copied setup record. Port IDs are arbitrary identities, never a fixed bitmap.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PortConfig {
    pub(crate) struct_size: u32,
    pub(crate) port_id: u64,
    pub(crate) capabilities: PortCapabilities,
    pub(crate) required_inputs: u32,
    pub(crate) required_outputs: u32,
}

/// Context geometry and timing supplied by the real transport contract.
#[repr(C)]
pub(crate) struct ContextConfig {
    pub(crate) struct_size: u32,
    pub(crate) safety_contract: u32,
    pub(crate) ports: *const PortConfig,
    pub(crate) port_count: usize,
    pub(crate) arm_timeout_ns: u64,
    pub(crate) maximum_snapshot_age_ns: u64,
}

/// Fresh normalized common-interlock observation from the transport owner.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct SafetyInput {
    pub(crate) struct_size: u32,
    pub(crate) sampled_at_ns: u64,
    pub(crate) valid: u32,
    pub(crate) health: u32,
    pub(crate) arm_feedback: u32,
    pub(crate) arm_epoch: u32,
}

/// Transport reports configured ports in setup order; unconfigured slots are absent.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct PortInput {
    pub(crate) struct_size: u32,
    pub(crate) present: u32,
    pub(crate) inputs: u32,
    pub(crate) valid_inputs: u32,
    pub(crate) confirmed_outputs: u32,
    pub(crate) confirmed_valid: u32,
}

/// One preallocated output request supplied to control-only transport code.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct PortOutput {
    pub(crate) struct_size: u32,
    pub(crate) port_id: u64,
    pub(crate) outputs: u32,
}

/// Borrowed transport vtable; it and its context outlive the GPIO context.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Transport {
    pub(crate) struct_size: u32,
    pub(crate) context: *mut c_void,
    pub(crate) sample: Option<
        unsafe extern "C" fn(*mut c_void, u64, *mut SafetyInput, *mut PortInput, usize) -> c_int,
    >,
    pub(crate) request_arm: Option<unsafe extern "C" fn(*mut c_void, u32) -> c_int>,
    pub(crate) apply_outputs:
        Option<unsafe extern "C" fn(*mut c_void, *const PortOutput, usize) -> c_int>,
    pub(crate) safe_outputs: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
}

/// Callback-readable state/epoch are one word; observations never authorize outputs.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct SafetySnapshot {
    pub(crate) struct_size: u32,
    pub(crate) state: u32,
    pub(crate) epoch: u32,
    pub(crate) health: u32,
    pub(crate) fault: u32,
    pub(crate) arm_feedback: u32,
    pub(crate) safe_submission_failed: u32,
}

/// Readback is independent of requested output; writes never fabricate feedback.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct PortSnapshot {
    pub(crate) struct_size: u32,
    pub(crate) port_id: u64,
    pub(crate) epoch: u32,
    pub(crate) present: u32,
    pub(crate) ready: u32,
    pub(crate) retired: u32,
    pub(crate) inputs: u32,
    pub(crate) valid_inputs: u32,
    pub(crate) requested_outputs: u32,
    pub(crate) confirmed_outputs: u32,
    pub(crate) confirmed_valid: u32,
}

/// Shared admission survives moving a control-owned Context value during setup.
struct Admission {
    word: AtomicU64,
}

/// Opaque stable endpoint. Setup allocation is the only source of port-count limits.
pub(crate) struct Port {
    config: PortConfig,
    admission: Arc<Admission>,
    ready: AtomicU32,
    retired: AtomicU32,
    intent: AtomicU64,
    observation: AtomicU64,
}

/// Control-owned context; only snapshots and prepared publication may run concurrently.
pub(crate) struct Context {
    ports: Vec<Port>,
    admission: Arc<Admission>,
    transport: Transport,
}

/// Inspect only the size prefix before accessing a sized record.
pub(crate) unsafe fn fits<T>(record: *const T) -> bool {
    !record.is_null() && unsafe { *record.cast::<u32>() as usize >= size_of::<T>() }
}

/// Allocate without panicking on capacity/admission failure.
fn storage<T>(count: usize) -> Result<Vec<T>, c_int> {
    let mut storage = Vec::new();
    storage
        .try_reserve_exact(count)
        .map_err(|_| GPIO_IO_ERROR)?;
    Ok(storage)
}

/// Setup-only validation uses signal sets independently of the number of ports.
fn valid_port(port: &PortConfig, appliance: bool) -> Result<(), c_int> {
    let caps = port.capabilities;
    if port.struct_size < size_of::<PortConfig>() as u32
        || caps.struct_size < size_of::<PortCapabilities>() as u32
        || caps.inputs & !(INPUT_COR | INPUT_CTCSS) != 0
        || caps.outputs & !(OUTPUT_PTT | OUTPUT_CTCSS | OUTPUT_MUTES | OUTPUT_SITE) != 0
        || caps.readback & !caps.outputs != 0
        || caps.safety & !SAFETY_ALL != 0
        || caps.input_pin_mask & caps.output_pin_mask != 0
    {
        return Err(GPIO_INVALID_ARGUMENT);
    }
    if port.required_inputs & !caps.inputs != 0
        || port.required_outputs & !caps.outputs != 0
        || (appliance && caps.safety != SAFETY_ALL)
    {
        return Err(GPIO_UNSUPPORTED);
    }
    Ok(())
}

/// Preallocate a context and submit safe outputs before publishing its handle.
pub(crate) unsafe extern "C" fn context_create(
    config: *const ContextConfig,
    transport: *const Transport,
    output: *mut *mut Context,
) -> c_int {
    if output.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    unsafe { *output = ptr::null_mut() };
    if !unsafe { fits(config) && fits(transport) } {
        return GPIO_INVALID_ARGUMENT;
    }
    let config = unsafe { &*config };
    let transport = unsafe { *transport };
    if config.safety_contract > 1
        || config.ports.is_null()
        || config.port_count == 0
        || config.maximum_snapshot_age_ns == 0
        || transport.sample.is_none()
        || transport.apply_outputs.is_none()
        || transport.safe_outputs.is_none()
    {
        return GPIO_INVALID_ARGUMENT;
    }
    let appliance = config.safety_contract == 1;
    if appliance && (transport.request_arm.is_none() || config.arm_timeout_ns == 0) {
        return GPIO_UNSUPPORTED;
    }
    let mut ports = match storage::<Port>(config.port_count) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let configs = unsafe { slice::from_raw_parts(config.ports, config.port_count) };
    let admission = Arc::new(Admission {
        word: AtomicU64::new((1_u64 << 32) | u64::from(DISARMED)),
    });
    for config in configs {
        if let Err(e) = valid_port(config, appliance) {
            return e;
        }
        if ports.iter().any(|p| p.config.port_id == config.port_id) {
            return GPIO_INVALID_ARGUMENT;
        }
        ports.push(Port {
            config: *config,
            admission: Arc::clone(&admission),
            ready: AtomicU32::new(0),
            retired: AtomicU32::new(0),
            intent: AtomicU64::new(0),
            observation: AtomicU64::new(0),
        });
    }
    if unsafe { transport.safe_outputs.expect("validated")(transport.context) } != GPIO_OK {
        return GPIO_IO_ERROR;
    }
    let context = Box::new(Context {
        ports,
        admission,
        transport,
    });
    unsafe { *output = Box::into_raw(context) };
    GPIO_OK
}

/// Borrow a stable configured port handle; lifetime ends with context destruction.
pub(crate) unsafe extern "C" fn context_get_port(
    context: *const Context,
    id: u64,
    output: *mut *const Port,
) -> c_int {
    if output.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    unsafe { *output = ptr::null() };
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let Some(port) = context.ports.iter().find(|port| port.config.port_id == id) else {
        return GPIO_UNSUPPORTED;
    };
    unsafe { *output = port };
    GPIO_OK
}

/// Bounded snapshot loads; epoch and safety state are observed together.
pub(crate) unsafe extern "C" fn context_snapshot(
    context: *const Context,
    output: *mut SafetySnapshot,
) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if !unsafe { fits(output) } {
        return GPIO_INVALID_ARGUMENT;
    }
    let admission = context.admission.word.load(Ordering::Acquire);
    unsafe {
        *output = SafetySnapshot {
            struct_size: size_of::<SafetySnapshot>() as u32,
            state: (admission as u32) & 3,
            epoch: (admission >> 32) as u32,
            health: (admission as u32 >> 8) & HEALTH_ALL,
            ..Default::default()
        }
    };
    GPIO_OK
}

/// Read one port without transport calls, allocation, locks or retry loops.
pub(crate) unsafe extern "C" fn port_snapshot(
    port: *const Port,
    output: *mut PortSnapshot,
) -> c_int {
    let Some(port) = (unsafe { port.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if !unsafe { fits(output) } {
        return GPIO_INVALID_ARGUMENT;
    }
    let admission = port.admission.word.load(Ordering::Acquire);
    let epoch = (admission >> 32) as u32;
    let intent = port.intent.load(Ordering::Acquire);
    let observation = port.observation.load(Ordering::Acquire);
    unsafe {
        *output = PortSnapshot {
            struct_size: size_of::<PortSnapshot>() as u32,
            port_id: port.config.port_id,
            epoch,
            present: observation as u32 & 1,
            ready: u32::from(port.ready.load(Ordering::Acquire) == epoch),
            retired: port.retired.load(Ordering::Acquire),
            inputs: (observation as u32 >> 1) & 3,
            valid_inputs: (observation as u32 >> 3) & 3,
            requested_outputs: if (intent >> 32) as u32 == epoch {
                intent as u32
            } else {
                OUTPUT_MUTES & port.config.capabilities.outputs
            },
            confirmed_outputs: (observation >> 32) as u32 & 31,
            confirmed_valid: (observation >> 40) as u32 & 31,
        }
    };
    GPIO_OK
}

/// Quiesce all borrowed port users before destruction. Safe submission is best effort.
pub(crate) unsafe extern "C" fn context_destroy(context: *mut Context) {
    if context.is_null() {
        return;
    }
    let context = unsafe { Box::from_raw(context) };
    context.admission.word.store(0, Ordering::Release);
    unsafe { context.transport.safe_outputs.expect("validated")(context.transport.context) };
}
