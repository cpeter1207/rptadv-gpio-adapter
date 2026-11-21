/* Test-only syscall interposition for the real pseudo-terminal used by this process. */
#define _GNU_SOURCE
#include <dlfcn.h>
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
int ioctl(int fd, unsigned long request, ...) {
    int (*next)(int, unsigned long, ...) = dlsym(RTLD_NEXT, "ioctl");
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
            if (keyboard_offline) { errno=ENODEV; return -1; }
            memcpy(argument,request == EVIOCGKEY(sizeof(pressed)) ? pressed : supported,keyboard_bytes);
            return (int)keyboard_bytes;
        }
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
