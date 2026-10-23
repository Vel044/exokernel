/* 显式选择 -netdev ...,id=liteeth 才出现的实验设备；SPI 10 => INTID 42。 */
static void create_exo_liteeth(VirtMachineState *vms) {
    NetClientState *net = qemu_find_netdev("liteeth");
    if (!net) {
        return;
    }
    MachineState *ms = MACHINE(vms);
    DeviceState *dev = qdev_new("exo-liteeth");
    qdev_prop_set_netdev(dev, "netdev", net);
    sysbus_realize_and_unref(SYS_BUS_DEVICE(dev), &error_fatal);
    sysbus_mmio_map(SYS_BUS_DEVICE(dev), 0, 0x0b000000);
    sysbus_mmio_map(SYS_BUS_DEVICE(dev), 1, 0x0b010000);
    sysbus_connect_irq(SYS_BUS_DEVICE(dev), 0, qdev_get_gpio_in(vms->gic, 10));
    const char *node = "/ethernet@b000000";
    qemu_fdt_add_subnode(ms->fdt, node);
    qemu_fdt_setprop_string(ms->fdt, node, "compatible", "litex,liteeth");
    qemu_fdt_setprop_sized_cells(ms->fdt, node, "reg", 2, 0x0b000000, 2, 0x1000, 2, 0x0b010000, 2,
                                 0x2000);
    static const char names[] = "mac\0buffer";
    qemu_fdt_setprop(ms->fdt, node, "reg-names", names, sizeof(names));
    qemu_fdt_setprop_cells(ms->fdt, node, "interrupts", 0, 10, 4);
    qemu_fdt_setprop_cell(ms->fdt, node, "litex,rx-slots", 2);
    qemu_fdt_setprop_cell(ms->fdt, node, "litex,tx-slots", 2);
    qemu_fdt_setprop_cell(ms->fdt, node, "litex,slot-size", 2048);
    static const uint8_t mac[] = {0x02, 0x45, 0x58, 0x4f, 0x00, 0x01};
    qemu_fdt_setprop(ms->fdt, node, "local-mac-address", mac, 6);
}
