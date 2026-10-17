use crate::{GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK, GPIO_UNSUPPORTED, context::*};
use std::{ffi::c_void, mem::size_of, ptr, slice};

thread_local! { static CHECK_ALLOCATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
thread_local! { static FAIL_ALLOCATION: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
static CALLBACK_ALLOCATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
struct TestAllocator;
unsafe impl std::alloc::GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if FAIL_ALLOCATION
            .try_with(|counter| match counter.get() {
                Some(0) => {
                    counter.set(None);
                    true
                }
                Some(n) => {
                    counter.set(Some(n - 1));
                    false
                }
                None => false,
            })
            .unwrap_or(false)
        {
            return ptr::null_mut();
        }
        if CHECK_ALLOCATIONS
            .try_with(|enabled| enabled.get())
            .unwrap_or(false)
        {
            CALLBACK_ALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static TEST_ALLOCATOR: TestAllocator = TestAllocator;

pub(super) struct Hardware {
    safe_calls: usize,
    fail_safe: bool,
    health: u32,
    fail_sample: bool,
    fail_arm: bool,
    fail_apply: bool,
    arm_epoch: u32,
    feedback: bool,
    missing_port: Option<usize>,
    observation_offset: i64,
    applied: Vec<PortOutput>,
    readback: u32,
    malformed: u32,
}

impl Default for Hardware {
    fn default() -> Self {
        Self {
            safe_calls: 0,
            fail_safe: false,
            health: HEALTH_ALL,
            fail_sample: false,
            fail_arm: false,
            fail_apply: false,
            arm_epoch: 0,
            feedback: false,
            missing_port: None,
            observation_offset: 0,
            applied: Vec::new(),
            readback: 0,
            malformed: 0,
        }
    }
}

unsafe extern "C" fn safe(context: *mut c_void) -> i32 {
    let hardware = unsafe { &mut *context.cast::<Hardware>() };
    hardware.safe_calls += 1;
    if hardware.fail_safe {
        GPIO_IO_ERROR
    } else {
        for output in &mut hardware.applied {
            output.outputs = OUTPUT_MUTES;
        }
        hardware.feedback = false;
        GPIO_OK
    }
}

unsafe extern "C" fn sample(
    context: *mut c_void,
    now: u64,
    safety: *mut SafetyInput,
    ports: *mut PortInput,
    count: usize,
) -> i32 {
    let hardware = unsafe { &mut *context.cast::<Hardware>() };
    if hardware.fail_sample {
        return GPIO_IO_ERROR;
    }
    unsafe {
        *safety = SafetyInput {
            struct_size: size_of::<SafetyInput>() as u32,
            sampled_at_ns: now.saturating_add_signed(hardware.observation_offset),
            valid: 1,
            health: hardware.health,
            arm_feedback: u32::from(hardware.feedback),
            arm_epoch: hardware.arm_epoch,
        }
    };
    for (i, port) in unsafe { slice::from_raw_parts_mut(ports, count) }
        .iter_mut()
        .enumerate()
    {
        *port = PortInput {
            struct_size: size_of::<PortInput>() as u32,
            present: u32::from(hardware.missing_port != Some(i)),
            valid_inputs: INPUT_COR,
            confirmed_valid: hardware.readback,
            confirmed_outputs: hardware.applied.get(i).map_or(OUTPUT_MUTES, |p| p.outputs),
            ..Default::default()
        };
        match hardware.malformed {
            4 => port.struct_size = 0,
            5 => port.valid_inputs = 0,
            6 => port.inputs = 128,
            7 => port.valid_inputs = 128 | INPUT_COR,
            _ => (),
        }
    }
    unsafe {
        match hardware.malformed {
            1 => (*safety).struct_size = 0,
            2 => (*safety).valid = 0,
            3 => (*safety).arm_feedback = 2,
            _ => (),
        }
    }
    GPIO_OK
}

unsafe extern "C" fn arm(context: *mut c_void, epoch: u32) -> i32 {
    let hardware = unsafe { &mut *context.cast::<Hardware>() };
    hardware.arm_epoch = epoch;
    if hardware.fail_arm {
        GPIO_IO_ERROR
    } else {
        GPIO_OK
    }
}
unsafe extern "C" fn apply(context: *mut c_void, outputs: *const PortOutput, count: usize) -> i32 {
    let hardware = unsafe { &mut *context.cast::<Hardware>() };
    if hardware.fail_apply {
        return GPIO_IO_ERROR;
    }
    hardware.applied = unsafe { slice::from_raw_parts(outputs, count) }.to_vec();
    GPIO_OK
}

fn transport(hardware: &mut Hardware) -> Transport {
    Transport {
        struct_size: size_of::<Transport>() as u32,
        context: ptr::from_mut(hardware).cast(),
        sample: Some(sample),
        request_arm: Some(arm),
        apply_outputs: Some(apply),
        safe_outputs: Some(safe),
    }
}

fn ports(count: usize, appliance: bool) -> Vec<PortConfig> {
    (0..count)
        .map(|i| PortConfig {
            struct_size: size_of::<PortConfig>() as u32,
            port_id: 1000 + i as u64,
            capabilities: PortCapabilities {
                struct_size: size_of::<PortCapabilities>() as u32,
                inputs: INPUT_COR,
                outputs: OUTPUT_PTT | OUTPUT_CTCSS | OUTPUT_MUTES | OUTPUT_SITE,
                safety: if appliance { SAFETY_ALL } else { 0 },
                ..Default::default()
            },
            required_inputs: INPUT_COR,
            required_outputs: OUTPUT_PTT | OUTPUT_CTCSS,
        })
        .collect()
}

fn config(ports: &[PortConfig], appliance: bool) -> ContextConfig {
    ContextConfig {
        struct_size: size_of::<ContextConfig>() as u32,
        safety_contract: u32::from(appliance),
        ports: ports.as_ptr(),
        port_count: ports.len(),
        arm_timeout_ns: 100,
        maximum_snapshot_age_ns: 50,
    }
}

pub(super) struct Runtime(pub(super) *mut Context);
impl Runtime {
    pub(super) fn new(hardware: &mut Hardware, count: usize, appliance: bool) -> Self {
        let configs = ports(count, appliance);
        let mut handle = ptr::null_mut();
        assert_eq!(
            unsafe {
                context_create(
                    &config(&configs, appliance),
                    &transport(hardware),
                    &mut handle,
                )
            },
            GPIO_OK
        );
        Self(handle)
    }
    pub(super) fn snapshot(&self) -> SafetySnapshot {
        let mut out = SafetySnapshot {
            struct_size: size_of::<SafetySnapshot>() as u32,
            ..Default::default()
        };
        assert_eq!(unsafe { context_snapshot(self.0, &mut out) }, GPIO_OK);
        out
    }
    pub(super) fn port(&self, i: usize) -> *const Port {
        let mut port = ptr::null();
        assert_eq!(
            unsafe { context_get_port(self.0, 1000 + i as u64, &mut port) },
            GPIO_OK
        );
        port
    }
    fn ready(&self, count: usize) {
        for i in 0..count {
            assert_eq!(
                unsafe { port_ready(self.port(i), self.snapshot().epoch) },
                GPIO_OK
            );
        }
    }
    pub(super) fn arm(&self, hardware: &mut Hardware, count: usize, now: u64) -> u32 {
        assert_eq!(unsafe { context_initialize(self.0, now) }, GPIO_OK);
        self.ready(count);
        let epoch = self.snapshot().epoch;
        assert_eq!(unsafe { context_request_arm(self.0, epoch, now) }, GPIO_OK);
        assert_eq!(self.snapshot().state, ARMING);
        hardware.feedback = true;
        assert_eq!(unsafe { context_service(self.0, now + 1) }, GPIO_OK);
        assert_eq!(self.snapshot().state, ARMED);
        assert_ne!(self.snapshot().epoch, epoch);
        self.snapshot().epoch
    }
    fn publish(&self, i: usize, epoch: u32, outputs: u32) -> i32 {
        unsafe {
            port_publish(
                self.port(i),
                &OutputIntent {
                    struct_size: size_of::<OutputIntent>() as u32,
                    epoch,
                    outputs,
                },
            )
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe { context_destroy(self.0) };
    }
}

#[test]
fn context_preallocates_configured_ports() {
    let mut hardware = Hardware::default();
    let ports = ports(7, true);
    let mut context = ptr::null_mut();
    assert_eq!(
        unsafe {
            context_create(
                &config(&ports, true),
                &transport(&mut hardware),
                &mut context,
            )
        },
        GPIO_OK
    );
    let runtime = Runtime(context);
    assert_eq!(hardware.safe_calls, 1);
    for expected in &ports {
        let mut port = ptr::null();
        assert_eq!(
            unsafe { context_get_port(runtime.0, expected.port_id, &mut port) },
            GPIO_OK
        );
        let mut status = PortSnapshot {
            struct_size: size_of::<PortSnapshot>() as u32,
            ..Default::default()
        };
        assert_eq!(unsafe { port_snapshot(port, &mut status) }, GPIO_OK);
        assert_eq!(status.port_id, expected.port_id);
        assert_eq!(status.confirmed_valid, 0);
        assert_eq!(status.ready, 0);
    }
    let mut snapshot = SafetySnapshot {
        struct_size: size_of::<SafetySnapshot>() as u32,
        ..Default::default()
    };
    assert_eq!(
        unsafe { context_snapshot(runtime.0, &mut snapshot) },
        GPIO_OK
    );
    assert_eq!(snapshot.state, DISARMED);
    drop(runtime);
    assert_eq!(hardware.safe_calls, 2);
}

#[test]
fn context_capabilities_are_hardware_specific() {
    let mut hardware = Hardware::default();
    let ordinary = ports(1, false);
    let mut context = ptr::null_mut();
    let mut io = transport(&mut hardware);
    io.request_arm = None;
    assert_eq!(
        unsafe { context_create(&config(&ordinary, false), &io, &mut context) },
        GPIO_OK
    );
    drop(Runtime(context));
    assert_eq!(
        unsafe { context_create(&config(&ordinary, true), &io, &mut context) },
        GPIO_UNSUPPORTED
    );
    assert!(context.is_null());
    let mut missing = ordinary.clone();
    missing[0].capabilities.outputs = OUTPUT_PTT;
    assert_eq!(
        unsafe { context_create(&config(&missing, false), &io, &mut context) },
        GPIO_UNSUPPORTED
    );
    let mut duplicate = ports(7, true);
    duplicate[6].port_id = duplicate[0].port_id;
    assert_eq!(
        unsafe {
            context_create(
                &config(&duplicate, true),
                &transport(&mut hardware),
                &mut context,
            )
        },
        GPIO_INVALID_ARGUMENT
    );
    hardware.fail_safe = true;
    assert_eq!(
        unsafe {
            context_create(
                &config(&ordinary, false),
                &transport(&mut hardware),
                &mut context,
            )
        },
        GPIO_IO_ERROR
    );
    assert!(context.is_null());
}

#[test]
fn context_setup_errors_never_publish_a_handle() {
    let mut hardware = Hardware::default();
    let ports = ports(1, true);
    let mut c = config(&ports, true);
    let io = transport(&mut hardware);
    let mut context = ptr::null_mut();
    for count in [0, usize::MAX] {
        c.port_count = count;
        assert_ne!(unsafe { context_create(&c, &io, &mut context) }, GPIO_OK);
        assert!(context.is_null());
    }
    c = config(&ports, true);
    c.struct_size = 0;
    assert_eq!(
        unsafe { context_create(&c, &io, &mut context) },
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe { context_create(ptr::null(), &io, &mut context) },
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(hardware.safe_calls, 0);
}

#[test]
fn context_arm_and_fault_matrix() {
    for health_bit in 0..5 {
        let mut hardware = Hardware::default();
        let runtime = Runtime::new(&mut hardware, 7, true);
        assert_ne!(
            unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 10) },
            GPIO_OK
        );
        let epoch = runtime.arm(&mut hardware, 7, 100);
        for port in 0..7 {
            assert_eq!(
                runtime.publish(port, epoch, OUTPUT_PTT | OUTPUT_CTCSS | OUTPUT_SITE),
                GPIO_OK
            );
        }
        assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
        assert!(
            hardware
                .applied
                .iter()
                .all(|p| p.outputs == OUTPUT_PTT | OUTPUT_CTCSS | OUTPUT_SITE)
        );
        hardware.health &= !(1 << health_bit);
        assert_ne!(unsafe { context_service(runtime.0, 103) }, GPIO_OK);
        assert_eq!(runtime.snapshot().state, FAULTED);
        assert!(hardware.applied.iter().all(|p| p.outputs == OUTPUT_MUTES));
        hardware.health = HEALTH_ALL;
        assert_ne!(unsafe { context_service(runtime.0, 104) }, GPIO_OK);
        assert_eq!(runtime.snapshot().state, FAULTED);
        assert_eq!(unsafe { context_recover(runtime.0, 105) }, GPIO_OK);
        runtime.ready(7);
        let request_epoch = runtime.snapshot().epoch;
        assert_eq!(
            unsafe { context_request_arm(runtime.0, request_epoch, 106) },
            GPIO_OK
        );
        assert_eq!(runtime.snapshot().state, ARMING);
        hardware.health &= !(1 << health_bit);
        assert_ne!(unsafe { context_service(runtime.0, 107) }, GPIO_OK);
        assert_eq!(runtime.snapshot().state, FAULTED);
        assert_ne!(runtime.publish(0, request_epoch, OUTPUT_PTT), GPIO_OK);
        assert!(hardware.applied.iter().all(|p| p.outputs == OUTPUT_MUTES));
    }
}

#[test]
fn context_recovery_never_replays_intent() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 1, true);
    let epoch = runtime.arm(&mut hardware, 1, 100);
    assert_eq!(
        runtime.publish(0, epoch, OUTPUT_PTT | OUTPUT_CTCSS),
        GPIO_OK
    );
    assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
    assert_eq!(unsafe { context_disarm(runtime.0) }, GPIO_OK);
    assert_ne!(runtime.publish(0, epoch, OUTPUT_PTT), GPIO_OK);
    assert_ne!(unsafe { port_ready(runtime.port(0), epoch) }, GPIO_OK);
    assert_eq!(unsafe { context_recover(runtime.0, 103) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, DISARMED);
    runtime.ready(1);
    let request_epoch = runtime.snapshot().epoch;
    assert_ne!(runtime.publish(0, request_epoch, OUTPUT_PTT), GPIO_OK);
    assert_eq!(
        unsafe { context_request_arm(runtime.0, request_epoch, 104) },
        GPIO_OK
    );
    hardware.feedback = true;
    hardware.arm_epoch = epoch;
    assert_eq!(unsafe { context_service(runtime.0, 105) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, ARMING);
    hardware.arm_epoch = request_epoch;
    assert_eq!(unsafe { context_service(runtime.0, 106) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, ARMED);
    assert!(hardware.applied.iter().all(|p| p.outputs == OUTPUT_MUTES));
    assert_ne!(runtime.publish(0, request_epoch, OUTPUT_PTT), GPIO_OK);
    assert_eq!(
        runtime.publish(0, runtime.snapshot().epoch, OUTPUT_PTT),
        GPIO_OK
    );
    assert_eq!(unsafe { context_service(runtime.0, 107) }, GPIO_OK);
    assert_eq!(hardware.applied[0].outputs, OUTPUT_PTT);
}

#[test]
fn context_invalid_health_readiness_and_time_fail_closed() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 7, true);
    assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_OK);
    runtime.ready(6);
    assert_ne!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 101) },
        GPIO_OK
    );
    runtime.ready(7);
    hardware.fail_arm = true;
    assert_ne!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 102) },
        GPIO_OK
    );
    assert_eq!(runtime.snapshot().state, FAULTED);
    for offset in [-51, 1] {
        hardware.fail_arm = false;
        hardware.observation_offset = offset;
        assert_ne!(unsafe { context_recover(runtime.0, 200) }, GPIO_OK);
        assert_eq!(runtime.snapshot().state, FAULTED);
    }
    hardware.observation_offset = 0;
    assert_eq!(unsafe { context_recover(runtime.0, 300) }, GPIO_OK);
    runtime.ready(7);
    assert_eq!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 300) },
        GPIO_OK
    );
    assert_ne!(unsafe { context_service(runtime.0, 400) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, FAULTED);
    assert_eq!(unsafe { context_recover(runtime.0, 500) }, GPIO_OK);
    assert_ne!(unsafe { context_service(runtime.0, 499) }, GPIO_OK);
}

#[test]
fn context_ordinary_is_independent_and_does_not_fabricate_readback() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 1, false);
    assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_OK);
    runtime.ready(1);
    assert_eq!(unsafe { context_service(runtime.0, 101) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, ARMED);
    assert_eq!(hardware.arm_epoch, 0);
    assert_eq!(runtime.snapshot().arm_feedback, 0);
    assert_eq!(
        runtime.publish(0, runtime.snapshot().epoch, OUTPUT_PTT),
        GPIO_OK
    );
    assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
    let mut status = PortSnapshot {
        struct_size: size_of::<PortSnapshot>() as u32,
        ..Default::default()
    };
    assert_eq!(
        unsafe { port_snapshot(runtime.port(0), &mut status) },
        GPIO_OK
    );
    assert_eq!(status.requested_outputs, OUTPUT_PTT);
    assert_eq!(status.confirmed_valid, 0);
    let mut other_hardware = Hardware::default();
    let other = Runtime::new(&mut other_hardware, 7, true);
    other.arm(&mut other_hardware, 7, 100);
    other_hardware.fail_sample = true;
    other_hardware.fail_safe = true;
    assert_ne!(unsafe { context_service(other.0, 102) }, GPIO_OK);
    assert_eq!(other.snapshot().safe_submission_failed, 1);
    assert_eq!(runtime.snapshot().state, ARMED);
    assert_eq!(hardware.applied[0].outputs, OUTPUT_PTT);
}

#[test]
fn context_safety_fault_releases_all_covered_outputs() {
    for failure in 0..3 {
        let mut hardware = Hardware::default();
        let runtime = Runtime::new(&mut hardware, 7, true);
        let epoch = runtime.arm(&mut hardware, 7, 100);
        for i in 0..7 {
            assert_eq!(
                runtime.publish(i, epoch, OUTPUT_PTT | OUTPUT_CTCSS | OUTPUT_SITE),
                GPIO_OK
            );
        }
        assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
        match failure {
            0 => hardware.missing_port = Some(6),
            1 => hardware.fail_sample = true,
            _ => hardware.fail_apply = true,
        }
        assert_ne!(unsafe { context_service(runtime.0, 103) }, GPIO_OK);
        assert_eq!(runtime.snapshot().state, FAULTED);
        let mut snapshot = PortSnapshot {
            struct_size: size_of::<PortSnapshot>() as u32,
            ..Default::default()
        };
        assert_eq!(
            unsafe { port_snapshot(runtime.port(0), &mut snapshot) },
            GPIO_OK
        );
        assert_eq!(snapshot.valid_inputs, 0);
        assert_eq!(snapshot.confirmed_valid, 0);
        assert_eq!(snapshot.present, 0);
        assert!(hardware.applied.iter().all(|p| p.outputs == OUTPUT_MUTES));
    }
}

#[test]
fn context_stalled_service_never_blocks_audio_or_claims_physical_protection() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 7, true);
    let epoch = runtime.arm(&mut hardware, 7, 100);
    for i in 0..7 {
        assert_eq!(
            runtime.publish(i, epoch, OUTPUT_PTT | OUTPUT_CTCSS),
            GPIO_OK
        );
    }
    assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
    // The simulated independent physical watchdog acts without software service.
    hardware.health &= !8;
    for output in &mut hardware.applied {
        output.outputs = OUTPUT_MUTES;
    }
    let calls = hardware.safe_calls;
    let mut status = PortSnapshot {
        struct_size: size_of::<PortSnapshot>() as u32,
        ..Default::default()
    };
    let allocations = CALLBACK_ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed);
    CHECK_ALLOCATIONS.with(|flag| flag.set(true));
    for _ in 0..1000 {
        assert_eq!(runtime.publish(0, epoch, OUTPUT_PTT), GPIO_OK);
        assert_eq!(
            unsafe { port_snapshot(runtime.port(0), &mut status) },
            GPIO_OK
        );
        assert_eq!(runtime.snapshot().state, ARMED);
    }
    CHECK_ALLOCATIONS.with(|flag| flag.set(false));
    assert_eq!(
        CALLBACK_ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed),
        allocations
    );
    assert_eq!(hardware.safe_calls, calls);
    assert!(hardware.applied.iter().all(|p| p.outputs == OUTPUT_MUTES));
    assert_ne!(unsafe { context_service(runtime.0, 103) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, FAULTED);
}

#[test]
fn context_shutdown_quiesces_and_retirement_invalidates_readiness() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 7, true);
    let epoch = runtime.arm(&mut hardware, 7, 100);
    assert_eq!(runtime.publish(0, epoch, OUTPUT_PTT), GPIO_OK);
    assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
    assert_eq!(unsafe { port_retire(runtime.0, 1000, epoch) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, DISARMED);
    assert_ne!(
        unsafe { port_ready(runtime.port(0), runtime.snapshot().epoch) },
        GPIO_OK
    );
    assert_ne!(runtime.publish(0, epoch, OUTPUT_PTT), GPIO_OK);
    let before = hardware.safe_calls;
    drop(runtime);
    assert_eq!(hardware.safe_calls, before + 1);
    assert!(hardware.applied.iter().all(|p| p.outputs == OUTPUT_MUTES));
}

#[test]
fn context_failed_disarm_is_a_latched_fault() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 1, true);
    runtime.arm(&mut hardware, 1, 100);
    hardware.fail_safe = true;
    assert_eq!(unsafe { context_disarm(runtime.0) }, GPIO_IO_ERROR);
    assert_eq!(runtime.snapshot().state, FAULTED);
    assert_eq!(runtime.snapshot().safe_submission_failed, 1);
}

#[test]
fn context_all_ffi_boundaries_reject_null_and_short_records() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 1, true);
    let configs = ports(1, true);
    let cfg = config(&configs, true);
    let io = transport(&mut hardware);
    let mut result = ptr::null_mut();
    let mut endpoint = ptr::null();
    let mut short = 0_u32;
    unsafe {
        assert_eq!(
            context_create(&cfg, &io, ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        for bad in [ptr::null(), ptr::from_ref(&short)] {
            assert_eq!(
                context_create(&cfg, bad.cast(), &mut result),
                GPIO_INVALID_ARGUMENT
            );
            assert_eq!(
                port_publish(runtime.port(0), bad.cast()),
                GPIO_INVALID_ARGUMENT
            );
        }
        for bad in [ptr::null_mut(), ptr::from_mut(&mut short)] {
            assert_eq!(
                context_snapshot(runtime.0, bad.cast()),
                GPIO_INVALID_ARGUMENT
            );
            assert_eq!(
                port_snapshot(runtime.port(0), bad.cast()),
                GPIO_INVALID_ARGUMENT
            );
        }
        assert_eq!(
            context_get_port(runtime.0, 1000, ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            context_get_port(ptr::null(), 1000, &mut endpoint),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            context_get_port(runtime.0, 999, &mut endpoint),
            GPIO_UNSUPPORTED
        );
        assert_eq!(
            context_snapshot(ptr::null(), ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            port_snapshot(ptr::null(), ptr::null_mut()),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            context_initialize(ptr::null_mut(), 1),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(
            context_request_arm(ptr::null_mut(), 0, 1),
            GPIO_INVALID_ARGUMENT
        );
        assert_eq!(context_disarm(ptr::null_mut()), GPIO_INVALID_ARGUMENT);
        assert_eq!(context_recover(ptr::null_mut(), 1), GPIO_INVALID_ARGUMENT);
        assert_eq!(context_service(ptr::null_mut(), 1), GPIO_INVALID_ARGUMENT);
        assert_eq!(port_retire(ptr::null_mut(), 1000, 0), GPIO_INVALID_ARGUMENT);
        assert_eq!(port_ready(ptr::null(), 0), GPIO_INVALID_ARGUMENT);
        assert_eq!(
            port_publish(ptr::null(), ptr::null()),
            GPIO_INVALID_ARGUMENT
        );
        context_destroy(ptr::null_mut());
    }
}

#[test]
fn context_setup_invalid_fields_and_allocation_failures() {
    let mut hardware = Hardware::default();
    let mut result = ptr::null_mut();
    for field in 0..18 {
        let mut ports = ports(1, true);
        let mut c = config(&ports, true);
        let mut io = transport(&mut hardware);
        match field {
            0 => c.safety_contract = 2,
            1 => c.ports = ptr::null(),
            2 => c.maximum_snapshot_age_ns = 0,
            3 => io.sample = None,
            4 => io.apply_outputs = None,
            5 => io.safe_outputs = None,
            6 => c.arm_timeout_ns = 0,
            7 => ports[0].struct_size = 0,
            8 => ports[0].capabilities.struct_size = 0,
            9 => ports[0].capabilities.inputs = 128,
            10 => ports[0].capabilities.outputs = 128,
            11 => ports[0].capabilities.readback = 128,
            12 => ports[0].capabilities.safety = 128,
            13 => {
                ports[0].capabilities.input_pin_mask = 1;
                ports[0].capabilities.output_pin_mask = 1;
            }
            14 => ports[0].required_inputs = INPUT_CTCSS,
            15 => ports[0].capabilities.safety = 0,
            // The first larger port allocation succeeds; fail each remaining fallible allocation.
            16 => FAIL_ALLOCATION.with(|counter| counter.set(Some(2))),
            _ => FAIL_ALLOCATION.with(|counter| counter.set(Some(3))),
        }
        let code = unsafe { context_create(&c, &io, &mut result) };
        FAIL_ALLOCATION.with(|counter| counter.set(None));
        assert_ne!(code, GPIO_OK, "field {field}");
        assert!(result.is_null());
    }
}

#[test]
fn context_every_setup_allocation_can_fail_without_aborting() {
    let mut hardware = Hardware::default();
    let ports = ports(1, true);
    let cfg = config(&ports, true);
    let io = transport(&mut hardware);
    for allocation in 0..5 {
        let mut result = ptr::null_mut();
        FAIL_ALLOCATION.with(|counter| counter.set(Some(allocation)));
        let code = unsafe { context_create(&cfg, &io, &mut result) };
        FAIL_ALLOCATION.with(|counter| counter.set(None));
        assert_eq!(code, GPIO_IO_ERROR, "allocation {allocation}");
        assert!(result.is_null());
    }
}

#[test]
fn context_malformed_samples_and_control_state_edges() {
    for malformed in 1..=8 {
        let mut hardware = Hardware::default();
        let runtime = Runtime::new(&mut hardware, 1, true);
        runtime.arm(&mut hardware, 1, 100);
        if malformed == 8 {
            hardware.readback = OUTPUT_PTT;
        } else {
            hardware.malformed = malformed;
        }
        assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_IO_ERROR);
        assert_eq!(runtime.snapshot().state, FAULTED);
        assert_ne!(unsafe { context_initialize(runtime.0, 103) }, GPIO_OK);
    }
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 2, true);
    assert_ne!(unsafe { context_service(runtime.0, 99) }, GPIO_OK);
    assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_OK);
    runtime.ready(2);
    let epoch = runtime.snapshot().epoch;
    assert_ne!(
        unsafe { context_request_arm(runtime.0, epoch - 1, 100) },
        GPIO_OK
    );
    assert_ne!(unsafe { port_ready(runtime.port(0), epoch - 1) }, GPIO_OK);
    assert_ne!(unsafe { port_retire(runtime.0, 999, epoch) }, GPIO_OK);
    assert_ne!(unsafe { port_retire(runtime.0, 1000, epoch - 1) }, GPIO_OK);
    hardware.health = 0;
    assert_eq!(
        unsafe { context_request_arm(runtime.0, epoch, 100) },
        GPIO_IO_ERROR
    );
    hardware.health = HEALTH_ALL;
    assert_eq!(unsafe { context_recover(runtime.0, 200) }, GPIO_OK);
    runtime.ready(2);
    assert_eq!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 200) },
        GPIO_OK
    );
    hardware.feedback = true;
    assert_eq!(unsafe { context_service(runtime.0, 200) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, ARMING); // Same-time feedback is not fresh.
    assert_eq!(unsafe { context_service(runtime.0, 201) }, GPIO_OK);
    assert_eq!(
        runtime.publish(0, runtime.snapshot().epoch, 128),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        runtime.publish(0, runtime.snapshot().epoch, OUTPUT_CTCSS),
        GPIO_INVALID_ARGUMENT
    );
    hardware.feedback = false;
    assert_eq!(unsafe { context_service(runtime.0, 202) }, GPIO_IO_ERROR);
    assert_eq!(unsafe { context_recover(runtime.0, u64::MAX - 1) }, GPIO_OK);
    runtime.ready(2);
    assert_eq!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, u64::MAX - 1) },
        GPIO_IO_ERROR
    );
}

#[test]
fn context_ordinary_readiness_retired_ports_and_real_readback() {
    let mut hardware = Hardware::default();
    let mut ports = ports(2, false);
    ports[1].capabilities.readback = OUTPUT_PTT;
    let mut handle = ptr::null_mut();
    assert_eq!(
        unsafe {
            context_create(
                &config(&ports, false),
                &transport(&mut hardware),
                &mut handle,
            )
        },
        GPIO_OK
    );
    let runtime = Runtime(handle);
    assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_OK);
    assert_eq!(unsafe { context_service(runtime.0, 100) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, DISARMED);
    assert_eq!(
        unsafe { port_retire(runtime.0, 1000, runtime.snapshot().epoch) },
        GPIO_OK
    );
    assert_eq!(unsafe { context_initialize(runtime.0, 101) }, GPIO_OK);
    assert_eq!(
        unsafe { port_ready(runtime.port(0), runtime.snapshot().epoch) },
        GPIO_UNSUPPORTED
    );
    assert_eq!(
        unsafe { port_ready(runtime.port(1), runtime.snapshot().epoch) },
        GPIO_OK
    );
    assert_eq!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 101) },
        GPIO_UNSUPPORTED
    );
    assert_eq!(unsafe { context_service(runtime.0, 102) }, GPIO_OK);
    assert_eq!(
        runtime.publish(0, runtime.snapshot().epoch, OUTPUT_PTT),
        GPIO_UNSUPPORTED
    );
    assert_eq!(
        runtime.publish(1, runtime.snapshot().epoch, OUTPUT_PTT),
        GPIO_OK
    );
    assert_eq!(unsafe { context_service(runtime.0, 103) }, GPIO_OK);
    hardware.readback = OUTPUT_PTT;
    assert_eq!(unsafe { context_service(runtime.0, 104) }, GPIO_OK);
    let mut snapshot = PortSnapshot {
        struct_size: size_of::<PortSnapshot>() as u32,
        ..Default::default()
    };
    assert_eq!(
        unsafe { port_snapshot(runtime.port(1), &mut snapshot) },
        GPIO_OK
    );
    assert_eq!(snapshot.confirmed_valid, OUTPUT_PTT);
    assert_eq!(snapshot.confirmed_outputs, OUTPUT_PTT);
}

#[test]
fn context_arm_wait_and_changed_feedback_are_not_admitted() {
    let mut hardware = Hardware::default();
    let runtime = Runtime::new(&mut hardware, 1, true);
    hardware.fail_safe = true;
    assert_eq!(unsafe { context_initialize(runtime.0, 100) }, GPIO_IO_ERROR);
    hardware.fail_safe = false;
    assert_eq!(unsafe { context_recover(runtime.0, 101) }, GPIO_OK);
    assert_eq!(unsafe { context_service(runtime.0, 101) }, GPIO_OK);
    runtime.ready(1);
    assert_eq!(
        unsafe { context_request_arm(runtime.0, runtime.snapshot().epoch, 102) },
        GPIO_OK
    );
    assert_eq!(unsafe { context_service(runtime.0, 103) }, GPIO_OK);
    assert_eq!(runtime.snapshot().state, ARMING);
    assert_eq!(unsafe { context_recover(runtime.0, 103) }, GPIO_UNSUPPORTED);
    hardware.feedback = true;
    assert_eq!(unsafe { context_service(runtime.0, 104) }, GPIO_OK);
    hardware.arm_epoch += 1;
    assert_eq!(unsafe { context_service(runtime.0, 105) }, GPIO_IO_ERROR);
}
