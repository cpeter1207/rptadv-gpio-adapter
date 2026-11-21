"""Exercise a published serial modem-line capability in the selected GPIO DSO."""
import ctypes as c
import fcntl
import os
import pty
import sys
import tempfile
import termios
class Descriptor(c.Structure):
    _fields_ = [('size', c.c_uint32), ('version', c.c_uint32), ('name', c.c_char_p)] + [
        (op, c.c_void_p) for op in ('open', 'service', 'close', 'service_outputs')]
lib = c.CDLL(sys.argv[1])
get = getattr(lib, 'rptadv_gpio_serial_descriptor')
get.restype = c.POINTER(Descriptor)
d = get().contents
assert d.size >= c.sizeof(d) and d.version == 1 and d.name == b'rptadv.serial-modem1'
assert all(getattr(d, op) for op in ('open','service','close'))
print("Admitted sized GPIO serial descriptor", flush=True)
class SerialConfig(c.Structure):
    _fields_ = [('size',c.c_uint32), ('version',c.c_uint32), ('path',c.c_char_p)] + [
        (op,c.c_uint32) for op in ('baud','data_bits','parity','stop_half_bits','outputs','idle')]
handle = c.c_void_p
word = c.c_uint32
opendir = c.CFUNCTYPE(c.c_int, c.POINTER(SerialConfig), c.POINTER(handle))(d.open)
service = c.CFUNCTYPE(c.c_int, handle, word, c.POINTER(word), c.POINTER(word))(d.service)
close = c.CFUNCTYPE(c.c_int, handle)(d.close)
interposer = c.CDLL(None)
interposer.rpt_test_modem_set.argtypes=[c.c_int]
interposer.rpt_test_modem_fail.argtypes=[c.c_ulong]
master, slave = pty.openpty()
try:
    path = os.ttyname(slave)
    os.environ['RPT_TEST_MODEM_TTY'] = path
    with tempfile.TemporaryDirectory() as tmp:
        alias = os.path.join(tmp, 'radio')
        os.symlink(path, alias)
        cfg = SerialConfig(c.sizeof(SerialConfig),1,os.fsencode(alias),9600,8,0,2,1,1)
        def opening():
            out = handle(123)
            status = opendir(c.byref(cfg),c.byref(out))
            return status,out
        if sys.argv[2:] == ["persistent-get"]:
            for idle in [0, 1]:
                cfg.idle = idle
                status, current = opening()
                assert status == 0 and current.value
                result, feedback = word(99), word(99)
                assert service(current, 1, c.byref(result), c.byref(feedback)) == 0
                interposer.rpt_test_modem_fail_persistent(termios.TIOCMGET)
                assert service(current, 1, c.byref(result), c.byref(feedback)) != 0
                assert (result.value, feedback.value) == (0, 0)
                assert bool(interposer.rpt_test_modem_get() & termios.TIOCM_RTS) == bool(idle), "idle write must survive persistent GET failure"
                assert close(current) != 0
                assert opening()[0] != 0
                interposer.rpt_test_modem_fail_persistent(0)
                assert close(current) == 0
            print("Both RTS polarities deasserted under persistent GET failure; retirement required confirmed readback.")
            sys.exit(0)
        if sys.argv[2:] == ["failed-startup"]:
            cfg.idle = 0
            interposer.rpt_test_modem_fail_after_setup()
            status, failed = opening()
            assert status != 0 and not failed.value
            assert interposer.rpt_test_modem_get() & termios.TIOCM_RTS
            try:
                stolen = os.open(alias, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
            except OSError as error:
                import errno
                assert error.errno == errno.EBUSY
            else:
                os.close(stolen)
                raise AssertionError("failed post-termios idle released exclusive tty")
            interposer.rpt_test_modem_fail_persistent(0)
            status, replacement = opening()
            assert status == 0 and replacement.value and not interposer.rpt_test_modem_get() & termios.TIOCM_RTS
            assert close(replacement) == 0
            print("Failed startup retained physical owner until confirmed retirement before successor acquisition.")
            sys.exit(0)
        for field, bad in [('size',8),('version',2),('baud',0),('parity',5),('data_bits',4),('stop_half_bits',3),('outputs',4),('idle',2)]:
            good = getattr(cfg,field)
            setattr(cfg,field,bad)
            status,missing = opening()
            assert status != 0 and missing.value is None, field
            setattr(cfg,field,good)
        interposer.rpt_test_modem_set(termios.TIOCM_DTR)
        status, current = opening()
        assert status == 0 and current.value, status
        assert interposer.rpt_test_modem_get() & (termios.TIOCM_DTR | termios.TIOCM_RTS) == termios.TIOCM_RTS
        cfg.path = os.fsencode(path)
        status, other = opening()
        assert status != 0 and other.value is None, "alias must not open a second owner"
        for state, expected in [(termios.TIOCM_CTS | termios.TIOCM_CAR,5),(termios.TIOCM_DSR | termios.TIOCM_RNG,10)]:
            interposer.rpt_test_modem_set(state | termios.TIOCM_RTS)
            result, feedback = word(99),word(99)
            assert service(current,1,c.byref(result),c.byref(feedback)) == 0
            assert (result.value, feedback.value) == (expected,0)
        interposer.rpt_test_modem_fail(termios.TIOCMGET)
        assert service(current,1,c.byref(result),c.byref(feedback)) != 0
        assert (result.value,feedback.value) == (0,0)
        interposer.rpt_test_modem_set(0)
        interposer.rpt_test_modem_fail(termios.TIOCMSET)
        assert close(current) != 0
        status, still_owned = opening()
        assert status != 0 and still_owned.value is None
        assert close(current) == 0
        assert interposer.rpt_test_modem_get() & termios.TIOCM_RTS
        status, next_owner = opening()
        assert status == 0 and next_owner.value
        assert close(next_owner) == 0
        # DTR encoder is active low, RTS PTT is active high.
        cfg.outputs, cfg.idle = 3, 2
        status, device = opening()
        assert status == 0
        levels = c.CFUNCTYPE(c.c_int, handle, word, c.POINTER(word), c.POINTER(word))(d.service_outputs)
        for requested in [0,1,3,2,0]:
            assert levels(device,requested,c.byref(result),c.byref(feedback)) == 0
            assert feedback.value == requested ^ 2
        assert levels(device,4,c.byref(result),c.byref(feedback)) != 0
        assert close(device) == 0
        print("Actual tty open/alias exclusion, termios, readback, inverted RTS, unused DTR idle, close and reacquisition passed.")
finally:
    os.close(slave)
    os.close(master)
