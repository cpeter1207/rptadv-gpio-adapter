# rptadv-gpio-adapter

`rptadv-gpio-adapter` is the radio GPIO boundary for `rpt_advanced`. It owns
exclusive CM119 HID interface access, logical PTT, ordinary GPIO outputs, and
active-low COR and external-CTCSS snapshots for one explicitly selected CM119
device. It also owns CM119 tuning EEPROM access and configured Linux
parallel-port GPIO.

This is the GPIO portion of ADR 0028's replacement for `res_usbradio.so`:

- CM119 discovery, HID signaling, EEPROM, and CM108AH HOOK-to-GPIO2 mapping;
- explicit ppdev or x86 raw-I/O parallel transport, with lock-free snapshots
  and output publication; and
- no PCM transport, ALSA mixer access, DSP, Asterisk helpers, or radio policy.

The independently released PortAudio/ALSA adapter owns audio, mixer controls,
and raw audio statistics. A separately selected Hamlib adapter owns CAT radio
control when a node uses it; Hamlib is not required for CM119 or parallel GPIO.

## ABI

ABI major 1 is exported by `librptadv_gpio_adapter.so.1`.  Consumers use the
public descriptor in `include/rptadv_gpio_adapter/rptadv_gpio_adapter.h`; it
does not expose libusb types.  The descriptor first probes a stable Linux USB
topology (for example `3-1`), then opens the same identity only after the
prior HID owner has released it.  A device can have only one HID owner.

One non-real-time service owner performs synchronous HID transfers. Native
ticks atomically publish prepared PTT/GPIO actions and read the latest input
snapshot without touching libusb, allocating, or taking a lock. Discovery and
EEPROM access are control-plane operations and use that same service owner;
they must never be called from an audio tick. Discovery returns stable USB
topology and an optional serial number, but never assigns a device
automatically. EEPROM operations preserve the established ASL3 user tuning
region at physical words 51 through 63, including its magic value and
checksum; manufacturer-reserved EEPROM words are never read or written.

ABI-1 descriptor additions provide both single and independently scheduled
baseline-XOR output pulses for CM119 PTT/GPIO and parallel data pins. Every
scheduled bit has its own monotonic deadline, which the service owner starts
when it observes the request; overlapping bits therefore expire independently.
Disjoint scheduled requests made before one service cycle are retained; a later
request for the same bit replaces its deadline.
They preserve legacy pulse polarity: a pulse temporarily inverts the persistent
output baseline, then the service owner restores that baseline at its deadline.
This supports active-low or active-high PTT and clip-LED wiring without
transport I/O from a native tick.

Parallel ports are selected only by an explicit ppdev path or raw I/O base;
the adapter never scans or claims an arbitrary host port. Use ppdev where it is
available. Direct I/O is limited to x86 Linux, requires process I/O permission,
and is rejected rather than emulated on other architectures. A non-real-time
service owner performs every port operation. Native ticks publish output
actions and read snapshots only.

Append-only ABI-1 descriptor operations retain the established active-low
four-bit binary channel selector and the RTX two-word serial programming
protocol. They are serialized control-plane operations, not native-tick
operations. RTX programming retains the legacy 20-bit most-significant-bit
order, settling intervals, always-low TX-power bit, and immediate TX-clear
behavior.

## Build

Install `libusb-1.0-0-dev`, Rust, and the ordinary project quality tools, then
run:

```sh
make
make test
```

The resulting package layout contains only the dynamic shared object, public
header, and pkg-config metadata.  No static archive is shipped.

## License

GPL-2.0-only.  See [COPYING](COPYING).
