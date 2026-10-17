//! Hardware-neutral, setup-sized GPIO contexts. Transport I/O is control-only.
//!
//! Port handles borrow their context. Stop all audio users before retirement or
//! destruction. Neither successful submission nor software gating proves that
//! a physical interlock has released an output.

use crate::{GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK, GPIO_UNSUPPORTED};
use std::{
    alloc::{Layout, alloc},
    cell::UnsafeCell,
    ffi::{c_int, c_void},
    mem::size_of,
    ptr, slice,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
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
/// Waiting for fresh, matching physical arm feedback; outputs remain safe.
pub(crate) const ARMING: u32 = 1;
/// Current-epoch outputs may be admitted.
pub(crate) const ARMED: u32 = 2;
/// Latched failure; restored health does not restore admission.
pub(crate) const FAULTED: u32 = 3;
const INITIALIZED: u32 = 4;
const ARM_FEEDBACK: u32 = 8;
const SAFE_FAILED: u32 = 1 << 24;
const FAULT_HEALTH: u32 = 1;
const FAULT_TRANSPORT: u32 = 2;
const FAULT_TIMEOUT: u32 = 3;
const FAULT_EPOCH: u32 = 4;
const FAULT_TIME: u32 = 5;

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

/// A single producer publishes one epoch and its complete action atomically.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct OutputIntent {
    pub(crate) struct_size: u32,
    pub(crate) epoch: u32,
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

/// Context-owned admission; borrowed ports never outlive this stable allocation.
struct Admission {
    word: AtomicU64,
}

/// Opaque stable endpoint. Setup allocation is the only source of port-count limits.
pub(crate) struct Port {
    config: PortConfig,
    admission: *const Admission,
    ready: AtomicU32,
    retired: AtomicU32,
    intent: AtomicU64,
    observation: AtomicU64,
}

/// Control-owned context; only snapshots and prepared publication may run concurrently.
pub(crate) struct Context {
    ports: Vec<Port>,
    admission: Box<Admission>,
    transport: Transport,
    control: UnsafeCell<Control>,
}

/// All mutable transport scratch belongs exclusively to one non-audio owner.
struct Control {
    appliance: bool,
    arm_timeout_ns: u64,
    maximum_age_ns: u64,
    last_time_ns: Option<u64>,
    arm_started_ns: u64,
    arm_deadline_ns: u64,
    arm_epoch: u32,
    safety: SafetyInput,
    inputs: Vec<PortInput>,
    outputs: Vec<PortOutput>,
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

/// Stable control-owned storage with an explicit allocation error at the C boundary.
fn boxed<T>(value: T) -> Result<Box<T>, c_int> {
    let pointer = unsafe { alloc(Layout::new::<T>()) }.cast::<T>();
    if pointer.is_null() {
        return Err(GPIO_IO_ERROR);
    }
    // The global allocator/layout match Box; initialized ownership is transferred once.
    unsafe {
        pointer.write(value);
        Ok(Box::from_raw(pointer))
    }
}

impl Port {
    /// Context destruction requires all borrowed port users to be quiescent.
    fn admission(&self) -> &Admission {
        unsafe { &*self.admission }
    }
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
    let admission = match boxed(Admission {
        word: AtomicU64::new((1_u64 << 32) | u64::from(DISARMED)),
    }) {
        Ok(value) => value,
        Err(error) => return error,
    };
    for config in configs {
        if let Err(e) = valid_port(config, appliance) {
            return e;
        }
        if ports.iter().any(|p| p.config.port_id == config.port_id) {
            return GPIO_INVALID_ARGUMENT;
        }
        ports.push(Port {
            config: *config,
            admission: ptr::from_ref(&*admission),
            ready: AtomicU32::new(0),
            retired: AtomicU32::new(0),
            intent: AtomicU64::new(0),
            observation: AtomicU64::new(0),
        });
    }
    let mut inputs = match storage(config.port_count) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut outputs = match storage(config.port_count) {
        Ok(v) => v,
        Err(e) => return e,
    };
    inputs.resize(config.port_count, PortInput::default());
    outputs.resize(config.port_count, PortOutput::default());
    if unsafe { transport.safe_outputs.expect("validated")(transport.context) } != GPIO_OK {
        return GPIO_IO_ERROR;
    }
    let context = match boxed(Context {
        ports,
        admission,
        transport,
        control: UnsafeCell::new(Control {
            appliance,
            arm_timeout_ns: config.arm_timeout_ns,
            maximum_age_ns: config.maximum_snapshot_age_ns,
            last_time_ns: None,
            arm_started_ns: 0,
            arm_deadline_ns: 0,
            arm_epoch: 0,
            safety: SafetyInput::default(),
            inputs,
            outputs,
        }),
    }) {
        Ok(value) => value,
        Err(error) => return error,
    };
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
            fault: (admission as u32 >> 16) & 255,
            arm_feedback: u32::from(admission as u32 & ARM_FEEDBACK != 0),
            safe_submission_failed: u32::from(admission as u32 & SAFE_FAILED != 0),
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
    let admission = port.admission().word.load(Ordering::Acquire);
    let epoch = (admission >> 32) as u32;
    let intent = port.intent.load(Ordering::Acquire);
    // Revocation clears INITIALIZED before any transport call or observation reset.
    let observation =
        if admission as u32 & INITIALIZED != 0 && port.retired.load(Ordering::Acquire) == 0 {
            port.observation.load(Ordering::Acquire)
        } else {
            0
        };
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
            requested_outputs: if admission as u32 & 3 == ARMED
                && (intent >> 32) as u32 == epoch
                && port.retired.load(Ordering::Acquire) == 0
            {
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

impl Context {
    /// Invalidate admission before any potentially stalled transport operation.
    fn revoke(&self, state: u32, fault: u32) -> bool {
        let old = self.admission.word.load(Ordering::Acquire);
        let epoch = (old >> 32) as u32;
        let next = epoch.checked_add(1);
        let (epoch, state, fault) = match next {
            Some(epoch) => (epoch, state, fault),
            None => (epoch, FAULTED, FAULT_EPOCH),
        };
        self.admission.word.store(
            (u64::from(epoch) << 32)
                | u64::from(state | (fault << 16) | (old as u32 & (HEALTH_ALL << 8))),
            Ordering::Release,
        );
        for port in &self.ports {
            port.ready.store(0, Ordering::Release);
            port.observation.store(0, Ordering::Release);
        }
        next.is_some()
    }

    /// Submission is not readback. A failed attempt remains explicitly visible.
    fn safe(&self) -> c_int {
        let result =
            unsafe { self.transport.safe_outputs.expect("validated")(self.transport.context) };
        if result != GPIO_OK {
            let word = self.admission.word.load(Ordering::Acquire);
            let reason = if word as u32 & (255 << 16) == 0 {
                FAULT_TRANSPORT << 16
            } else {
                0
            };
            self.admission.word.store(
                word | u64::from(FAULTED | SAFE_FAILED | reason),
                Ordering::Release,
            );
            return GPIO_IO_ERROR;
        }
        GPIO_OK
    }

    /// Latch a fault, invalidate readiness and requests, then attempt hardware release.
    fn fault(&self, reason: u32) -> c_int {
        self.revoke(FAULTED, reason);
        self.safe();
        GPIO_IO_ERROR
    }

    /// Sample into preallocated scratch; a prior successful read cannot mask failure.
    fn sample(&self, control: &mut Control, now: u64) -> Result<(), u32> {
        if control.last_time_ns.is_some_and(|last| now < last) {
            return Err(FAULT_TIME);
        }
        control.last_time_ns = Some(now);
        control.safety = SafetyInput {
            struct_size: size_of::<SafetyInput>() as u32,
            ..Default::default()
        };
        for input in &mut control.inputs {
            *input = PortInput {
                struct_size: size_of::<PortInput>() as u32,
                ..Default::default()
            };
        }
        if unsafe {
            self.transport.sample.expect("validated")(
                self.transport.context,
                now,
                &mut control.safety,
                control.inputs.as_mut_ptr(),
                control.inputs.len(),
            )
        } != GPIO_OK
        {
            return Err(FAULT_TRANSPORT);
        }
        let safety = control.safety;
        let required_health = if control.appliance { HEALTH_ALL } else { 16 };
        let word = self.admission.word.load(Ordering::Acquire);
        self.admission.word.store(
            (word & !u64::from((HEALTH_ALL << 8) | ARM_FEEDBACK))
                | u64::from((safety.health & HEALTH_ALL) << 8)
                | if control.appliance && safety.arm_feedback == 1 {
                    u64::from(ARM_FEEDBACK)
                } else {
                    0
                },
            Ordering::Release,
        );
        if safety.struct_size < size_of::<SafetyInput>() as u32
            || safety.valid != 1
            || safety.health & required_health != required_health
            || safety.arm_feedback > 1
            || now
                .checked_sub(safety.sampled_at_ns)
                .is_none_or(|age| age > control.maximum_age_ns)
        {
            return Err(FAULT_HEALTH);
        }
        for (port, input) in self.ports.iter().zip(&control.inputs) {
            if port.retired.load(Ordering::Acquire) != 0 {
                continue;
            }
            let caps = port.config.capabilities;
            if input.struct_size < size_of::<PortInput>() as u32
                || input.present != 1
                || input.valid_inputs & port.config.required_inputs != port.config.required_inputs
                || input.inputs & !caps.inputs != 0
                || input.valid_inputs & !caps.inputs != 0
                || input.confirmed_valid & !caps.readback != 0
            {
                return Err(FAULT_HEALTH);
            }
            port.observation.store(
                u64::from(input.present | (input.inputs << 1) | (input.valid_inputs << 3))
                    | (u64::from(input.confirmed_outputs & caps.outputs) << 32)
                    | (u64::from(input.confirmed_valid) << 40),
                Ordering::Release,
            );
        }
        Ok(())
    }

    /// Initialize only through explicit control action; recovery never requests arm.
    fn initialize(&self, now: u64) -> c_int {
        if !self.revoke(DISARMED, 0) {
            self.safe();
            return GPIO_IO_ERROR;
        }
        if self.safe() != GPIO_OK {
            return GPIO_IO_ERROR;
        }
        // SAFETY: all control entry points require one externally serialized owner.
        let control = unsafe { &mut *self.control.get() };
        if let Err(reason) = self.sample(control, now) {
            return self.fault(reason);
        }
        self.admission
            .word
            .fetch_or(u64::from(INITIALIZED), Ordering::Release);
        GPIO_OK
    }

    fn all_ready(&self, epoch: u32) -> bool {
        self.ports.iter().all(|p| {
            p.retired.load(Ordering::Acquire) != 0 || p.ready.load(Ordering::Acquire) == epoch
        })
    }

    /// Confirming an arm transaction creates a fresh output epoch, not old PTT replay.
    fn admit(&self) -> c_int {
        let old = self.admission.word.load(Ordering::Acquire);
        let Some(epoch) = ((old >> 32) as u32).checked_add(1) else {
            return self.fault(FAULT_EPOCH);
        };
        for p in &self.ports {
            if p.retired.load(Ordering::Acquire) == 0 {
                p.ready.store(epoch, Ordering::Release);
            }
        }
        self.admission.word.store(
            (u64::from(epoch) << 32) | (old & u64::from(!3_u32)) | u64::from(ARMED),
            Ordering::Release,
        );
        GPIO_OK
    }
}

/// Explicit startup initialization. A latched fault requires context_recover.
pub(crate) unsafe extern "C" fn context_initialize(context: *mut Context, now: u64) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if context.admission.word.load(Ordering::Acquire) as u32 & 3 != DISARMED {
        return GPIO_UNSUPPORTED;
    }
    context.initialize(now)
}

/// Confirm completed radio/session/processor warm-up on the sole control owner.
pub(crate) unsafe extern "C" fn port_ready(port: *const Port, epoch: u32) -> c_int {
    let Some(port) = (unsafe { port.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let word = port.admission().word.load(Ordering::Acquire);
    if word as u32 & (3 | INITIALIZED) != INITIALIZED
        || (word >> 32) as u32 != epoch
        || port.retired.load(Ordering::Acquire) != 0
    {
        return GPIO_UNSUPPORTED;
    }
    port.ready.store(epoch, Ordering::Release);
    GPIO_OK
}

/// Request physical arm only after every retained port confirms current readiness.
pub(crate) unsafe extern "C" fn context_request_arm(
    context: *mut Context,
    epoch: u32,
    now: u64,
) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let word = context.admission.word.load(Ordering::Acquire);
    if word as u32 & (3 | INITIALIZED) != INITIALIZED
        || (word >> 32) as u32 != epoch
        || !context.all_ready(epoch)
    {
        return GPIO_UNSUPPORTED;
    }
    let control = unsafe { &mut *context.control.get() };
    if !control.appliance {
        return GPIO_UNSUPPORTED;
    }
    if let Err(reason) = context.sample(control, now) {
        return context.fault(reason);
    }
    let Some(deadline) = now.checked_add(control.arm_timeout_ns) else {
        return context.fault(FAULT_TIME);
    };
    control.arm_started_ns = now;
    control.arm_deadline_ns = deadline;
    control.arm_epoch = epoch;
    context
        .admission
        .word
        .fetch_or(u64::from(ARMING), Ordering::Release);
    if unsafe {
        context
            .transport
            .request_arm
            .expect("appliance setup validated")(context.transport.context, epoch)
    } != GPIO_OK
    {
        return context.fault(FAULT_TRANSPORT);
    }
    GPIO_OK
}

/// Disarm and invalidate old intent before transport I/O; caller serializes control.
pub(crate) unsafe extern "C" fn context_disarm(context: *mut Context) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let advanced = context.revoke(DISARMED, 0);
    let result = context.safe();
    if !advanced { GPIO_IO_ERROR } else { result }
}

/// Repeat initialization after a fault without automatically requesting physical arm.
pub(crate) unsafe extern "C" fn context_recover(context: *mut Context, now: u64) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if matches!(
        context.admission.word.load(Ordering::Acquire) as u32 & 3,
        ARMED | ARMING
    ) {
        return GPIO_UNSUPPORTED;
    }
    context.initialize(now)
}

/// Bounded audio publication. Stale stores can race revocation but cannot be admitted.
pub(crate) unsafe extern "C" fn port_publish(
    port: *const Port,
    action: *const OutputIntent,
) -> c_int {
    let Some(port) = (unsafe { port.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if !unsafe { fits(action) } {
        return GPIO_INVALID_ARGUMENT;
    }
    let action = unsafe { *action };
    if action.outputs & !port.config.capabilities.outputs != 0
        || action.outputs & (OUTPUT_PTT | OUTPUT_CTCSS) == OUTPUT_CTCSS
    {
        return GPIO_INVALID_ARGUMENT;
    }
    let word = port.admission().word.load(Ordering::Acquire);
    if word as u32 & 3 != ARMED
        || (word >> 32) as u32 != action.epoch
        || port.ready.load(Ordering::Acquire) != action.epoch
        || port.retired.load(Ordering::Acquire) != 0
    {
        return GPIO_UNSUPPORTED;
    }
    port.intent.store(
        (u64::from(action.epoch) << 32) | u64::from(action.outputs),
        Ordering::Release,
    );
    GPIO_OK
}

/// Service samples and applies prepared intent without ever involving audio owners.
pub(crate) unsafe extern "C" fn context_service(context: *mut Context, now: u64) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let word = context.admission.word.load(Ordering::Acquire);
    if word as u32 & 3 == FAULTED {
        return GPIO_IO_ERROR;
    }
    if word as u32 & INITIALIZED == 0 {
        return GPIO_UNSUPPORTED;
    }
    let control = unsafe { &mut *context.control.get() };
    if let Err(reason) = context.sample(control, now) {
        return context.fault(reason);
    }
    let state = word as u32 & 3;
    if control.appliance && state == ARMING {
        if now >= control.arm_deadline_ns {
            return context.fault(FAULT_TIMEOUT);
        }
        if control.safety.arm_feedback == 1
            && control.safety.arm_epoch == control.arm_epoch
            && control.safety.sampled_at_ns > control.arm_started_ns
            && context.admit() != GPIO_OK
        {
            return GPIO_IO_ERROR;
        }
    } else if control.appliance && state == ARMED {
        if control.safety.arm_feedback != 1 || control.safety.arm_epoch != control.arm_epoch {
            return context.fault(FAULT_HEALTH);
        }
    } else if !control.appliance
        && state == DISARMED
        && context.all_ready((word >> 32) as u32)
        && context.admit() != GPIO_OK
    {
        return GPIO_IO_ERROR;
    }
    let admission = context.admission.word.load(Ordering::Acquire);
    let epoch = (admission >> 32) as u32;
    for (port, output) in context.ports.iter().zip(&mut control.outputs) {
        let intent = port.intent.load(Ordering::Acquire);
        let admitted = admission as u32 & 3 == ARMED
            && port.retired.load(Ordering::Acquire) == 0
            && port.ready.load(Ordering::Acquire) == epoch
            && (intent >> 32) as u32 == epoch;
        *output = PortOutput {
            struct_size: size_of::<PortOutput>() as u32,
            port_id: port.config.port_id,
            outputs: if admitted {
                intent as u32
            } else {
                OUTPUT_MUTES & port.config.capabilities.outputs
            },
        };
    }
    if unsafe {
        context.transport.apply_outputs.expect("validated")(
            context.transport.context,
            control.outputs.as_ptr(),
            control.outputs.len(),
        )
    } != GPIO_OK
    {
        return context.fault(FAULT_TRANSPORT);
    }
    GPIO_OK
}

/// Retire after quiescing that port's audio owner. Shared context is disarmed first.
pub(crate) unsafe extern "C" fn port_retire(context: *mut Context, id: u64, epoch: u32) -> c_int {
    let Some(context) = (unsafe { context.as_ref() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if (context.admission.word.load(Ordering::Acquire) >> 32) as u32 != epoch {
        return GPIO_UNSUPPORTED;
    }
    let Some(port) = context.ports.iter().find(|p| p.config.port_id == id) else {
        return GPIO_UNSUPPORTED;
    };
    context.revoke(DISARMED, 0);
    port.retired.store(1, Ordering::Release);
    context.safe()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::context_tests::{Hardware, Runtime};

    #[test]
    fn context_stale_epoch_races_cannot_replay_a_late_store() {
        let mut hardware = Hardware::default();
        let runtime = Runtime::new(&mut hardware, 1, true);
        let old = runtime.arm(&mut hardware, 1, 100);
        assert_eq!(unsafe { context_disarm(runtime.0) }, GPIO_OK);
        let new = runtime.arm(&mut hardware, 1, 200);
        assert_ne!(old, new);
        // A producer paused after admission load resumes after recovery.
        let port = unsafe { &*runtime.port(0) };
        port.intent.store(
            (u64::from(old) << 32) | u64::from(OUTPUT_PTT | OUTPUT_CTCSS),
            Ordering::Release,
        );
        assert_eq!(unsafe { context_service(runtime.0, 202) }, GPIO_OK);
        let mut status = PortSnapshot {
            struct_size: size_of::<PortSnapshot>() as u32,
            ..Default::default()
        };
        assert_eq!(unsafe { port_snapshot(port, &mut status) }, GPIO_OK);
        assert_eq!(status.requested_outputs, OUTPUT_MUTES);
        port.ready.store(old, Ordering::Release);
        assert_eq!(unsafe { context_service(runtime.0, 203) }, GPIO_OK);
        assert_ne!(
            unsafe {
                port_publish(
                    port,
                    &OutputIntent {
                        struct_size: size_of::<OutputIntent>() as u32,
                        epoch: new,
                        outputs: OUTPUT_PTT,
                    },
                )
            },
            GPIO_OK
        );
    }

    #[test]
    fn context_epoch_exhaustion_never_reuses_a_token() {
        let mut hardware = Hardware::default();
        let runtime = Runtime::new(&mut hardware, 1, true);
        let context = unsafe { &*runtime.0 };
        context.admission.word.store(
            (u64::from(u32::MAX) << 32) | u64::from(DISARMED),
            Ordering::Release,
        );
        assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_IO_ERROR);
        assert_eq!(runtime.snapshot().state, FAULTED);
        assert_eq!(runtime.snapshot().epoch, u32::MAX);
        assert_eq!(runtime.snapshot().fault, FAULT_EPOCH);
        assert_eq!(unsafe { context_recover(runtime.0, 101) }, GPIO_IO_ERROR);
        assert_eq!(runtime.snapshot().epoch, u32::MAX);
    }

    #[test]
    fn context_admission_exhaustion_and_retired_publication_stay_safe() {
        for appliance in [false, true] {
            let mut hardware = Hardware::default();
            let runtime = Runtime::new(&mut hardware, 1, appliance);
            let context = unsafe { &*runtime.0 };
            assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_OK);
            if appliance {
                runtime.arm(&mut hardware, 1, 100);
            }
            // Admit from the last epoch; there is no wraparound token available.
            context.admission.word.store(
                (u64::from(u32::MAX) << 32)
                    | u64::from(INITIALIZED | if appliance { ARMING } else { DISARMED }),
                Ordering::Release,
            );
            context.ports[0].ready.store(u32::MAX, Ordering::Release);
            assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_IO_ERROR);
            assert_eq!(runtime.snapshot().state, FAULTED);
            assert_eq!(unsafe { context_disarm(runtime.0) }, GPIO_IO_ERROR);
            unsafe { (*context.control.get()).last_time_ns = None };
            context.admission.word.store(
                (2_u64 << 32) | u64::from(ARMED | INITIALIZED),
                Ordering::Release,
            );
            context.ports[0].ready.store(2, Ordering::Release);
            context.ports[0].retired.store(1, Ordering::Release);
            context.ports[0]
                .intent
                .store((2_u64 << 32) | u64::from(OUTPUT_PTT), Ordering::Release);
            let mut snapshot = PortSnapshot {
                struct_size: size_of::<PortSnapshot>() as u32,
                ..Default::default()
            };
            assert_eq!(
                unsafe { port_snapshot(&context.ports[0], &mut snapshot) },
                GPIO_OK
            );
            assert_eq!(snapshot.requested_outputs, OUTPUT_MUTES);
            assert_eq!(
                unsafe {
                    port_publish(
                        &context.ports[0],
                        &OutputIntent {
                            struct_size: size_of::<OutputIntent>() as u32,
                            epoch: 2,
                            outputs: OUTPUT_PTT,
                        },
                    )
                },
                GPIO_UNSUPPORTED
            );
        }
    }
}
