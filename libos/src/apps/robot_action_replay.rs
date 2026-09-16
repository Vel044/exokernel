//! 执行HVF阶段已经生成的单个ACT Action Chunk。
//!
//! 本应用不采集相机、不运行模型；它从只读ext4读取600个float32，经现有
//! CDC ACM/SCServo驱动以30Hz执行。策略层负责限幅和失败关扭矩，USB协议仍
//! 位于drivers，IRQ等待仍由runtime::usb_executor负责。

use crate::drivers::{scservo::FeetechMotorsBus, usb_serial::UsbSerialTransport};

const ACTION_VA: u64 = exo_abi::ACT_INPUT_ARENA_BASE;
const ACTION_DIM: usize = 6;
const ACTION_STEPS: usize = 100;

pub(crate) async fn run(
    info: &exo_abi::UserBootInfo,
    transport: &mut UsbSerialTransport,
) -> Result<(), u64> {
    let filesystem = crate::fs::FileSystem::mount(info).map_err(|_| 0x780u64)?;
    let file = filesystem
        .read_mapped("/actions.f32le", ACTION_VA, (ACTION_STEPS * ACTION_DIM * 4) as u64)
        .map_err(|_| 0x781u64)?;
    if file.as_slice().len() != ACTION_STEPS * ACTION_DIM * 4 {
        return Err(0x782);
    }
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
    for (slot, bytes) in actions.iter_mut().flatten().zip(file.as_slice().chunks_exact(4)) {
        *slot = f32::from_le_bytes(bytes.try_into().unwrap());
        if !slot.is_finite() {
            return Err(0x783);
        }
    }
    let mut bus = FeetechMotorsBus::so101(transport);
    let result = async {
        bus.connect().await.map_err(|_| 0x784u64)?;
        bus.read_calibration().await.map_err(|_| 0x785u64)?;
        bus.enable_torque(None).await.map_err(|_| 0x786u64)?;
        let torque = bus.sync_read_torque_enabled().await.map_err(|_| 0x787u64)?;
        if torque.iter().any(|value| *value != 1) {
            return Err(0x788);
        }
        crate::runtime::puts(b"[libos] replay torque enabled; executing ACT action chunk\r\n");
        execute_actions(&mut bus, actions).await?;
        Ok::<(), u64>(())
    };
    let result = result.await;
    // 无论动作成功还是失败，都通过同一USB执行器提交Torque_Enable=0。
    let disabled = bus.disable_torque(None).await;
    if result.is_err() || disabled.is_err() {
        return Err(result.err().unwrap_or(0x789));
    }
    crate::runtime::puts(b"[libos] replay complete; torque disabled\r\n");
    Ok(())
}

async fn execute_actions(
    bus: &mut FeetechMotorsBus<'_>,
    actions: [[f32; ACTION_DIM]; ACTION_STEPS],
) -> Result<(), u64> {
    let frequency = crate::runtime::counter_frequency().max(1);
    let period = (frequency / 30).max(1);
    let mut deadline = crate::runtime::counter();
    let initial = bus.sync_read_positions_f32().await.map_err(|_| 0x790u64)?;
    let mut commanded = initial;
    for (step, action) in actions.into_iter().enumerate() {
        let current = bus.sync_read_positions_f32().await.map_err(|_| 0x791u64)?;
        let mut target = commanded;
        for joint in 0..ACTION_DIM {
            if !action[joint].is_finite() {
                return Err(0x792);
            }
            let next = commanded[joint] + (action[joint] - commanded[joint]).clamp(-0.1, 0.1);
            target[joint] = current[joint] + (next - current[joint]).clamp(-2.0, 2.0);
        }
        commanded = target;
        bus.sync_write_positions_f32(target)
            .await
            .map_err(|_| 0x793u64)?;
        if step % 10 == 0 || step + 1 == ACTION_STEPS {
            crate::runtime::puts(b"[libos] replay action step=");
            crate::runtime::hex((step + 1) as u64);
            crate::runtime::puts(b"/100\r\n");
        }
        deadline = deadline.wrapping_add(period);
        while crate::runtime::counter().wrapping_sub(deadline) > u64::MAX / 2 {
            core::hint::spin_loop();
        }
    }
    crate::runtime::delay_ns(1_000_000_000);
    let measured = bus.sync_read_positions_f32().await.map_err(|_| 0x794u64)?;
    if measured
        .iter()
        .zip(initial)
        .all(|(after, before)| (*after - before).abs() < 0.5)
    {
        return Err(0x795);
    }
    Ok(())
}
