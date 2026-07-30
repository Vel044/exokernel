//! SO101的SCServo启动与运动策略。
//!
//! Protocol 0封包、寄存器和Motor Bus API位于drivers::scservo；这里仅决定
//! 启动时PING/读取哪些电机，以及是否平滑移动到校准中位。

use crab_usb::EventHandler;

use crate::apps::usb_task::{signal_ready, ReadySignal};
#[cfg(feature = "scservo-move")]
use crate::drivers::scservo::ControlTable;
use crate::drivers::scservo::{FeetechMotorsBus, MotorName, ScservoError};
use crate::drivers::usb_serial::UsbSerialTransport;

pub(crate) fn run(
    transport: &mut UsbSerialTransport,
    handler: &EventHandler,
    intid: u32,
    notification: &crate::notification::Notification,
    ready_signal: Option<ReadySignal>,
) -> ! {
    let mut bus = FeetechMotorsBus::so101(transport);
    let result =
        crate::runtime::usb_executor::block_on_usb(startup(&mut bus), handler, intid, notification);
    match result {
        Ok(()) => {
            #[cfg(feature = "scservo-move")]
            {
                if let Err(error) = crate::runtime::usb_executor::block_on_usb(
                    move_to_neutral(&mut bus),
                    handler,
                    intid,
                    notification,
                ) {
                    // 运动失败也尽力释放六个舵机，避免错误退出后继续保持扭矩。
                    let _ = crate::runtime::usb_executor::block_on_usb(
                        bus.disable_torque(None),
                        handler,
                        intid,
                        notification,
                    );
                    crate::runtime::puts(b"[libos] neutral move failed code=");
                    crate::runtime::hex(error_code(error));
                    crate::runtime::puts(b"\r\n");
                    crate::runtime::exit(0x301);
                }
                crate::runtime::puts(b"[libos] SCServo motion startup complete\r\n");
            }
            #[cfg(not(feature = "scservo-move"))]
            crate::runtime::puts(b"[libos] SCServo read-only startup complete\r\n");

            // system-smoke等到协议探测和可选运动完成后才继续验收。
            signal_ready(ready_signal);
            loop {
                crate::runtime::delay_ns(1_000_000_000);
            }
        }
        Err(error) => {
            crate::runtime::puts(b"[libos] SCServo startup failed code=");
            crate::runtime::hex(error_code(error));
            crate::runtime::puts(b"\r\n");
            crate::runtime::exit(0x300);
        }
    }
}

async fn startup(bus: &mut FeetechMotorsBus<'_>) -> Result<(), ScservoError> {
    crate::runtime::puts(b"[libos] SCServo baudrate=1000000\r\n");
    bus.connect().await?;
    bus.read_calibration().await?;
    for motor in MotorName::ALL {
        let position = bus.read_position(motor).await?;
        let status = bus.read_status(motor).await?;
        crate::runtime::puts(b"[libos] READ Present_Position ");
        crate::runtime::puts(motor.label());
        crate::runtime::puts(b" value=");
        crate::runtime::hex(position as u64);
        crate::runtime::puts(b" status=");
        crate::runtime::hex(status.error as u64);
        crate::runtime::puts(b" success\r\n");
    }
    Ok(())
}

#[cfg(feature = "scservo-move")]
async fn move_to_neutral(bus: &mut FeetechMotorsBus<'_>) -> Result<(), ScservoError> {
    // 前五个关节归一化中点为0，夹爪中点为50%。
    let neutral = [Some(0), Some(0), Some(0), Some(0), Some(0), Some(50)];
    let mut current = bus.sync_read(ControlTable::PresentPosition, true).await?;

    bus.enable_torque(None).await?;
    crate::runtime::puts(b"[libos] moving to calibrated neutral pose\r\n");

    let mut round = 0;
    let mut neutral_reached = false;
    while round < 40 {
        let mut command = [None; 6];
        let mut reached = true;
        for index in 0..6 {
            let present = current[index].ok_or(ScservoError::Timeout)?;
            let target = neutral[index].unwrap();
            let delta = target - present;
            if delta.unsigned_abs() > 3 {
                reached = false;
            }
            command[index] = Some(if delta > 5 {
                present + 5
            } else if delta < -5 {
                present - 5
            } else {
                target
            });
        }
        bus.sync_write(ControlTable::GoalPosition, command, true)
            .await?;
        if reached {
            neutral_reached = true;
            break;
        }
        crate::runtime::delay_ns(100_000_000);
        current = bus.sync_read(ControlTable::PresentPosition, true).await?;
        round += 1;
    }
    if !neutral_reached {
        return Err(ScservoError::Timeout);
    }

    bus.disable_torque(None).await?;
    crate::runtime::puts(b"[libos] calibrated neutral pose reached; torque disabled\r\n");
    Ok(())
}

fn error_code(error: ScservoError) -> u64 {
    match error {
        ScservoError::Transport => 1,
        ScservoError::Timeout => 2,
        ScservoError::MalformedPacket => 3,
        ScservoError::Checksum => 4,
        ScservoError::WrongId => 5,
        ScservoError::Device(value) => 0x100 + value as u64,
        ScservoError::InvalidArgument => 6,
        ScservoError::NotConnected => 7,
    }
}
