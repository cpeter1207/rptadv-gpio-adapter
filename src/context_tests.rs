use crate::{GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK, GPIO_UNSUPPORTED, context::*};
use std::{ffi::c_void, mem::size_of, ptr, slice};

#[derive(Default)]
struct Hardware {
    safe_calls: usize,
    fail_safe: bool,
}

unsafe extern "C" fn safe(context: *mut c_void) -> i32 {
    let hardware = unsafe { &mut *context.cast::<Hardware>() };
    hardware.safe_calls += 1;
    if hardware.fail_safe {
        GPIO_IO_ERROR
    } else {
        GPIO_OK
    }
}

unsafe extern "C" fn sample(
    _: *mut c_void,
    now: u64,
    safety: *mut SafetyInput,
    ports: *mut PortInput,
    count: usize,
) -> i32 {
    unsafe {
        *safety = SafetyInput {
            struct_size: size_of::<SafetyInput>() as u32,
            sampled_at_ns: now,
            valid: 1,
            health: HEALTH_ALL,
            ..Default::default()
        }
    };
    for port in unsafe { slice::from_raw_parts_mut(ports, count) } {
        *port = PortInput {
            struct_size: size_of::<PortInput>() as u32,
            present: 1,
            valid_inputs: INPUT_COR,
            ..Default::default()
        };
    }
    GPIO_OK
}

unsafe extern "C" fn arm(_: *mut c_void, _: u32) -> i32 {
    GPIO_OK
}
unsafe extern "C" fn apply(_: *mut c_void, _: *const PortOutput, _: usize) -> i32 {
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

struct Runtime(*mut Context);
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
