# Arm PrimeCell UART (PL011) driver

Driver implementation for the [PL011 UART peripheral](https://developer.arm.com/documentation/ddi0183/latest/).

The driver is designed to function regardless of whether a Memory Management Unit (MMU) is present.
The primary role of the `OwnedMmioPointer` is to manage the lifetime of the peripheral, ensuring
proper resource handling. In a system that includes an MMU, the peripheral's lifetime is dynamic
because it is mapped into memory rather than having a fixed address. In a system without an MMU, the
`OwnedMmioPointer` can be instantiated directly from the physical address of the register block,
providing access to the peripheral without requiring memory mapping.

## Implemented features

* Enabling/disabling UART peripheral which includes configuring the data bit number, parity settings, stop bit count
  and baudrate
* Checking various status flags
* Non-blocking read/write functions
* Handling UART errors
* Reading UART identification structure
* 98% unit test coverage
* Setting FIFO level of RX/TX interrupts
* Reading, masking and clearing interrupts
* Implementing various traits
  * `embedded_hal_nb::serial::{Write, Read}` (optional, behind the `embedded-hal-nb` feature flag)
  * `embedded_io::{Write, Read}` (optional, behind the `embedded-io` feature flag)
  * `core::fmt::Write`

## Feature flags

* `embedded-hal-nb`: Adds implementations of `embedded-hal-nb` traits for the UART driver.
* `embedded-io`: Adds implementations of `embedded-io` traits for the UART driver.

## Future plans

* Handling modem control and status signals
* Adding peripheral testing

## License

The project is MIT and Apache-2.0 dual licensed, see `LICENSE-APACHE` and `LICENSE-MIT`.

## Maintainers

arm-pl011-uart is a trustedfirmware.org maintained project. All contributions are ultimately merged by the maintainers
listed below.

* Bálint Dobszay <balint.dobszay@arm.com>
  [balint-dobszay-arm](https://github.com/balint-dobszay-arm)
* Imre Kis <imre.kis@arm.com>
  [imre-kis-arm](https://github.com/imre-kis-arm)
* Sandrine Afsa <sandrine.afsa@arm.com>
  [sandrine-bailleux-arm](https://github.com/sandrine-bailleux-arm)

## Contributing

Please follow the directions of the [Trusted Firmware Processes](https://trusted-firmware-docs.readthedocs.io/en/latest/generic_processes/index.html)

Contributions are handled through [review.trustedfirmware.org](https://review.trustedfirmware.org/q/project:arm-firmware-crates/arm-pl011-uart).

## Arm trademark notice

Arm is a registered trademark of Arm Limited (or its subsidiaries or affiliates).

This project uses some of the Arm product, service or technology trademarks, as listed in the
[Trademark List][1], in accordance with the [Arm Trademark Use Guidelines][2].

Subsequent uses of these trademarks throughout this repository do not need to be prefixed with the
Arm word trademark.

[1]: https://www.arm.com/company/policies/trademarks/arm-trademark-list
[2]: https://www.arm.com/company/policies/trademarks/guidelines-trademarks

--------------

*Copyright The arm-pl011-uart Contributors.*
