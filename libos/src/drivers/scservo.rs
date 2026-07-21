//! Feetech STS/SMS Protocol 0 与 SO101 电机总线。
//!
//! 这一层只处理 CDC ACM 已经提供的串行字节流；xHCI、DMA、IRQ 和 USB
//! descriptor 都留在 `usb_app`，因此同一套协议可以运行在 QEMU PCI xHCI
//! 和 Pi5 直连 xHCI 两条后端上。

use alloc::vec::Vec;

use crate::usb_app::CdcAcmTransport;
use crab_usb::EventHandler;

pub const BROADCAST_ID: u8 = 0xfe;
pub const INST_PING: u8 = 1;
pub const INST_READ: u8 = 2;
pub const INST_WRITE: u8 = 3;
pub const INST_REG_WRITE: u8 = 4;
pub const INST_ACTION: u8 = 5;
pub const INST_SYNC_READ: u8 = 0x82;
pub const INST_SYNC_WRITE: u8 = 0x83;

const MAX_PACKET_LEN: usize = 250;
const MAX_PARAMS: usize = MAX_PACKET_LEN - 6;
const DEFAULT_TIMEOUT_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScservoError {
    Transport,
    Timeout,
    MalformedPacket,
    Checksum,
    WrongId,
    Device(u8),
    InvalidArgument,
    NotConnected,
}

#[derive(Clone, Copy, Debug)]
pub struct StatusPacket {
    pub id: u8,
    pub error: u8,
    pub params: [u8; MAX_PARAMS],
    pub param_len: usize,
}

impl StatusPacket {
    fn empty() -> Self {
        Self {
            id: 0,
            error: 0,
            params: [0; MAX_PARAMS],
            param_len: 0,
        }
    }
}

/// Protocol 0 的收发器。`rx` 保留上一次 USB bulk transfer 的半包和粘包。
pub struct Protocol0<'a> {
    transport: &'a mut CdcAcmTransport,
    rx: Vec<u8>,
    input: [u8; 512],
}

impl<'a> Protocol0<'a> {
    pub fn new(transport: &'a mut CdcAcmTransport) -> Self {
        Self {
            transport,
            rx: Vec::new(),
            input: [0; 512],
        }
    }

    /// `FF FF ID LENGTH INSTRUCTION PARAM... CHECKSUM`。
    pub fn build_packet(id: u8, instruction: u8, params: &[u8]) -> Result<Vec<u8>, ScservoError> {
        // 总长度 = 两个包头 + ID + LENGTH + 指令 + 参数 + checksum。
        let total = params
            .len()
            .checked_add(6)
            .ok_or(ScservoError::InvalidArgument)?;
        if total > MAX_PACKET_LEN || id == 0xff || params.len() > MAX_PARAMS {
            return Err(ScservoError::InvalidArgument);
        }

        let mut packet = Vec::with_capacity(total);
        packet.push(0xff);
        packet.push(0xff);
        packet.push(id);
        // LENGTH 包含 instruction、所有参数和 checksum。
        packet.push((params.len() + 2) as u8);
        packet.push(instruction);
        packet.extend_from_slice(params);
        packet.push(checksum(&packet[2..]));
        Ok(packet)
    }

    async fn send(&mut self, id: u8, instruction: u8, params: &[u8]) -> Result<(), ScservoError> {
        let packet = Self::build_packet(id, instruction, params)?;
        let written = self
            .transport
            .write(&packet)
            .await
            .map_err(|_| ScservoError::Transport)?;
        if written != packet.len() {
            return Err(ScservoError::Transport);
        }
        Ok(())
    }

    async fn transaction(
        &mut self,
        id: u8,
        instruction: u8,
        params: &[u8],
    ) -> Result<StatusPacket, ScservoError> {
        if id >= BROADCAST_ID {
            return Err(ScservoError::InvalidArgument);
        }
        let packet = Self::build_packet(id, instruction, params)?;
        let (written, received) = self
            .transport
            .exchange(&packet, &mut self.input)
            .await
            .map_err(|_| ScservoError::Transport)?;
        if written != packet.len() {
            return Err(ScservoError::Transport);
        }
        if received != 0 {
            self.rx.extend_from_slice(&self.input[..received]);
        }
        let packet = self.read_packet(id, DEFAULT_TIMEOUT_NS).await?;
        if packet.error != 0 {
            return Err(ScservoError::Device(packet.error));
        }
        Ok(packet)
    }

    pub async fn ping(&mut self, id: u8) -> Result<u16, ScservoError> {
        self.transaction(id, INST_PING, &[]).await?;
        self.read_u16(id, 3).await
    }

    pub async fn read(&mut self, id: u8, address: u8, length: u8) -> Result<Vec<u8>, ScservoError> {
        let packet = self.transaction(id, INST_READ, &[address, length]).await?;
        if packet.param_len < length as usize {
            return Err(ScservoError::MalformedPacket);
        }
        Ok(packet.params[..length as usize].to_vec())
    }

    pub async fn write(&mut self, id: u8, address: u8, data: &[u8]) -> Result<(), ScservoError> {
        if data.len() > MAX_PARAMS - 1 {
            return Err(ScservoError::InvalidArgument);
        }
        let mut params = Vec::with_capacity(data.len() + 1);
        params.push(address);
        params.extend_from_slice(data);
        self.transaction(id, INST_WRITE, &params).await.map(|_| ())
    }

    pub async fn reg_write(
        &mut self,
        id: u8,
        address: u8,
        data: &[u8],
    ) -> Result<(), ScservoError> {
        let mut params = Vec::with_capacity(data.len() + 1);
        params.push(address);
        params.extend_from_slice(data);
        self.transaction(id, INST_REG_WRITE, &params)
            .await
            .map(|_| ())
    }

    pub async fn action(&mut self) -> Result<(), ScservoError> {
        // ACTION 是广播指令，设备不会返回状态包。
        self.send(BROADCAST_ID, INST_ACTION, &[]).await
    }

    pub async fn sync_write(
        &mut self,
        address: u8,
        data_len: u8,
        values: &[(u8, &[u8])],
    ) -> Result<(), ScservoError> {
        if values.is_empty()
            || values
                .iter()
                .any(|(_, data)| data.len() != data_len as usize)
        {
            return Err(ScservoError::InvalidArgument);
        }
        let mut params = Vec::with_capacity(2 + values.len() * (data_len as usize + 1));
        params.push(address);
        params.push(data_len);
        for (id, data) in values {
            params.push(*id);
            params.extend_from_slice(data);
        }
        self.send(BROADCAST_ID, INST_SYNC_WRITE, &params).await
    }

    pub async fn sync_read(
        &mut self,
        address: u8,
        data_len: u8,
        ids: &[u8],
    ) -> Result<Vec<StatusPacket>, ScservoError> {
        if ids.is_empty() || ids.iter().any(|id| *id >= BROADCAST_ID) {
            return Err(ScservoError::InvalidArgument);
        }
        let mut params = Vec::with_capacity(ids.len() + 2);
        params.push(address);
        params.push(data_len);
        params.extend_from_slice(ids);
        self.send(BROADCAST_ID, INST_SYNC_READ, &params).await?;

        let mut packets = Vec::with_capacity(ids.len());
        for id in ids {
            let packet = self.read_packet(*id, DEFAULT_TIMEOUT_NS).await?;
            if packet.error != 0 {
                return Err(ScservoError::Device(packet.error));
            }
            if packet.param_len < data_len as usize {
                return Err(ScservoError::MalformedPacket);
            }
            packets.push(packet);
        }
        Ok(packets)
    }

    pub async fn broadcast_ping(&mut self, ids: &[u8]) -> Result<Vec<(u8, u16)>, ScservoError> {
        // 对 Protocol 0，广播 PING 的多个响应需要独立收包；逐 ID PING
        // 虽然多占几帧，但不会让无方向控制的 CDC ACM 转接器发生碰撞。
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(model) = self.ping(*id).await {
                result.push((*id, model));
            }
        }
        Ok(result)
    }

    async fn read_u16(&mut self, id: u8, address: u8) -> Result<u16, ScservoError> {
        let data = self.read(id, address, 2).await?;
        Ok(u16::from_le_bytes([data[0], data[1]]))
    }

    async fn read_packet(
        &mut self,
        expected_id: u8,
        timeout_ns: u64,
    ) -> Result<StatusPacket, ScservoError> {
        let deadline = crate::runtime::counter().wrapping_add(ticks_for_ns(timeout_ns));
        loop {
            match self.try_parse_packet() {
                Ok(Some(packet)) => {
                    if packet.id == expected_id {
                        return Ok(packet);
                    }
                    // 粘包中可能有别的设备响应；丢弃它，继续寻找目标 ID。
                    continue;
                }
                Ok(None) => {}
                // 丢弃损坏帧后继续同步，避免一个 USB 分片直接终止总线。
                Err(ScservoError::Checksum) => continue,
                Err(error) => return Err(error),
            }

            if expired(deadline) {
                return Err(ScservoError::Timeout);
            }
            let length = self
                .transport
                .read(&mut self.input)
                .await
                .map_err(|_| ScservoError::Transport)?;
            if length != 0 {
                self.rx.extend_from_slice(&self.input[..length]);
                if self.rx.len() > MAX_PACKET_LEN * 4 {
                    let keep = self.rx.len().saturating_sub(MAX_PACKET_LEN * 2);
                    self.rx.drain(..keep);
                }
            }
        }
    }

    fn try_parse_packet(&mut self) -> Result<Option<StatusPacket>, ScservoError> {
        let Some(header) = self.rx.windows(2).position(|pair| pair == [0xff, 0xff]) else {
            if self.rx.len() > 1 {
                let last = *self.rx.last().unwrap();
                self.rx.clear();
                if last == 0xff {
                    self.rx.push(last);
                }
            }
            return Ok(None);
        };
        if header != 0 {
            self.rx.drain(..header);
        }
        if self.rx.len() < 4 {
            return Ok(None);
        }

        let id = self.rx[2];
        let length = self.rx[3] as usize;
        let total = length.checked_add(4).ok_or(ScservoError::MalformedPacket)?;
        if id >= BROADCAST_ID || length < 2 || total > MAX_PACKET_LEN {
            self.rx.drain(..1);
            return Ok(None);
        }
        if self.rx.len() < total {
            return Ok(None);
        }

        let packet = &self.rx[..total];
        if checksum(&packet[2..total - 1]) != packet[total - 1] {
            self.rx.drain(..2);
            return Err(ScservoError::Checksum);
        }

        let param_len = length - 2;
        let mut result = StatusPacket::empty();
        result.id = id;
        result.error = packet[4];
        result.param_len = param_len;
        result.params[..param_len].copy_from_slice(&packet[5..5 + param_len]);
        self.rx.drain(..total);
        Ok(Some(result))
    }
}

pub fn checksum(bytes: &[u8]) -> u8 {
    (!bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))) & 0xff
}

fn ticks_for_ns(ns: u64) -> u64 {
    let frequency = crate::runtime::counter_frequency();
    ((frequency as u128 * ns as u128) / 1_000_000_000) as u64
}

fn expired(deadline: u64) -> bool {
    (crate::runtime::counter().wrapping_sub(deadline) as i64) >= 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotorName {
    ShoulderPan,
    ShoulderLift,
    ElbowFlex,
    WristFlex,
    WristRoll,
    Gripper,
}

impl MotorName {
    pub const ALL: [Self; 6] = [
        Self::ShoulderPan,
        Self::ShoulderLift,
        Self::ElbowFlex,
        Self::WristFlex,
        Self::WristRoll,
        Self::Gripper,
    ];

    pub const fn id(self) -> u8 {
        match self {
            Self::ShoulderPan => 1,
            Self::ShoulderLift => 2,
            Self::ElbowFlex => 3,
            Self::WristFlex => 4,
            Self::WristRoll => 5,
            Self::Gripper => 6,
        }
    }

    pub const fn label(self) -> &'static [u8] {
        match self {
            Self::ShoulderPan => b"shoulder_pan",
            Self::ShoulderLift => b"shoulder_lift",
            Self::ElbowFlex => b"elbow_flex",
            Self::WristFlex => b"wrist_flex",
            Self::WristRoll => b"wrist_roll",
            Self::Gripper => b"gripper",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotorNormMode {
    RangeM100100,
    Range0100,
    Degrees,
}

#[derive(Clone, Copy, Debug)]
pub struct MotorConfig {
    pub name: MotorName,
    pub id: u8,
    pub model: &'static [u8],
    pub norm_mode: MotorNormMode,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MotorCalibration {
    pub id: u8,
    pub drive_mode: u8,
    pub homing_offset: i32,
    pub range_min: i32,
    pub range_max: i32,
}

// 来自 lerobot 的 SO101 Follower 标定文件 R12254705。
// 启动时还会重新读取电机中的同一组寄存器，运行时以硬件数据为准。
const SO101_CALIBRATION: [MotorCalibration; 6] = [
    MotorCalibration {
        id: 1,
        drive_mode: 0,
        homing_offset: -1429,
        range_min: 776,
        range_max: 3510,
    },
    MotorCalibration {
        id: 2,
        drive_mode: 0,
        homing_offset: -1620,
        range_min: 846,
        range_max: 3090,
    },
    MotorCalibration {
        id: 3,
        drive_mode: 0,
        homing_offset: -1762,
        range_min: 915,
        range_max: 3110,
    },
    MotorCalibration {
        id: 4,
        drive_mode: 0,
        homing_offset: -1717,
        range_min: 820,
        range_max: 3160,
    },
    MotorCalibration {
        id: 5,
        drive_mode: 0,
        homing_offset: 2030,
        range_min: 2,
        range_max: 4095,
    },
    MotorCalibration {
        id: 6,
        drive_mode: 0,
        homing_offset: 1602,
        range_min: 1953,
        range_max: 3525,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlTable {
    ModelNumber,
    Id,
    BaudRate,
    HomingOffset,
    OperatingMode,
    TorqueEnable,
    GoalPosition,
    GoalVelocity,
    PresentPosition,
    PresentVelocity,
    PresentLoad,
    PresentVoltage,
    PresentTemperature,
    Status,
    Moving,
    MinPositionLimit,
    MaxPositionLimit,
}

impl ControlTable {
    pub const fn spec(self) -> (u8, u8) {
        match self {
            Self::ModelNumber => (3, 2),
            Self::Id => (5, 1),
            Self::BaudRate => (6, 1),
            Self::HomingOffset => (31, 2),
            Self::OperatingMode => (33, 1),
            Self::TorqueEnable => (40, 1),
            Self::GoalPosition => (42, 2),
            Self::GoalVelocity => (46, 2),
            Self::PresentPosition => (56, 2),
            Self::PresentVelocity => (58, 2),
            Self::PresentLoad => (60, 2),
            Self::PresentVoltage => (62, 1),
            Self::PresentTemperature => (63, 1),
            Self::Status => (65, 1),
            Self::Moving => (66, 1),
            Self::MinPositionLimit => (9, 2),
            Self::MaxPositionLimit => (11, 2),
        }
    }

    const fn sign_bit(self) -> Option<u8> {
        match self {
            Self::HomingOffset => Some(11),
            Self::GoalVelocity | Self::PresentVelocity | Self::PresentPosition => Some(15),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MotorStatus {
    pub error: u8,
    pub voltage: u8,
    pub temperature: u8,
    pub moving: u8,
}

/// FeetechMotorsBus 的固定配置 Rust 版本。
pub struct FeetechMotorsBus<'a> {
    protocol: Protocol0<'a>,
    pub motors: [MotorConfig; 6],
    pub calibration: [MotorCalibration; 6],
    pub apply_drive_mode: bool,
    connected: bool,
}

impl<'a> FeetechMotorsBus<'a> {
    pub fn so101(transport: &'a mut CdcAcmTransport) -> Self {
        Self {
            protocol: Protocol0::new(transport),
            motors: [
                MotorConfig {
                    name: MotorName::ShoulderPan,
                    id: 1,
                    model: b"sts3215",
                    norm_mode: MotorNormMode::RangeM100100,
                },
                MotorConfig {
                    name: MotorName::ShoulderLift,
                    id: 2,
                    model: b"sts3215",
                    norm_mode: MotorNormMode::RangeM100100,
                },
                MotorConfig {
                    name: MotorName::ElbowFlex,
                    id: 3,
                    model: b"sts3215",
                    norm_mode: MotorNormMode::RangeM100100,
                },
                MotorConfig {
                    name: MotorName::WristFlex,
                    id: 4,
                    model: b"sts3215",
                    norm_mode: MotorNormMode::RangeM100100,
                },
                MotorConfig {
                    name: MotorName::WristRoll,
                    id: 5,
                    model: b"sts3215",
                    norm_mode: MotorNormMode::RangeM100100,
                },
                MotorConfig {
                    name: MotorName::Gripper,
                    id: 6,
                    model: b"sts3215",
                    norm_mode: MotorNormMode::Range0100,
                },
            ],
            calibration: SO101_CALIBRATION,
            apply_drive_mode: true,
            connected: false,
        }
    }

    pub async fn connect(&mut self) -> Result<(), ScservoError> {
        for motor in self.motors {
            crate::runtime::puts(b"[libos] PING start ");
            crate::runtime::puts(motor.name.label());
            crate::runtime::puts(b" id=");
            crate::runtime::hex(motor.id as u64);
            crate::runtime::puts(b"\r\n");
            let model = self.protocol.ping(motor.id).await?;
            crate::runtime::puts(b"[libos] PING ");
            crate::runtime::puts(motor.name.label());
            crate::runtime::puts(b" id=");
            crate::runtime::hex(motor.id as u64);
            crate::runtime::puts(b" model=");
            crate::runtime::hex(model as u64);
            crate::runtime::puts(b" success\r\n");
        }
        self.connected = true;
        Ok(())
    }

    pub async fn ping(&mut self, motor: MotorName) -> Result<u16, ScservoError> {
        self.protocol.ping(motor.id()).await
    }

    pub async fn read(
        &mut self,
        table: ControlTable,
        motor: MotorName,
        normalize: bool,
    ) -> Result<i32, ScservoError> {
        self.ensure_connected()?;
        let (address, length) = table.spec();
        let bytes = self.protocol.read(motor.id(), address, length).await?;
        let mut value = little_endian(&bytes) as i32;
        if let Some(bit) = table.sign_bit() {
            value = decode_sign_magnitude(value as u32, bit);
        }
        if normalize && matches!(table, ControlTable::PresentPosition) {
            return Ok(self.normalize(motor, value));
        }
        Ok(value)
    }

    pub async fn write(
        &mut self,
        table: ControlTable,
        motor: MotorName,
        mut value: i32,
        normalize: bool,
    ) -> Result<(), ScservoError> {
        self.ensure_connected()?;
        let (address, length) = table.spec();
        if normalize && matches!(table, ControlTable::GoalPosition) {
            value = self.unnormalize(motor, value);
        }
        let encoded = if let Some(bit) = table.sign_bit() {
            encode_sign_magnitude(value, bit)?
        } else {
            value as u32
        };
        let data = little_endian_bytes(encoded, length as usize);
        self.protocol.write(motor.id(), address, &data).await
    }

    pub async fn sync_read(
        &mut self,
        table: ControlTable,
        normalize: bool,
    ) -> Result<[Option<i32>; 6], ScservoError> {
        self.ensure_connected()?;
        let (address, length) = table.spec();
        let ids = [1, 2, 3, 4, 5, 6];
        let packets = self.protocol.sync_read(address, length, &ids).await?;
        let mut values = [None; 6];
        for packet in packets {
            if packet.id >= 1 && packet.id <= 6 {
                let index = packet.id as usize - 1;
                let mut value = little_endian(&packet.params[..length as usize]) as i32;
                if let Some(bit) = table.sign_bit() {
                    value = decode_sign_magnitude(value as u32, bit);
                }
                values[index] = Some(
                    if normalize && matches!(table, ControlTable::PresentPosition) {
                        self.normalize(self.motors[index].name, value)
                    } else {
                        value
                    },
                );
            }
        }
        Ok(values)
    }

    pub async fn sync_write(
        &mut self,
        table: ControlTable,
        values: [Option<i32>; 6],
        normalize: bool,
    ) -> Result<(), ScservoError> {
        self.ensure_connected()?;
        let (address, length) = table.spec();
        let mut encoded = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            let Some(mut value) = value else { continue };
            if normalize && matches!(table, ControlTable::GoalPosition) {
                value = self.unnormalize(self.motors[index].name, value);
            }
            let raw = if let Some(bit) = table.sign_bit() {
                encode_sign_magnitude(value, bit)?
            } else {
                value as u32
            };
            encoded.push((
                self.motors[index].id,
                little_endian_bytes(raw, length as usize),
            ));
        }
        let refs: Vec<(u8, &[u8])> = encoded
            .iter()
            .map(|(id, bytes)| (*id, bytes.as_slice()))
            .collect();
        self.protocol.sync_write(address, length, &refs).await
    }

    pub async fn enable_torque(&mut self, motor: Option<MotorName>) -> Result<(), ScservoError> {
        self.write_selected(ControlTable::TorqueEnable, motor, 1)
            .await
    }

    pub async fn disable_torque(&mut self, motor: Option<MotorName>) -> Result<(), ScservoError> {
        self.write_selected(ControlTable::TorqueEnable, motor, 0)
            .await
    }

    pub async fn read_position(&mut self, motor: MotorName) -> Result<i32, ScservoError> {
        self.read(ControlTable::PresentPosition, motor, true).await
    }

    pub async fn write_position(
        &mut self,
        motor: MotorName,
        value: i32,
    ) -> Result<(), ScservoError> {
        self.write(ControlTable::GoalPosition, motor, value, true)
            .await
    }

    pub async fn read_velocity(&mut self, motor: MotorName) -> Result<i32, ScservoError> {
        self.read(ControlTable::PresentVelocity, motor, false).await
    }

    pub async fn read_load(&mut self, motor: MotorName) -> Result<i32, ScservoError> {
        self.read(ControlTable::PresentLoad, motor, false).await
    }

    pub async fn read_voltage(&mut self, motor: MotorName) -> Result<i32, ScservoError> {
        self.read(ControlTable::PresentVoltage, motor, false).await
    }

    pub async fn read_temperature(&mut self, motor: MotorName) -> Result<i32, ScservoError> {
        self.read(ControlTable::PresentTemperature, motor, false)
            .await
    }

    pub async fn read_status(&mut self, motor: MotorName) -> Result<MotorStatus, ScservoError> {
        Ok(MotorStatus {
            error: self.read(ControlTable::Status, motor, false).await? as u8,
            voltage: self.read_voltage(motor).await? as u8,
            temperature: self.read_temperature(motor).await? as u8,
            moving: self.read(ControlTable::Moving, motor, false).await? as u8,
        })
    }

    pub async fn reg_write(
        &mut self,
        table: ControlTable,
        motor: MotorName,
        value: i32,
    ) -> Result<(), ScservoError> {
        self.ensure_connected()?;
        let (address, length) = table.spec();
        let raw = if let Some(bit) = table.sign_bit() {
            encode_sign_magnitude(value, bit)?
        } else {
            value as u32
        };
        self.protocol
            .reg_write(
                motor.id(),
                address,
                &little_endian_bytes(raw, length as usize),
            )
            .await
    }

    pub async fn action(&mut self) -> Result<(), ScservoError> {
        self.ensure_connected()?;
        self.protocol.action().await
    }

    pub async fn broadcast_ping(&mut self) -> Result<[Option<u16>; 6], ScservoError> {
        let ids = [1, 2, 3, 4, 5, 6];
        let found = self.protocol.broadcast_ping(&ids).await?;
        let mut result = [None; 6];
        for (id, model) in found {
            if (1..=6).contains(&id) {
                result[id as usize - 1] = Some(model);
            }
        }
        Ok(result)
    }

    pub async fn read_calibration(&mut self) -> Result<[MotorCalibration; 6], ScservoError> {
        self.ensure_connected()?;
        let mut calibration = self.calibration;
        for index in 0..self.motors.len() {
            let motor = self.motors[index].name;
            calibration[index].homing_offset =
                self.read(ControlTable::HomingOffset, motor, false).await?;
            calibration[index].range_min = self
                .read(ControlTable::MinPositionLimit, motor, false)
                .await?;
            calibration[index].range_max = self
                .read(ControlTable::MaxPositionLimit, motor, false)
                .await?;
        }
        self.calibration = calibration;
        Ok(calibration)
    }

    pub async fn write_calibration(
        &mut self,
        calibration: [MotorCalibration; 6],
    ) -> Result<(), ScservoError> {
        for index in 0..self.motors.len() {
            let motor = self.motors[index].name;
            self.write(
                ControlTable::HomingOffset,
                motor,
                calibration[index].homing_offset,
                false,
            )
            .await?;
            self.write(
                ControlTable::MinPositionLimit,
                motor,
                calibration[index].range_min,
                false,
            )
            .await?;
            self.write(
                ControlTable::MaxPositionLimit,
                motor,
                calibration[index].range_max,
                false,
            )
            .await?;
        }
        self.calibration = calibration;
        Ok(())
    }

    async fn write_selected(
        &mut self,
        table: ControlTable,
        motor: Option<MotorName>,
        value: i32,
    ) -> Result<(), ScservoError> {
        match motor {
            Some(motor) => self.write(table, motor, value, false).await,
            None => {
                let mut values = [None; 6];
                values.fill(Some(value));
                self.sync_write(table, values, false).await
            }
        }
    }

    fn ensure_connected(&self) -> Result<(), ScservoError> {
        if self.connected {
            Ok(())
        } else {
            Err(ScservoError::NotConnected)
        }
    }

    fn calibration(&self, motor: MotorName) -> MotorCalibration {
        self.calibration[motor.id() as usize - 1]
    }

    fn normalize(&self, motor: MotorName, value: i32) -> i32 {
        let calibration = self.calibration(motor);
        let min = calibration.range_min;
        let max = calibration.range_max.max(min + 1);
        let bounded = value.clamp(min, max);
        let span = (max - min) as i64;
        let mut normalized = match self.motor(motor).norm_mode {
            MotorNormMode::RangeM100100 => ((bounded - min) as i64 * 200 / span - 100) as i32,
            MotorNormMode::Range0100 => ((bounded - min) as i64 * 100 / span) as i32,
            MotorNormMode::Degrees => ((value - (min + max) / 2) as i64 * 360 / 4095) as i32,
        };
        if self.apply_drive_mode && calibration.drive_mode != 0 {
            normalized = match self.motor(motor).norm_mode {
                MotorNormMode::Range0100 => 100 - normalized,
                _ => -normalized,
            };
        }
        normalized
    }

    fn unnormalize(&self, motor: MotorName, value: i32) -> i32 {
        let calibration = self.calibration(motor);
        let min = calibration.range_min;
        let max = calibration.range_max.max(min + 1);
        let mut value = value;
        if self.apply_drive_mode && calibration.drive_mode != 0 {
            value = match self.motor(motor).norm_mode {
                MotorNormMode::Range0100 => 100 - value,
                _ => -value,
            };
        }
        match self.motor(motor).norm_mode {
            MotorNormMode::RangeM100100 => {
                min + ((value.clamp(-100, 100) + 100) * (max - min) / 200)
            }
            MotorNormMode::Range0100 => min + value.clamp(0, 100) * (max - min) / 100,
            MotorNormMode::Degrees => (value * 4095 / 360) + (min + max) / 2,
        }
    }

    fn motor(&self, motor: MotorName) -> MotorConfig {
        self.motors[motor.id() as usize - 1]
    }
}

fn little_endian(bytes: &[u8]) -> u32 {
    bytes.iter().enumerate().fold(0u32, |value, (index, byte)| {
        value | (*byte as u32) << (index * 8)
    })
}

fn little_endian_bytes(value: u32, length: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(length);
    for index in 0..length {
        bytes.push((value >> (index * 8)) as u8);
    }
    bytes
}

fn decode_sign_magnitude(value: u32, sign_bit: u8) -> i32 {
    let magnitude = value & ((1u32 << sign_bit) - 1);
    if value & (1u32 << sign_bit) != 0 {
        -(magnitude as i32)
    } else {
        magnitude as i32
    }
}

fn encode_sign_magnitude(value: i32, sign_bit: u8) -> Result<u32, ScservoError> {
    let magnitude = value.unsigned_abs();
    if magnitude >= (1u32 << sign_bit) {
        return Err(ScservoError::InvalidArgument);
    }
    Ok(magnitude | if value < 0 { 1u32 << sign_bit } else { 0 })
}

/// 默认应用：连接后只做探测和读取，故障时退出并由 EL1 回收资源。
pub fn run(
    transport: &mut CdcAcmTransport,
    handler: &EventHandler,
    intid: u32,
    notification: &crate::notification::Notification,
) -> ! {
    let mut bus = FeetechMotorsBus::so101(transport);
    let result = crate::usb_app::block_on_usb(startup(&mut bus), handler, intid, notification);
    match result {
        Ok(()) => {
            #[cfg(feature = "scservo-move")]
            {
                if let Err(error) = crate::usb_app::block_on_usb(
                    move_to_neutral(&mut bus),
                    handler,
                    intid,
                    notification,
                ) {
                    // 运动失败也尽力释放六个舵机，避免错误退出后继续保持扭矩。
                    let _ = crate::usb_app::block_on_usb(
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
            {
                crate::runtime::puts(b"[libos] SCServo read-only startup complete\r\n");
            }
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
    // 读取实际保存的标定范围，让归一化位置使用硬件当前配置。
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
    // LeRobot 的标定流程把每个关节的中点作为安全初始姿态；前五个
    // 关节的归一化中点是 0，夹爪中点是 50%。
    let neutral = [Some(0), Some(0), Some(0), Some(0), Some(0), Some(50)];
    let mut current = bus.sync_read(ControlTable::PresentPosition, true).await?;

    bus.enable_torque(None).await?;
    crate::runtime::puts(b"[libos] moving to calibrated neutral pose\r\n");

    // 每轮最多移动 5 个归一化单位，并等待 100 ms，避免大幅跳转。
    let mut round = 0;
    let mut neutral_reached = false;
    while round < 40 {
        let mut command = [None; 6];
        let mut reached = true;
        for index in 0..6 {
            let present = current[index].ok_or(ScservoError::Timeout)?;
            let target = neutral[index].unwrap();
            let delta = target - present;
            // STS3215的机械死区和整数归一化可能让反馈稳定在目标附近3个单位。
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
