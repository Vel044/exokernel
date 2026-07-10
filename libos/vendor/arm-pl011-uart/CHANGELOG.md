# Changelog

## 0.5.0

### Breaking changes

- Updated `safe-mmio` to 0.3.0.

## 0.4.0

### Breaking changes

- Updated to `embedded-io` 0.7.1.

### Other changes

- Updated to 2024 edition. This increases the MSRV to 1.85.

## 0.3.2

- Moved repository under arm-firmware-crates.
- Updated trademark notice in readme.
- Updated copyright notices to refer to contributors rather than Arm.

## 0.3.1

- Updated `safe-mmio` dependency to 0.2.1.
- Added example to documentation.

## 0.3.0

- Use safe MMIO methods from new version of safe-mmio

## 0.2.0

- Made `embedded-hal` and `embedded-io-nb` trait implementations optional, behind corresponding
  feature flags.
- Added methods to read, mask and clear interrupts.
- Added methods to set FIFO level of RX/TX interrupts

## 0.1.0

Initial release.
