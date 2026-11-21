"""Use a real read-only fd and interpose only Linux evdev query ioctls at the device boundary."""
import ctypes as c
import os
import pty
import sys


class Descriptor(c.Structure):
    _fields_ = [
        ("size", c.c_uint32), ("version", c.c_uint32), ("name", c.c_char_p),
        ("open", c.c_void_p), ("sample", c.c_void_p), ("close", c.c_void_p),
    ]


provider = c.CDLL(sys.argv[1])
getter = provider.rptadv_gpio_keyboard_descriptor
getter.restype = c.POINTER(Descriptor)
api = getter().contents
assert api.size >= c.sizeof(api) and api.version == 1 and api.name == b"rptadv.evdev-held-key1"
word = c.c_uint32
handle = c.c_void_p
open_device = c.CFUNCTYPE(c.c_int, c.c_char_p, c.POINTER(word), word, c.POINTER(handle))(api.open)
sample = c.CFUNCTYPE(c.c_int, handle, c.POINTER(word))(api.sample)
close = c.CFUNCTYPE(None, handle)(api.close)
fixture = c.CDLL(None)
master, slave = pty.openpty()
try:
    path = os.ttyname(slave)
    os.environ["RPT_TEST_KEYBOARD"] = path
    fixture.rpt_test_evdev_key(42, 1, 1)
    fixture.rpt_test_evdev_key(300, 1, 0)
    for codes, valid in [([0], False), ([768], False), ([37], False), ([42, 300], True)]:
        owner = handle()
        spans = (word * len(codes))(*codes)
        status = open_device(os.fsencode(path), spans, len(codes), c.byref(owner))
        assert (status == 0) == valid and bool(owner) == valid
    down = word(99)
    assert sample(owner, c.byref(down)) == 0 and down.value == 1
    fixture.rpt_test_evdev_key(300, 1, 1)
    assert sample(owner, c.byref(down)) == 0 and down.value == 3
    fixture.rpt_test_evdev_key(42, 1, 0)
    assert sample(owner, c.byref(down)) == 0 and down.value == 2
    fixture.rpt_test_evdev_key(42, 0, 0)
    assert sample(owner, c.byref(down)) != 0 and down.value == 0, "live key map lost selected released code"
    class Extended(Descriptor):
        _fields_ = [("sample_valid", c.c_void_p)]
    assert api.size >= c.sizeof(Extended)
    extended = c.cast(getter(), c.POINTER(Extended)).contents
    observe = c.CFUNCTYPE(c.c_int, handle, c.POINTER(word), c.POINTER(word))(extended.sample_valid)
    valid = word(99)
    assert observe(owner, c.byref(down), c.byref(valid)) == 0 and (down.value, valid.value) == (2, 2)
    fixture.rpt_test_evdev_key(42, 1, 0)
    assert observe(owner, c.byref(down), c.byref(valid)) == 0 and (down.value, valid.value) == (2, 3)
    fixture.rpt_test_evdev_bytes(1)
    down.value = 99
    assert sample(owner, c.byref(down)) != 0 and down.value == 0, "truncated key 300 must remain unknown"
    fixture.rpt_test_evdev_bytes(96)
    fixture.rpt_test_evdev_offline(1)
    assert sample(owner, c.byref(down)) != 0 and down.value == 0
    close(owner)
    print("Real evdev descriptor confirmed current held/released states, high EV_KEY codes, capability rejection and disconnect invalidation.")
finally:
    os.close(slave)
    os.close(master)
