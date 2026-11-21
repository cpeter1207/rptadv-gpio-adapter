/* Test-only syscall interposition for the real pseudo-terminal used by this process. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <asm/termbits.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <linux/ppdev.h>
#include <stdatomic.h>
#include <string.h>
#include <stdarg.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>
static unsigned char parallel_trace[1024];
static int parallel_writes;
static atomic_int pause_cat, cat_entered;
void rpt_test_cat_pause(int pause) { atomic_store(&pause_cat, pause); }
int rpt_test_cat_pending(void) { return atomic_load(&cat_entered); }
/* Same C ABI as Hamlib rig_get_ptt; no private rig fields are examined. */
int rig_get_ptt(void *rig, int vfo, int *ptt) {
    int (*next)(void *, int, int *) = dlsym(RTLD_NEXT, "rig_get_ptt");
    atomic_store(&cat_entered, 1);
    while (atomic_load(&pause_cat)) usleep(1000);
    atomic_store(&cat_entered, 0);
    return next(rig, vfo, ptt);
}
int rpt_test_parallel_writes(void) { return parallel_writes; }
int rpt_test_parallel_level(int i) { return i >= 0 && i < parallel_writes ? parallel_trace[i] : -1; }
static unsigned char supported[96], pressed[96];
static int keyboard_offline;
static unsigned keyboard_bytes = sizeof(pressed);
static int modem_state;
static unsigned long fail_request, persistent_request;
static int fault_after_setup;
static unsigned long fault_request;
static int fault_skip, fault_times, mismatch_field, termios_reads, ignore_set;
static int evdev_result = -2, fail_pressed, fail_flock;
void rpt_test_fail_pressed(int value) { fail_pressed = value; }
void rpt_test_fail_flock(void) { fail_flock = 1; }
void rpt_test_ioctl_fault(unsigned long request, int skip, int times)
{
    fault_request = request; fault_skip = skip; fault_times = times;
}
void rpt_test_termios_mismatch(int field) { mismatch_field = field; termios_reads = 0; }
void rpt_test_ignore_modem_set(int value) { ignore_set = value; }
void rpt_test_evdev_result(int value) { evdev_result = value; }
static int selected(int fd, const char *name)
{
    const char *path = getenv(name);
    struct stat actual, target;
    return path && fstat(fd, &actual) == 0 && stat(path, &target) == 0
        && S_ISCHR(actual.st_mode) && actual.st_rdev == target.st_rdev;
}
int flock(int fd, int operation)
{
    int (*next)(int, int) = dlsym(RTLD_NEXT, "flock");
    if (fail_flock && selected(fd, "RPT_TEST_MODEM_TTY")) {
        fail_flock = 0; errno = EWOULDBLOCK; return -1;
    }
    return next(fd, operation);
}
int ioctl(int fd, unsigned long request, ...) {
    int (*next)(int, unsigned long, ...) = dlsym(RTLD_NEXT, "ioctl");
    if (request == fault_request && selected(fd, "RPT_TEST_MODEM_TTY")) {
        if (fault_skip) --fault_skip;
        else if (fault_times) { --fault_times; errno = EIO; return -1; }
    }
    if (request == TIOCEXCL || request == TIOCNXCL)
        return next(fd, request);
    va_list arguments;
    va_start(arguments, request);
    void *argument = va_arg(arguments, void *);
    va_end(arguments);
    if (request == PPCLAIM || request == PPRELEASE || request == PPWDATA || request == PPRSTATUS) {
        const char *path = getenv("RPT_TEST_PARALLEL");
        struct stat actual, target;
        if (path && fstat(fd, &actual) == 0 && stat(path, &target) == 0 && S_ISCHR(actual.st_mode) && actual.st_rdev == target.st_rdev) {
            if (request == PPWDATA && parallel_writes < (int)sizeof(parallel_trace))
                parallel_trace[parallel_writes++] = *(unsigned char *)argument;
            if (request == PPRSTATUS) *(unsigned char *)argument = 0xff;
            return 0;
        }
    }
    if (request == EVIOCGBIT(EV_KEY,sizeof(supported)) || request == EVIOCGKEY(sizeof(pressed))) {
        const char *path = getenv("RPT_TEST_KEYBOARD");
        struct stat actual, target;
        if (path && fstat(fd,&actual) == 0 && stat(path,&target) == 0 && S_ISCHR(actual.st_mode) && actual.st_rdev == target.st_rdev) {
            if (fail_pressed && request == EVIOCGKEY(sizeof(pressed))) { errno = EIO; return -1; }
            if (evdev_result != -2) return evdev_result;
            if (keyboard_offline) { errno=ENODEV; return -1; }
            memcpy(argument,request == EVIOCGKEY(sizeof(pressed)) ? pressed : supported,keyboard_bytes);
            return (int)keyboard_bytes;
        }
    }
    if (request == TCGETS2 && mismatch_field && selected(fd, "RPT_TEST_MODEM_TTY")) {
        int rc = next(fd, request, argument);
        if (!rc && ++termios_reads == 2) {
            struct termios2 *settings = argument;
            if (mismatch_field == 1) settings->c_cflag ^= PARENB;
            if (mismatch_field == 2) ++settings->c_ispeed;
            if (mismatch_field == 3) ++settings->c_ospeed;
            if (mismatch_field == 4) settings->c_iflag |= IXON;
            mismatch_field = 0;
        }
        return rc;
    }
    if (request == 0x402c542bUL && fault_after_setup) { /* TCSETS2 on amd64 and arm64 */
        int rc = next(fd, request, argument);
        if (!rc) {
            modem_state = TIOCM_RTS;
            persistent_request = TIOCMSET;
            fault_after_setup = 0;
        }
        return rc;
    }
    if (request == TIOCMGET || request == TIOCMSET) {
        const char *path = getenv("RPT_TEST_MODEM_TTY");
        struct stat actual, target;
        if (path && fstat(fd, &actual) == 0 && stat(path, &target) == 0 &&
            S_ISCHR(actual.st_mode) && actual.st_rdev == target.st_rdev) {
            if (request == fail_request || request == persistent_request) {
                fail_request = 0;
                errno = EIO;
                return -1;
            }
            if (request == TIOCMGET)
                *(int *)argument = modem_state;
            else if (ignore_set)
                --ignore_set;
            else
                modem_state = (modem_state & ~(TIOCM_RTS | TIOCM_DTR)) | (*(int *)argument & (TIOCM_RTS | TIOCM_DTR));
            return 0;
        }
    }
    return next(fd, request, argument);
}
void rpt_test_modem_fail_persistent(unsigned long request) { persistent_request = request; }
void rpt_test_modem_fail_after_setup(void) { fault_after_setup = 1; }
void rpt_test_modem_set(int value) { modem_state = value; }
int rpt_test_modem_get(void) { return modem_state; }
void rpt_test_modem_fail(unsigned long request) { fail_request = request; }
void rpt_test_evdev_key(unsigned code, int present, int held) {
    if (code >= sizeof(pressed)*8) return;
    unsigned char bit=(unsigned char)(1u << (code % 8));
    if (present) supported[code / 8] |= bit;
    else supported[code / 8] &= (unsigned char)~bit;
    if (held) pressed[code / 8] |= bit;
    else pressed[code / 8] &= (unsigned char)~bit;
}
void rpt_test_evdev_offline(int value) { keyboard_offline=value; }
void rpt_test_evdev_bytes(unsigned value) { keyboard_bytes=value <= sizeof(pressed) ? value : sizeof(pressed); }
