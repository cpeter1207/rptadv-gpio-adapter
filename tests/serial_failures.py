"""Exercise tty ABI failures and uncertain ownership on a real pseudo-terminal."""
import ctypes as c
import os
import pty
import sys
import termios

word, handle = c.c_uint32, c.c_void_p
class Descriptor(c.Structure):
    _fields_ = [("size", word), ("version", word), ("name", c.c_char_p)] + [
        (name, handle) for name in ("open", "service", "close", "service_outputs", "open_retiring", "quarantine")]
class Config(c.Structure):
    _fields_ = [("size", word), ("version", word), ("path", c.c_char_p)] + [
        (name, word) for name in ("baud", "data_bits", "parity", "stop_half_bits", "outputs", "idle")]
lib = c.CDLL(sys.argv[1])
lib.rptadv_gpio_serial_descriptor.restype = c.POINTER(Descriptor)
api = lib.rptadv_gpio_serial_descriptor().contents
open_type = c.CFUNCTYPE(c.c_int, c.POINTER(Config), c.POINTER(handle))
opening, retiring = open_type(api.open), open_type(api.open_retiring)
service_type = c.CFUNCTYPE(c.c_int, handle, word, c.POINTER(word), c.POINTER(word))
service, levels = service_type(api.service), service_type(api.service_outputs)
close = c.CFUNCTYPE(c.c_int, handle)(api.close)
quarantine = c.CFUNCTYPE(None, handle)(api.quarantine)
fixture = c.CDLL(None)
fixture.rpt_test_ioctl_fault.argtypes = [c.c_ulong, c.c_int, c.c_int]
fixture.rpt_test_modem_fail_persistent.argtypes = [c.c_ulong]
master, slave = pty.openpty()
try:
    path = os.fsencode(os.ttyname(slave))
    os.environ["RPT_TEST_MODEM_TTY"] = os.fsdecode(path)
    cfg = Config(c.sizeof(Config), 1, path, 9600, 8, 0, 2, 3, 0)
    owner = handle(123)
    odd_config = c.cast(c.byref(cfg, 1), c.POINTER(Config))
    odd_handle = c.cast(c.byref(cfg, 1), c.POINTER(handle))
    odd_word = c.cast(c.byref(cfg, 1), c.POINTER(word))
    def open_owner():
        return opening(c.byref(cfg), c.byref(owner))
    for pointer in [None, odd_handle]:
        assert opening(c.byref(cfg), pointer) == -1
    for pointer in [None, odd_config]:
        assert opening(pointer, c.byref(owner)) == -1 and not owner.value
    for value in [None, b""]:
        cfg.path = value
        assert open_owner() == -1 and not owner.value
    for value in [b"/no-such-rpt-serial", b"/tmp", b"/dev/null"]:
        cfg.path = value
        assert open_owner() == -5 and not owner.value
    cfg.path = path
    # The real PTY may reject supported UART framing. Either result must
    # release its exclusive ownership so the next attempt can proceed.
    for field, values in [("data_bits", [5, 6, 7, 8]), ("parity", [0, 1, 2, 3, 4]), ("stop_half_bits", [2, 4])]:
        old = getattr(cfg, field)
        for value in values:
            setattr(cfg, field, value)
            result = open_owner()
            assert (result == 0 and close(owner) == 0) or (result == -5 and not owner.value)
        setattr(cfg, field, old)
    cfg.data_bits, cfg.stop_half_bits = 5, 3
    result = open_owner()
    assert (result == 0 and close(owner) == 0) or (result == -5 and not owner.value)
    cfg.data_bits, cfg.stop_half_bits = 8, 2
    for request, skip in [(termios.TIOCMGET, 0), (termios.TIOCMSET, 0), (0x802C542A, 0), (0x802C542A, 1), (0x402C542B, 0), (termios.TIOCEXCL, 0)]:
        fixture.rpt_test_modem_set(termios.TIOCM_RTS)
        fixture.rpt_test_ioctl_fault(request, skip, 1)
        assert open_owner() == -5 and not owner.value, (request, skip)
        fixture.rpt_test_ioctl_fault(0, 0, 0)
        assert open_owner() == 0 and close(owner) == 0
    fixture.rpt_test_fail_flock()
    assert open_owner() == -5 and not owner.value
    assert open_owner() == 0 and close(owner) == 0
    for field in [1, 2, 3, 4]:
        fixture.rpt_test_termios_mismatch(field)
        assert open_owner() == -5 and not owner.value, field
    fixture.rpt_test_modem_set(termios.TIOCM_RTS)
    fixture.rpt_test_ignore_modem_set(1)
    assert open_owner() == -5 and not owner.value
    assert open_owner() == 0
    inputs, outputs = word(99), word(99)
    for bad in [None, handle(1)]:
        assert service(bad, 0, c.byref(inputs), c.byref(outputs)) == -1
        assert levels(bad, 0, c.byref(inputs), c.byref(outputs)) == -1
        assert close(bad) == -1
        quarantine(bad)
    assert service(owner, 2, c.byref(inputs), c.byref(outputs)) == -1
    for bad in [None, odd_word]:
        assert levels(owner, 0, bad, c.byref(outputs)) == -1
        assert levels(owner, 0, c.byref(inputs), bad) == -1
    assert levels(owner, 0, c.byref(inputs), c.byref(inputs)) == -1
    assert levels(owner, 4, c.byref(inputs), c.byref(outputs)) == -1
    assert service(owner, 0, c.byref(inputs), c.byref(outputs)) == 0
    quarantine(owner)
    fixture.rpt_test_modem_fail_persistent(termios.TIOCMSET)
    assert open_owner() == -5 and not owner.value
    fixture.rpt_test_modem_fail_persistent(0)
    assert open_owner() == 0 and close(owner) == 0
    fixture.rpt_test_modem_fail_after_setup()
    assert retiring(c.byref(cfg), c.byref(owner)) == -5 and owner.value
    assert levels(owner, 0, c.byref(inputs), c.byref(outputs)) == -5
    assert inputs.value == outputs.value == 0 and close(owner) == -5
    quarantine(owner)
    fixture.rpt_test_modem_fail_persistent(0)
    assert open_owner() == 0 and close(owner) == 0
    print("TTY framing, setup/readback failures, ABI validation and failed-owner retirement passed.")
finally:
    os.close(slave)
    os.close(master)
