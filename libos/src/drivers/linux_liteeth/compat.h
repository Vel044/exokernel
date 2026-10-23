/* 第一方 Linux API 窄兼容层：只承接 LiteEth 的线性报文、单实例和单线程语义。
 * 所有状态由同一个 EL0 驱动任务持有；IRQ 回调与 TX/open/stop 不并发执行。
 * 这里没有 POSIX syscall 或 Linux 内核结构的二进制兼容承诺。 */
#ifndef EXO_LITEETH_COMPAT_H
#define EXO_LITEETH_COMPAT_H
#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
typedef uint8_t u8;
typedef uint32_t u32;
typedef int irqreturn_t;
typedef int netdev_tx_t;
#define __iomem
#define IRQ_HANDLED 1
#define NET_RX_DROP 1
#define NETDEV_TX_OK 0
#define NETDEV_TX_BUSY 16
#define ENOMEM 12
#define EINVAL 22
#define EBUSY 16
#define ENXIO 6
#define EIO 5
#define unlikely(x) __builtin_expect(!!(x), 0)
#define IS_ERR(p) ((uintptr_t)(p) >= (uintptr_t)-4095)
#define PTR_ERR(p) ((intptr_t)(p))
#define ERR_PTR(e) ((void *)(intptr_t)(e))

/* Rust 桥：只把已校验的资源映射为 Device 内存，所有指针均为 CPU VA。 */
extern void *exo_lx_alloc(size_t size);
extern void exo_lx_free(void *ptr, size_t size);
extern void *exo_lx_map(uint64_t pa, uint64_t size, unsigned index);
extern int exo_lx_unmap(void *va, uint64_t size);
extern int exo_lx_bind_irq(unsigned irq);
extern int exo_lx_unbind_irq(unsigned irq);
extern int exo_lx_receive(const u8 *frame, size_t length);
extern void exo_lx_puts(const char *bytes, size_t length);
extern uint64_t exo_lx_ticks(void);
extern uint64_t exo_lx_frequency(void);
extern void exo_lx_fault(void);

struct device_node {
    u32 rx_slots, tx_slots, slot_size;
    u8 mac[6];
};
struct device {
    /* DTB 属性由 EL0 解析，托管资源由 adapter.c 在失败/解绑时回收。 */
    struct device_node *of_node;
    void *owner;
};
struct pcpu_sw_netstats {
    uint64_t rx_packets, rx_bytes, tx_packets, tx_bytes;
};
struct rtnl_link_stats64 {
    uint64_t rx_packets, tx_packets, rx_bytes, tx_bytes;
    uint64_t rx_errors, tx_errors, rx_dropped, tx_dropped;
};
struct sk_buff {
    /* head 拥有完整 L2 帧；data/len 可经 eth_type_trans 前移，不改变 head 所有权。 */
    u8 *head, *data;
    size_t len, capacity;
    uint16_t protocol;
};
struct net_device;
struct net_device_ops {
    int (*ndo_open)(struct net_device *);
    int (*ndo_stop)(struct net_device *);
    void (*ndo_get_stats64)(struct net_device *, struct rtnl_link_stats64 *);
    netdev_tx_t (*ndo_start_xmit)(struct sk_buff *, struct net_device *);
};
struct net_device {
    const struct net_device_ops *netdev_ops;
    struct pcpu_sw_netstats *tstats;
    struct rtnl_link_stats64 stats;
    void *priv;
    struct device *parent;
    int irq;
    const char *name;
    u8 mac[6];
    bool stopped, carrier, registered, opened;
};
struct platform_device {
    struct device dev;
    void *drvdata;
    uint64_t mac_pa, mac_size, buffer_pa, buffer_size;
    unsigned irq;
    void *allocations[4];
    size_t sizes[4];
    unsigned allocation_count;
    void *mappings[2];
    uint64_t mapping_sizes[2];
};
struct of_device_id {
    const char *compatible;
};
struct platform_driver {
    int (*probe)(struct platform_device *);
    void (*remove_new)(struct platform_device *);
    struct {
        const char *name;
        const struct of_device_id *of_match_table;
    } driver;
};
#define SET_NETDEV_DEV(n, d) ((n)->parent = (d))
#define MODULE_DEVICE_TABLE(kind, table)
#define MODULE_AUTHOR(value) static const char lx_author[] __attribute__((used)) = value
#define MODULE_DESCRIPTION(value) static const char lx_description[] __attribute__((used)) = value
#define MODULE_LICENSE(value) static const char lx_license[] __attribute__((used)) = value
#define module_platform_driver(value) static struct platform_driver *lx_driver = &(value)

void lx_log(const char *format, ...);
#define dev_dbg(d, f, ...) lx_log(f, ##__VA_ARGS__)
#define dev_err(d, f, ...) lx_log(f, ##__VA_ARGS__)
#define netdev_err(d, f, ...) lx_log(f, ##__VA_ARGS__)
#define netdev_info(d, f, ...) lx_log(f, ##__VA_ARGS__)
int net_ratelimit(void);
struct net_device *devm_alloc_etherdev(struct device *, size_t);
void *lx_devm_stats(struct device *, size_t);
#define devm_netdev_alloc_pcpu_stats(d, t) lx_devm_stats(d, sizeof(t))
void *devm_platform_ioremap_resource_byname(struct platform_device *, const char *);
static inline void *netdev_priv(struct net_device *n) { return n->priv; }
static inline void platform_set_drvdata(struct platform_device *p, void *v) { p->drvdata = v; }
static inline void *platform_get_drvdata(struct platform_device *p) { return p->drvdata; }
static inline int platform_get_irq(struct platform_device *p, unsigned index) {
    return index ? -ENXIO : (int)p->irq;
}
int of_property_read_u32(struct device_node *, const char *, u32 *);
int of_get_ethdev_address(struct device_node *, struct net_device *);
void eth_hw_addr_random(struct net_device *);
int register_netdev(struct net_device *);
void unregister_netdev(struct net_device *);
int request_irq(unsigned, irqreturn_t (*)(int, void *), unsigned long, const char *, void *);
void free_irq(unsigned, void *);

/* LiteX CSR32：低 8/16 位有效值仍使用 32 位 volatile 事务。
 * AArch64 的 dmb osh 保证设备缓冲复制、寄存器提交和事件确认的观察顺序。 */
static inline void lx_io_barrier(void) {
#ifdef __aarch64__
    __asm__ volatile("dmb osh" ::: "memory");
#else
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
#endif
}
static inline u32 litex_read32(const volatile void *p) {
    u32 v = *(const volatile u32 *)p;
    lx_io_barrier();
    return v;
}
static inline u8 litex_read8(const volatile void *p) { return (u8)litex_read32(p); }
static inline void litex_write8(volatile void *p, u8 v) {
    lx_io_barrier();
    *(volatile u32 *)p = v;
    lx_io_barrier();
}
static inline void litex_write16(volatile void *p, uint16_t v) {
    lx_io_barrier();
    *(volatile u32 *)p = v;
    lx_io_barrier();
}
void memcpy_fromio(void *, const volatile void *, size_t);
void memcpy_toio(volatile void *, const void *, size_t);
struct sk_buff *netdev_alloc_skb_ip_align(struct net_device *, size_t);
u8 *skb_put(struct sk_buff *, size_t);
uint16_t eth_type_trans(struct sk_buff *, struct net_device *);
int netif_rx(struct sk_buff *);
void dev_kfree_skb_any(struct sk_buff *);
static inline bool netif_queue_stopped(struct net_device *n) { return n->stopped; }
static inline void netif_start_queue(struct net_device *n) { n->stopped = false; }
static inline void netif_stop_queue(struct net_device *n) { n->stopped = true; }
static inline void netif_wake_queue(struct net_device *n) { n->stopped = false; }
static inline void netif_carrier_on(struct net_device *n) { n->carrier = true; }
static inline void netif_carrier_off(struct net_device *n) { n->carrier = false; }
static inline void dev_sw_netstats_rx_add(struct net_device *n, unsigned len) {
    n->tstats->rx_packets++;
    n->tstats->rx_bytes += len;
}
static inline void dev_sw_netstats_tx_add(struct net_device *n, unsigned count, unsigned len) {
    n->tstats->tx_packets += count;
    n->tstats->tx_bytes += len;
}
static inline void netdev_stats_to_stats64(struct rtnl_link_stats64 *to,
                                           const struct rtnl_link_stats64 *from) {
    *to = *from;
}
static inline void dev_fetch_sw_netstats(struct rtnl_link_stats64 *to,
                                         const struct pcpu_sw_netstats *s) {
    to->rx_packets += s->rx_packets;
    to->rx_bytes += s->rx_bytes;
    to->tx_packets += s->tx_packets;
    to->tx_bytes += s->tx_bytes;
}
#endif
