"""Check the exported held-key ABI validation and native ioctl failure paths."""
import ctypes as c
import os
import pty
import sys

word, handle = c.c_uint32, c.c_void_p
class Descriptor(c.Structure):
    _fields_ = [("size", word), ("version", word), ("name", c.c_char_p)] + [
        (name, handle) for name in ("open", "sample", "close", "sample_valid")]
lib = c.CDLL(sys.argv[1])
lib.rptadv_gpio_keyboard_descriptor.restype = c.POINTER(Descriptor)
api = lib.rptadv_gpio_keyboard_descriptor().contents
opening = c.CFUNCTYPE(c.c_int, c.c_char_p, c.POINTER(word), word, c.POINTER(handle))(api.open)
sample = c.CFUNCTYPE(c.c_int, handle, c.POINTER(word))(api.sample)
observe = c.CFUNCTYPE(c.c_int, handle, c.POINTER(word), c.POINTER(word))(api.sample_valid)
close = c.CFUNCTYPE(None, handle)(api.close)
fixture = c.CDLL(None)
master, slave = pty.openpty()
try:
    path = os.fsencode(os.ttyname(slave))
    os.environ["RPT_TEST_KEYBOARD"] = os.fsdecode(path)
    keys = (word * 2)(42, 300)
    fixture.rpt_test_evdev_key(42, 1, 1)
    fixture.rpt_test_evdev_key(300, 1, 0)
    owner = handle(123)
    odd_word = c.cast(c.byref(keys, 1), c.POINTER(word))
    odd_handle = c.cast(c.byref(keys, 1), c.POINTER(handle))
    for name, value in [("null", None), ("unaligned", odd_handle)]:
        assert opening(path, keys, 1, value) == -1, name
    for selected_path, codes, count in [
        (None, keys, 1), (path, None, 1), (path, odd_word, 1),
        (path, keys, 0), (path, keys, 3), (b"", keys, 1),
    ]:
        assert opening(selected_path, codes, count, c.byref(owner)) == -1 and not owner.value
    assert opening(b"/no-such-rpt-keyboard", keys, 1, c.byref(owner)) == -5
    for result in [-1, 97]:
        fixture.rpt_test_evdev_result(result)
        assert opening(path, keys, 1, c.byref(owner)) == -5 and not owner.value
    fixture.rpt_test_evdev_result(-2)
    assert opening(path, keys, 2, c.byref(owner)) == 0
    down, valid = word(99), word(99)
    for pointer in [None, odd_word]:
        assert sample(owner, pointer) == -1
        assert observe(owner, pointer, c.byref(valid)) == -1
        assert observe(owner, c.byref(down), pointer) == -1
    assert observe(owner, c.byref(down), c.byref(down)) == -1
    for bad_owner in [None, handle(1)]:
        assert sample(bad_owner, c.byref(down)) == -1 and down.value == 0
        assert observe(bad_owner, c.byref(down), c.byref(valid)) == -1
        assert down.value == valid.value == 0
        close(bad_owner)
    assert sample(owner, c.byref(down)) == 0 and down.value == 1
    for result in [-1, 97]:
        fixture.rpt_test_evdev_result(result)
        assert observe(owner, c.byref(down), c.byref(valid)) == -5
        assert down.value == valid.value == 0
    fixture.rpt_test_evdev_result(-2)
    fixture.rpt_test_fail_pressed(1)
    assert observe(owner, c.byref(down), c.byref(valid)) == -5
    assert down.value == valid.value == 0
    fixture.rpt_test_fail_pressed(0)
    close(owner)
    print("Held-key pointer validation, failed open, oversized ioctl and failed sample passed.")
finally:
    os.close(slave)
    os.close(master)
