/* 第一方兼容语义的宿主测试；与真正 QEMU 收发验收分开，不冒充硬件验证。 */
#include "../../libos/src/drivers/linux_liteeth/adapter.c"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static unsigned live, maps, bound, received, allocation_calls, fail_at;
static int fail_map = -1;
static uint32_t csr[1024];
static uint8_t slots[8192], last_frame[2048];
void *exo_lx_alloc(size_t size) {
    allocation_calls++;
    if (fail_at == allocation_calls)
        return NULL;
    void *p = calloc(1, size);
    if (p)
        live++;
    return p;
}
void exo_lx_free(void *p, size_t size) {
    assert(p && live);
    live--;
    free(p);
}
void *exo_lx_map(uint64_t pa, uint64_t size, unsigned index) {
    if ((int)index == fail_map)
        return NULL;
    maps++;
    return index ? (void *)slots : (void *)csr;
}
int exo_lx_unmap(void *va, uint64_t size) {
    assert(maps);
    maps--;
    return 0;
}
int exo_lx_bind_irq(unsigned irq) {
    assert(!bound);
    bound = irq;
    return 0;
}
int exo_lx_unbind_irq(unsigned irq) {
    assert(bound == irq);
    bound = 0;
    return 0;
}
int exo_lx_receive(const u8 *p, size_t size) {
    received++;
    memcpy(last_frame, p, size);
    return 0;
}
void exo_lx_puts(const char *p, size_t n) {}
uint64_t exo_lx_ticks(void) { return 123456789; }
uint64_t exo_lx_frequency(void) { return 1000000; }
void exo_lx_fault(void) { abort(); }
int main(void) {
    struct lx_config c = {.mac_pa = 0xb000000,
                          .mac_size = 4096,
                          .buffer_pa = 0xb010000,
                          .buffer_size = 8192,
                          .irq = 42,
                          .rx_slots = 2,
                          .tx_slots = 2,
                          .slot_size = 2048,
                          .mac = {2, 1, 2, 3, 4, 5}};
    /* 两次托管分配和两个映射的失败路径必须释放已取得资源。 */
    for (unsigned i = 1; i <= 2; i++) {
        allocation_calls = 0;
        fail_at = i;
        assert(exo_liteeth_attach(&c) < 0);
        assert(!live && !maps && !bound);
    }
    fail_at = 0;
    for (int i = 0; i < 2; i++) {
        fail_map = i;
        assert(exo_liteeth_attach(&c) < 0);
        assert(!live && !maps && !bound);
    }
    fail_map = -1;
    assert(exo_liteeth_attach(&c) == 0);
    assert(exo_liteeth_open() == 0);
    assert(bound == 42);
    /* 宿主普通数组没有 W1C，手动置零；W1C/真实 IRQ 留给 QEMU 验收。 */
    csr[0x10 / 4] = 0;
    csr[0x30 / 4] = 0;
    uint8_t frame[60];
    for (unsigned i = 0; i < sizeof(frame); i++)
        frame[i] = (u8)i;
    unsigned baseline = live;
    csr[0x1c / 4] = 0;
    assert(exo_liteeth_send(frame, sizeof(frame)) == NETDEV_TX_BUSY);
    assert(live == baseline);
    csr[0x1c / 4] = 1;
    csr[0x30 / 4] = 1;
    exo_liteeth_handle_irq();
    assert(exo_liteeth_send(frame, sizeof(frame)) == 0);
    assert(live == baseline);
    assert(!memcmp(slots + 4096, frame, sizeof(frame)));
    /* 两个 TX slot 轮转，以及超长报文消费/统计语义。 */
    assert(exo_liteeth_send(frame, sizeof(frame)) == 0);
    assert(!memcmp(slots + 6144, frame, sizeof(frame)));
    uint8_t oversized[2049] = {0};
    assert(exo_liteeth_send(oversized, sizeof(oversized)) == 0);
    assert(live == baseline);
    memcpy(slots + 2048, frame, sizeof(frame));
    csr[0] = 1;
    csr[1] = sizeof(frame);
    csr[0x10 / 4] = 1;
    exo_liteeth_handle_irq();
    assert(received == 1 && !memcmp(last_frame, frame, sizeof(frame)));
    assert(live == baseline);
    csr[1] = 2049;
    exo_liteeth_handle_irq();
    struct rtnl_link_stats64 s = {0};
    assert(exo_liteeth_stats(&s) == 0);
    assert(s.rx_packets == 1 && s.tx_packets == 2 && s.rx_errors == 1 && s.tx_errors == 1);
    assert(exo_liteeth_close() == 0);
    assert(!bound);
    assert(exo_liteeth_open() == 0);
    assert(exo_liteeth_detach() == 0);
    assert(!live && !maps && !bound);
    assert(exo_liteeth_attach(&c) == 0);
    assert(exo_liteeth_detach() == 0);
    assert(!live && !maps && !bound);
    puts("PASS adapter allocation/map rollback, BUSY ownership, slot rotation, RX copy, stats, "
         "stop/reopen/remove/reprobe");
}
