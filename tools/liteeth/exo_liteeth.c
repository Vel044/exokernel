/* SPDX-License-Identifier: GPL-2.0-or-later
 * LiteEth 软件 MAC 的 QEMU 测试设备，非完整 FPGA/PHY 时序模型。
 * CSR 与 slot 布局对照 Linux litex_liteeth.c；帧通过 QEMU NIC backend 收发。
 * 固定 2 RX + 2 TX slot，长度不含 FCS。只用于移植验收，不宣称板级等价。 */
#include "qemu/osdep.h"
#include "hw/irq.h"
#include "hw/qdev-properties.h"
#include "hw/sysbus.h"
#include "net/net.h"
#include "qapi/error.h"
#include "qemu/module.h"
#include "qemu/timer.h"
#define TYPE_EXO_LITEETH "exo-liteeth"
OBJECT_DECLARE_SIMPLE_TYPE(ExoLiteEth, EXO_LITEETH)
struct ExoLiteEth {
    SysBusDevice parent_obj;
    MemoryRegion csr, buffer;
    qemu_irq irq;
    NICConf conf;
    bool trace;
    NICState *nic;
    QEMUTimer *tx_timer;
    uint32_t rx_slot, next_slot, rx_len, rx_pending, rx_enable;
    uint32_t tx_slot, tx_len, tx_pending, tx_enable, ready;
};
static void update(ExoLiteEth *s) {
    qemu_set_irq(s->irq, (s->rx_pending && s->rx_enable) || (s->tx_pending && s->tx_enable));
}
static void tx_done(void *opaque) {
    ExoLiteEth *s = opaque;
    s->ready = 1;
    s->tx_pending = 1;
    update(s);
}
static uint64_t csr_read(void *opaque, hwaddr addr, unsigned size) {
    ExoLiteEth *s = opaque;
    switch (addr) {
    case 0x00:
        return s->rx_slot;
    case 0x04:
        return s->rx_len;
    case 0x0c:
    case 0x10:
        return s->rx_pending;
    case 0x14:
        return s->rx_enable;
    case 0x1c:
        return s->ready;
    case 0x24:
        return s->tx_slot;
    case 0x28:
        return s->tx_len;
    case 0x2c:
    case 0x30:
        return s->tx_pending;
    case 0x34:
        return s->tx_enable;
    default:
        return 0;
    }
}
static void csr_write(void *opaque, hwaddr addr, uint64_t value, unsigned size) {
    ExoLiteEth *s = opaque;
    if (s->trace) { fprintf(stderr,"LITEETH_TRACE WRITE %02x %u\n",(unsigned)addr,(unsigned)value); }
    switch (addr) {
    case 0x10:
        s->rx_pending &= ~(value & 1);
        update(s);
        qemu_flush_queued_packets(qemu_get_queue(s->nic));
        return;
    case 0x14:
        s->rx_enable = value & 1;
        break;
    case 0x24:
        s->tx_slot = value & 0xff;
        break;
    case 0x28:
        s->tx_len = value & 0xffff;
        break;
    case 0x30:
        s->tx_pending &= ~(value & 1);
        break;
    case 0x34:
        s->tx_enable = value & 1;
        break;
    case 0x18:
        if ((value & 1) && s->ready && s->tx_slot < 2 && s->tx_len >= 14 && s->tx_len <= 2048) {
            uint8_t *ram = memory_region_get_ram_ptr(&s->buffer);
            if (s->trace) { fprintf(stderr,"LITEETH_TRACE TX %u %u\n",s->tx_slot,s->tx_len); }
            s->ready = 0;
            qemu_send_packet(qemu_get_queue(s->nic), ram + 4096 + s->tx_slot * 2048, s->tx_len);
            /* 保留异步忙窗口，以验证原驱动的 NETDEV_TX_BUSY/IRQ 唤醒。 */
            timer_mod(s->tx_timer, qemu_clock_get_ns(QEMU_CLOCK_VIRTUAL) + 1000000);
        }
        break;
    }
    update(s);
}
static const MemoryRegionOps csr_ops = {
    .read = csr_read,
    .write = csr_write,
    .endianness = DEVICE_LITTLE_ENDIAN,
    .valid.min_access_size = 4,
    .valid.max_access_size = 4,
    .impl.min_access_size = 4,
    .impl.max_access_size = 4,
};
static bool can_receive(NetClientState *nc) {
    ExoLiteEth *s = qemu_get_nic_opaque(nc);
    return !s->rx_pending && s->rx_enable;
}
static ssize_t receive(NetClientState *nc, const uint8_t *bytes, size_t size) {
    ExoLiteEth *s = qemu_get_nic_opaque(nc);
    if (!can_receive(nc))
        return 0;
    if (size < 14 || size > 2048)
        return size;
    uint8_t *ram = memory_region_get_ram_ptr(&s->buffer);
    s->rx_slot = s->next_slot;
    s->next_slot ^= 1;
    s->rx_len = size;
    memcpy(ram + s->rx_slot * 2048, bytes, size);
    if (s->trace) { fprintf(stderr,"LITEETH_TRACE RX %u %zu\n",s->rx_slot,size); }
    s->rx_pending = 1;
    update(s);
    return size;
}
static NetClientInfo net_info = {.type = NET_CLIENT_DRIVER_NIC,
                                 .size = sizeof(NICState),
                                 .can_receive = can_receive,
                                 .receive = receive};
static void realize(DeviceState *dev, Error **errp) {
    ExoLiteEth *s = EXO_LITEETH(dev);
    memory_region_init_io(&s->csr, OBJECT(s), &csr_ops, s, "liteeth.csr", 0x1000);
    memory_region_init_ram(&s->buffer, OBJECT(s), "liteeth.buffer", 8192, &error_fatal);
    sysbus_init_mmio(SYS_BUS_DEVICE(s), &s->csr);
    sysbus_init_mmio(SYS_BUS_DEVICE(s), &s->buffer);
    sysbus_init_irq(SYS_BUS_DEVICE(s), &s->irq);
    s->ready = 1;
    s->tx_timer = timer_new_ns(QEMU_CLOCK_VIRTUAL, tx_done, s);
    qemu_macaddr_default_if_unset(&s->conf.macaddr);
    s->nic = qemu_new_nic(&net_info, &s->conf, object_get_typename(OBJECT(dev)), dev->id,
                          &dev->mem_reentrancy_guard, s);
    qemu_format_nic_info_str(qemu_get_queue(s->nic), s->conf.macaddr.a);
}
static const Property props[] = {DEFINE_NIC_PROPERTIES(ExoLiteEth, conf), DEFINE_PROP_BOOL("trace",ExoLiteEth,trace,false)};
static void class_init(ObjectClass *klass, const void *data) {
    DeviceClass *dc = DEVICE_CLASS(klass);
    dc->realize = realize;
    dc->user_creatable = false;
    /* 模型未序列化 pending/timer；测试限定为冷启动实例，不执行迁移。 */
    dc->desc = "LiteEth port test fixture (no migration)";
    device_class_set_props(dc, props);
}
static const TypeInfo info = {.name = TYPE_EXO_LITEETH,
                              .parent = TYPE_SYS_BUS_DEVICE,
                              .instance_size = sizeof(ExoLiteEth),
                              .class_init = class_init};
static void register_types(void) { type_register_static(&info); }
type_init(register_types)
