/* 第一方适配实现。原 Linux 文件保持不变，以下兼容服务只面向 LiteEth。
 * 限制：一个设备、一个所属 EL0 线程、线性 L2 帧；不实现 Linux 网络协议栈。 */
#include "compat.h"
static struct platform_device device;
static struct device_node node;
static irqreturn_t (*irq_handler)(int, void *);
static void *irq_cookie;
static bool active;
static int teardown_error;
static bool eq(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}
static void copy(void *dst, const void *src, size_t n) {
    u8 *d = dst;
    const u8 *s = src;
    while (n--)
        *d++ = *s++;
}
void lx_log(const char *fmt, ...) {
    /* 仅覆盖本驱动实际使用的 %d；格式不支持时保留原字符，不解引用任意参数。 */
    char out[256];
    size_t n = 0;
    va_list args;
    va_start(args, fmt);
    for (size_t i = 0; fmt[i] && n < sizeof(out) - 1; i++) {
        if (fmt[i] == '%' && fmt[i + 1] == 'd') {
            int v = va_arg(args, int);
            unsigned x = (unsigned)v;
            if (v < 0) {
                out[n++] = '-';
                x = 0u - x;
            }
            char digits[10];
            unsigned k = 0;
            do {
                digits[k++] = '0' + x % 10;
                x /= 10;
            } while (x && k < 10);
            while (k && n < sizeof(out) - 1)
                out[n++] = digits[--k];
            i++;
        } else
            out[n++] = fmt[i];
    }
    va_end(args);
    exo_lx_puts(out, n);
}
int net_ratelimit(void) {
    static uint64_t start;
    static unsigned count;
    uint64_t now = exo_lx_ticks(), freq = exo_lx_frequency();
    if (now - start >= freq) {
        start = now;
        count = 0;
    }
    return count++ < 10;
}
static void *managed(struct device *d, size_t size) {
    struct platform_device *p = d->owner;
    if (p->allocation_count >= 4)
        return NULL;
    void *ptr = exo_lx_alloc(size);
    if (!ptr)
        return NULL;
    unsigned i = p->allocation_count++;
    p->allocations[i] = ptr;
    p->sizes[i] = size;
    return ptr;
}
struct net_device *devm_alloc_etherdev(struct device *d, size_t priv_size) {
    struct net_device *n = managed(d, sizeof(*n) + priv_size);
    if (!n)
        return NULL;
    n->priv = (u8 *)n + sizeof(*n);
    n->name = "eth0";
    n->stopped = true;
    return n;
}
void *lx_devm_stats(struct device *d, size_t size) { return managed(d, size); }
void *devm_platform_ioremap_resource_byname(struct platform_device *p, const char *name) {
    unsigned index;
    if (eq(name, "mac"))
        index = 0;
    else if (eq(name, "buffer"))
        index = 1;
    else
        return ERR_PTR(-EINVAL);
    uint64_t pa = index ? p->buffer_pa : p->mac_pa, size = index ? p->buffer_size : p->mac_size;
    void *va = exo_lx_map(pa, size, index);
    if (!va)
        return ERR_PTR(-ENOMEM);
    p->mappings[index] = va;
    p->mapping_sizes[index] = size;
    return va;
}
int of_property_read_u32(struct device_node *np, const char *key, u32 *value) {
    if (eq(key, "litex,rx-slots"))
        *value = np->rx_slots;
    else if (eq(key, "litex,tx-slots"))
        *value = np->tx_slots;
    else if (eq(key, "litex,slot-size"))
        *value = np->slot_size;
    else
        return -EINVAL;
    return 0;
}
int of_get_ethdev_address(struct device_node *np, struct net_device *n) {
    unsigned bits = 0;
    for (unsigned i = 0; i < 6; i++)
        bits |= np->mac[i];
    if (!bits || (np->mac[0] & 1))
        return -EINVAL;
    copy(n->mac, np->mac, 6);
    return 0;
}
void eth_hw_addr_random(struct net_device *n) {
    uint64_t seed = exo_lx_ticks();
    for (unsigned i = 0; i < 6; i++)
        n->mac[i] = (u8)(seed >> (i * 8));
    n->mac[0] = (n->mac[0] & 0xfe) | 2;
}
int register_netdev(struct net_device *n) {
    if (n->registered)
        return -EBUSY;
    n->registered = true;
    return 0;
}
void unregister_netdev(struct net_device *n) {
    if (n->opened) {
        n->netdev_ops->ndo_stop(n);
        n->opened = false;
    }
    n->registered = false;
}
int request_irq(unsigned irq, irqreturn_t (*handler)(int, void *), unsigned long flags,
                const char *name, void *cookie) {
    /* 这是保留原 C 驱动时使用的窄兼容接口，不是 Linux IRQ 核心的完整移植。
     * 原 liteeth_open 传入 liteeth_interrupt 与 netdev；这里只支持单设备、flags=0，
     * 不支持共享 IRQ、线程化 handler 或任意 Linux IRQ domain 的逻辑号转换。 */
    if (irq_handler || flags || irq != device.irq || !handler)
        return -EINVAL;
    /* 函数指针与 cookie 只保存在 EL0；EL1 不接收或执行它们。
     * exo_lx_bind_irq → Notification::bind_irq → SYS_IRQ_BIND，绑定到预建通知。
     * 先保存回调，再开放 GIC；本任务尚未 wait，不会并发进入驱动。 */
    irq_handler = handler;
    irq_cookie = cookie;
    if (exo_lx_bind_irq(irq)) {
        irq_handler = NULL;
        irq_cookie = NULL;
        return -EIO;
    }
    return 0;
}
void free_irq(unsigned irq, void *cookie) {
    if (cookie != irq_cookie || irq != device.irq) {
        teardown_error = -EINVAL;
        return;
    }
    /* Linux free_irq 还会等待已经执行的 IRQ 处理完成。本兼容层由同一个 EL0
     * 线程串行 wait/调用回调/stop，进入这里时不可能同时有 C IRQ 回调正在执行。
     * exo_lx_unbind_irq → runtime::irq_unbind → SYS_IRQ_UNBIND。
     * 先在 EL1 撤销路由，再清除回调；失败则禁止回收可能仍被使用的状态。 */
    if (exo_lx_unbind_irq(irq)) {
        teardown_error = -EIO;
        return;
    }
    irq_handler = NULL;
    irq_cookie = NULL;
}
void memcpy_fromio(void *dst, const volatile void *src, size_t len) {
    /* 设备给出的 slot/len 仍不可信；仅允许访问 DTB 中 RX slot 的范围。 */
    uintptr_t p = (uintptr_t)src, base = (uintptr_t)device.mappings[1];
    size_t span = (size_t)node.rx_slots * node.slot_size;
    if (p < base || p - base > span || len > span - (p - base) ||
        len > node.slot_size - (p - base) % node.slot_size) {
        exo_lx_fault();
        return;
    }
    u8 *d = dst;
    const volatile u8 *s = src;
    for (size_t i = 0; i < len; i++)
        d[i] = s[i];
    lx_io_barrier();
}
void memcpy_toio(volatile void *dst, const void *src, size_t len) {
    uintptr_t p = (uintptr_t)dst,
              base = (uintptr_t)device.mappings[1] + (size_t)node.rx_slots * node.slot_size;
    size_t span = (size_t)node.tx_slots * node.slot_size;
    if (p < base || p - base > span || len > span - (p - base) ||
        len > node.slot_size - (p - base) % node.slot_size) {
        exo_lx_fault();
        return;
    }
    volatile u8 *d = dst;
    const u8 *s = src;
    for (size_t i = 0; i < len; i++)
        d[i] = s[i];
    lx_io_barrier();
}

struct sk_buff *netdev_alloc_skb_ip_align(struct net_device *n, size_t size) {
    if (!size || size > 65536)
        return NULL;
    struct sk_buff *s = exo_lx_alloc(sizeof(*s));
    if (!s)
        return NULL;
    s->head = exo_lx_alloc(size);
    if (!s->head) {
        exo_lx_free(s, sizeof(*s));
        return NULL;
    }
    s->data = s->head;
    s->capacity = size;
    return s;
}

u8 *skb_put(struct sk_buff *s, size_t size) {
    if (size > s->capacity - s->len) {
        exo_lx_fault();
        return NULL;
    }
    u8 *p = s->data + s->len;
    s->len += size;
    return p;
}
uint16_t eth_type_trans(struct sk_buff *s, struct net_device *n) {
    if (s->len < 14)
        return 0; /* 原驱动未拒绝短帧；交付时将其明确丢弃。 */
    uint16_t p = (uint16_t)s->data[12] | ((uint16_t)s->data[13] << 8);
    s->data += 14;
    s->len -= 14;
    return p;
}
void dev_kfree_skb_any(struct sk_buff *s) {
    exo_lx_free(s->head, s->capacity);
    exo_lx_free(s, sizeof(*s));
}
int netif_rx(struct sk_buff *s) {
    size_t header = (size_t)(s->data - s->head);
    int ret = header == 14 ? exo_lx_receive(s->head, s->len + header) : NET_RX_DROP;
    /* netif_rx 消费 skb，即便上层拒绝，原驱动也不能再次释放。 */
    dev_kfree_skb_any(s);
    return ret;
}
#include "../../../vendor/liteeth-linux/litex_liteeth.c"

/* FFI 配置不包含虚拟指针，PA 与 GIC INTID 来自用户态 DTB 发现。 */
struct lx_config {
    uint64_t mac_pa, mac_size, buffer_pa, buffer_size;
    u32 irq, rx_slots, tx_slots, slot_size;
    u8 mac[6];
};
static void cleanup(void) {
    for (unsigned i = 0; i < 2; i++)
        if (device.mappings[i]) {
            if (exo_lx_unmap(device.mappings[i], device.mapping_sizes[i])) {
                exo_lx_fault();
                return;
            }
            device.mappings[i] = NULL;
        }
    while (device.allocation_count) {
        unsigned i = --device.allocation_count;
        exo_lx_free(device.allocations[i], device.sizes[i]);
    }
}
int exo_liteeth_attach(const struct lx_config *c) {
    if (active || !c || !c->rx_slots || !c->tx_slots || !c->slot_size || c->slot_size > 2048 ||
        c->rx_slots > 256 || c->tx_slots > 256 || c->mac_size < 0x44 ||
        ((uint64_t)c->rx_slots + c->tx_slots) * c->slot_size > c->buffer_size)
        return -EINVAL;
    device = (struct platform_device){0};
    node = (struct device_node){
        .rx_slots = c->rx_slots, .tx_slots = c->tx_slots, .slot_size = c->slot_size};
    copy(node.mac, c->mac, 6);
    device.dev.of_node = &node;
    device.dev.owner = &device;
    device.mac_pa = c->mac_pa;
    device.mac_size = c->mac_size;
    device.buffer_pa = c->buffer_pa;
    device.buffer_size = c->buffer_size;
    device.irq = c->irq;
    teardown_error = 0;
    int ret = lx_driver->probe(&device);
    if (ret) {
        cleanup();
        return ret;
    }
    active = true;
    return 0;
}
int exo_liteeth_open(void) {
    if (!active)
        return -EINVAL;
    struct net_device *n = device.drvdata;
    if (n->opened)
        return -EBUSY;
    int ret = n->netdev_ops->ndo_open(n);
    if (!ret)
        n->opened = true;
    return ret;
}
int exo_liteeth_send(const u8 *bytes, size_t len) {
    if (!active || !bytes || len < 14)
        return -EINVAL;
    struct net_device *n = device.drvdata;
    if (!n->opened)
        return -EINVAL;
    if (n->stopped)
        return NETDEV_TX_BUSY;
    struct sk_buff *s = netdev_alloc_skb_ip_align(n, len);
    if (!s)
        return -ENOMEM;
    copy(skb_put(s, len), bytes, len);
    int ret = n->netdev_ops->ndo_start_xmit(s, n);
    /* BUSY 表示 Linux 驱动没有消费；本封装仍拥有临时报文并负责回收。调用者保留原帧用于重试。 */
    if (ret == NETDEV_TX_BUSY)
        dev_kfree_skb_any(s);
    return ret;
}
int exo_liteeth_handle_irq(void) {
    /* Rust Driver::wait_irq 收到通知后才调用这里，在 EL0 执行保存的回调；
     * 回调清完设备 pending 返回后，Rust 包装层再 SYS_IRQ_ACK 重新开放 GIC SPI。 */
    return irq_handler ? irq_handler(device.irq, irq_cookie) : -EINVAL;
}
int exo_liteeth_stats(struct rtnl_link_stats64 *s) {
    if (!active || !s)
        return -EINVAL;
    ((struct net_device *)device.drvdata)->netdev_ops->ndo_get_stats64(device.drvdata, s);
    return 0;
}
int exo_liteeth_close(void) {
    if (!active)
        return -EINVAL;
    struct net_device *n = device.drvdata;
    if (n->opened) {
        n->netdev_ops->ndo_stop(n);
        if (teardown_error)
            return teardown_error;
        n->opened = false;
    }
    return 0;
}
int exo_liteeth_detach(void) {
    if (!active)
        return -EINVAL;
    int ret = exo_liteeth_close();
    if (ret)
        return ret;
    lx_driver->remove_new(&device);
    cleanup();
    active = false;
    return 0;
}
