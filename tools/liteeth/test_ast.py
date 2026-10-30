#!/usr/bin/env python3
"""正例及故意改错的源码都重新经过解析器；不修改生产驱动，不只检查源码哈希。"""
import json
from pathlib import Path
import re
import unittest

from ast_frontends import ROOT, ParseError, build_syn, clang_environment, c_ast, rust_ast, digest
from ast_normalize import plain
from check_ast import (DEFAULT_C, DEFAULT_RUST, API_FILES, compare_models, write_result)

OUTPUT = ROOT / "target/liteeth-ast/tests"


def replace(text, old, new):
    if text.count(old) != 1:
        raise AssertionError(f"故障注入位置不唯一：{old!r}，出现 {text.count(old)} 次")
    return text.replace(old, new, 1)


def function_edit(text, name, old, new):
    return function_transform(text, name, lambda chunk: replace(chunk, old, new))


def function_transform(text, name, transform):
    start = re.search(r"^pub unsafe fn "+re.escape(name)+r"\b", text, re.M)
    following = re.search(r"^pub unsafe fn ", text[start.end():], re.M)
    end = start.end()+following.start() if following else len(text)
    return text[:start.start()] + transform(text[start.start():end]) + text[end:]


def passing_control(source):
    """明确修正当前三个差异的临时正例，仅写 target；不是把 C 的树复制成 Rust 的树。"""
    source = replace(source, "pub struct liteeth {", "#[repr(C)]\npub struct liteeth {")
    for old, new in [
        ("rx_slot as usize * (*r#priv).slot_size as usize", "(rx_slot as u32 * (*r#priv).slot_size) as usize"),
        ("(*r#priv).tx_slot as usize * (*r#priv).slot_size as usize", "((*r#priv).tx_slot * (*r#priv).slot_size) as usize"),
        ("(*r#priv).num_rx_slots as usize * (*r#priv).slot_size as usize", "((*r#priv).num_rx_slots * (*r#priv).slot_size) as usize"),
        ("(*netdev).irq,\n        liteeth_interrupt,", "(*netdev).irq as u32,\n        liteeth_interrupt,"),
        ("free_irq((*netdev).irq, netdev.cast());", "free_irq((*netdev).irq as u32, netdev.cast());"),
        ("(*netdev).irq = irq as u32;", "(*netdev).irq = irq;")]:
        source = replace(source, old, new)
    return source


class AstComparisonTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        OUTPUT.mkdir(parents=True, exist_ok=True)
        cls.binary = build_syn()
        cls.environment = clang_environment()
        cls.c_roots, cls.c_manifest = c_ast(DEFAULT_C, cls.environment, OUTPUT / "reference-c")
        cls.original = DEFAULT_RUST.read_text()
        cls.records = []
        cls.control = passing_control(cls.original)
        cls.netdevice_path = OUTPUT / "positive/netdevice.rs"
        cls.netdevice_path.parent.mkdir(parents=True, exist_ok=True)
        cls.netdevice_path.write_text(replace((ROOT / "libos/src/linux_api/netdevice.rs").read_text(),
                                              "pub irq: u32,", "pub irq: i32,"))
        cls.control_api = [p if p.name != "netdevice.rs" else cls.netdevice_path for p in API_FILES]

    def model(self, name, source, api=None, c_roots=None, c_source=DEFAULT_C):
        output = OUTPUT / name
        output.mkdir(parents=True, exist_ok=True)
        path = output / "liteeth.rs"
        path.write_text(source)
        data = rust_ast([path]+(api or self.control_api), output / "rust", binary=self.binary)
        report, c_tree, r_tree = compare_models(c_roots or self.c_roots, c_source, data, path)
        write_result(output, report, c_tree, r_tree)
        return report, c_tree, r_tree

    def test_01_actual_baseline_reports_existing_differences(self):
        report, _, _ = self.model("actual-baseline", self.original, api=API_FILES)
        self.assertFalse(report["passed"])
        self.assertEqual({f["category"] for f in report["findings"]}, {"function_ast", "layout"})
        self.assertEqual([f["name"] for f in report["functions"] if f["status"] == "unsupported"], [])
        self.assertEqual({f["name"] for f in report["functions"] if f["status"] == "different"},
                         {"liteeth_rx", "liteeth_open", "liteeth_stop", "liteeth_start_xmit", "liteeth_probe"})
        self.records.append(dict(name="当前源码真实差异", result="按预期报告，未假定基线通过"))

    def test_02_independent_positive_control_and_formatting(self):
        report, c_tree, r_tree = self.model("positive", self.control)
        self.assertTrue(report["passed"], json.dumps(report["findings"], ensure_ascii=False, indent=2))
        self.assertEqual(len(report["functions"]), 9)
        self.assertFalse(report["semantic_equivalence_proven"])
        formatted = "// 中文注释不会改变 AST。\n" + self.control
        formatted = replace(formatted, "LITEETH_WRITER_LENGTH: usize = 0x04;", "LITEETH_WRITER_LENGTH: usize = 4;")
        formatted = function_edit(formatted, "liteeth_open", "netif_start_queue(netdev);\n    0", "netif_start_queue(netdev);\n    return (0);")
        formatted = function_edit(formatted, "liteeth_rx", ".byte_add(LITEETH_WRITER_SLOT)", ".byte_add((LITEETH_WRITER_SLOT))")
        formatted_report, _, formatted_r = self.model("positive-formatting", formatted)
        self.assertTrue(formatted_report["passed"], formatted_report["findings"])
        self.assertEqual(plain(r_tree), plain(formatted_r))
        c_path = OUTPUT / "positive-c-formatting/litex_liteeth.c"
        c_path.parent.mkdir(parents=True, exist_ok=True)
        c_path.write_text("/* 中文注释会改变字节偏移，不改变 AST 含义。 */\n" +
                          replace(DEFAULT_C.read_text(), "#define LITEETH_WRITER_LENGTH     0x04", "#define LITEETH_WRITER_LENGTH     4"))
        roots, _ = c_ast(c_path, self.environment, c_path.parent / "c")
        c_report, formatted_c, _ = self.model("positive-c-formatting", self.control, c_roots=roots, c_source=c_path)
        self.assertTrue(c_report["passed"], c_report["findings"])
        self.assertEqual(plain(c_tree), plain(formatted_c))
        self.records.append(dict(name="独立 Rust 正例及注释/进制/括号/显式 return", result="9 个函数与全局登记规范化后相同"))

    def test_03_negative_source_mutations(self):
        def edit(name, old, new):
            return lambda text: function_edit(text, name, old, new)

        cases = [
            ("wrong-register-constant", lambda s: replace(s, "LITEETH_READER_START: usize = 0x18;", "LITEETH_READER_START: usize = 0x1C;"), "constant_value", None),
            ("unused-constant-type", lambda s: replace(s, "LITEETH_MDIO_R: usize", "LITEETH_MDIO_R: u16"), "constant_type_mapping", None),
            ("wrong-register-use", edit("liteeth_start_xmit", "base.byte_add(LITEETH_READER_START), 1", "base.byte_add(LITEETH_READER_READY), 1"), "function_ast", "liteeth_start_xmit"),
            ("reverse-length-comparison", edit("liteeth_rx", "len > 2048", "len < 2048"), "function_ast", "liteeth_rx"),
            ("wrong-boolean-operator", edit("liteeth_rx", "len == 0 || len > 2048", "len == 0 && len > 2048"), "function_ast", "liteeth_rx"),
            ("swapped-buffer-arguments", edit("liteeth_start_xmit", "memcpy_toio(txbuffer, (*skb).data, (*skb).len as usize);", "memcpy_toio((*skb).data.cast(), txbuffer.cast(), (*skb).len as usize);"), "function_ast", "liteeth_start_xmit"),
            ("wrong-copy-length", edit("liteeth_rx", "len as usize,", "(len - 1) as usize,"), "function_ast", "liteeth_rx"),
            ("changed-mmio-write-order", edit("liteeth_open", "litex_write8((*r#priv).base.byte_add(LITEETH_WRITER_EV_ENABLE), 1);\n    litex_write8((*r#priv).base.byte_add(LITEETH_READER_EV_ENABLE), 1);", "litex_write8((*r#priv).base.byte_add(LITEETH_READER_EV_ENABLE), 1);\n    litex_write8((*r#priv).base.byte_add(LITEETH_WRITER_EV_ENABLE), 1);"), "function_ast", "liteeth_open"),
            ("missing-free-irq", edit("liteeth_stop", "    free_irq((*netdev).irq as u32, netdev.cast());\n", ""), "function_ast", "liteeth_stop"),
            ("busy-becomes-success", edit("liteeth_start_xmit", "return NETDEV_TX_BUSY;", "return NETDEV_TX_OK;"), "function_ast", "liteeth_start_xmit"),
            ("wrong-irq-context-pointer", edit("liteeth_open", "        netdev.cast(),", "        r#priv.cast(),"), "function_ast", "liteeth_open"),
            ("wrong-irq-flags", edit("liteeth_open", "        liteeth_interrupt,\n        0,", "        liteeth_interrupt,\n        128,"), "function_ast", "liteeth_open"),
            ("missing-carrier-call", edit("liteeth_open", "    netif_carrier_on(netdev);\n", ""), "function_ast", "liteeth_open"),
            ("wrong-tail-return", edit("liteeth_open", "netif_start_queue(netdev);\n    0", "netif_start_queue(netdev);\n    1"), "function_ast", "liteeth_open"),
            ("missing-error-jump", edit("liteeth_rx", "if len == 0 || len > 2048 {\n            break 'rx_drop;", "if len == 0 || len > 2048 {"), "function_ast", "liteeth_rx"),
            ("extra-narrowing-cast", edit("liteeth_rx", "netdev_alloc_skb_ip_align(netdev, len as u32)", "netdev_alloc_skb_ip_align(netdev, len as u8 as u32)"), "function_ast", "liteeth_rx"),
            ("missing-explicit-integer-cast", edit("liteeth_rx", "netdev_alloc_skb_ip_align(netdev, len as u32)", "netdev_alloc_skb_ip_align(netdev, len)"), "unsupported", "liteeth_rx"),
            ("missing-pointer-offset-cast", edit("liteeth_probe", "((*r#priv).num_rx_slots * (*r#priv).slot_size) as usize", "(*r#priv).num_rx_slots * (*r#priv).slot_size"), "unsupported", "liteeth_probe"),
            ("wrong-default-slot-count", edit("liteeth_setup_slots", "(*r#priv).num_rx_slots = 2;", "(*r#priv).num_rx_slots = 3;"), "function_ast", "liteeth_setup_slots"),
            ("missing-rx-delivery", edit("liteeth_rx", "return netif_rx(skb);", "return 0;"), "function_ast", "liteeth_rx"),
            ("reversed-pending-test", edit("liteeth_interrupt", "let reg = litex_read8((*r#priv).base.byte_add(LITEETH_READER_EV_PENDING));\n    if reg != 0", "let reg = litex_read8((*r#priv).base.byte_add(LITEETH_READER_EV_PENDING));\n    if reg == 0"), "function_ast", "liteeth_interrupt"),
            ("wrong-callback-binding", lambda s: replace(s, "ndo_stop: liteeth_stop,", "ndo_stop: liteeth_open,"), "global_ast", None),
            ("wrong-device-compatible", lambda s: replace(s, 'compatible: "litex,liteeth",', 'compatible: "litex,other",'), "global_ast", None),
            ("wrong-module-metadata", lambda s: replace(s, 'MODULE_LICENSE!("GPL");', 'MODULE_LICENSE!("MIT");'), "global_ast", None),
            ("changed-struct-field-order", lambda s: replace(s, "pub tx_slot: u32,            // 下一发送槽\n    pub num_tx_slots: u32,       // TX 槽数", "pub num_tx_slots: u32,       // TX 槽数\n    pub tx_slot: u32,            // 下一发送槽"), "struct_fields", None),
            ("missing-c-layout", lambda s: replace(s, "#[repr(C)]\npub struct liteeth", "pub struct liteeth"), "layout", None),
            ("renamed-entry-parameter", lambda s: function_transform(s, "liteeth_stop", lambda chunk: re.sub(r"\bnetdev\b", "device", chunk)), "function_ast", "liteeth_stop"),
            ("unknown-wait-syntax", edit("liteeth_stop", "    netif_stop_queue(netdev);", "    netif_stop_queue(netdev).await;"), "unsupported", "liteeth_stop"),
            ("hidden-try-branch", edit("liteeth_open", "    if err != 0 {", "    let hidden = Some(err)?;\n    if err != 0 {"), "unsupported", "liteeth_open"),
            ("unreviewed-import-alias", lambda s: replace(s, "use core::{ffi::c_void, ptr};", "use core::{ffi::c_void, ptr};\nuse crate::linux_api::netdevice::netif_carrier_on as other_carrier;"), "source_boundary", None),
            ("hidden-statement-cfg", edit("liteeth_open", "    netif_carrier_on(netdev);", "    #[cfg(any())]\n    netif_carrier_on(netdev);"), "unsupported", "liteeth_open"),
            ("shadowed-dependency-name", edit("liteeth_open", "    netif_carrier_on(netdev);", "    let netif_carrier_on = netif_carrier_off;\n    netif_carrier_on(netdev);"), "unsupported", "liteeth_open"),
            ("hidden-register-cfg", lambda s: replace(s, "pub const LITEETH_READER_START:", "#[cfg(any())]\npub const LITEETH_READER_START:"), "source_boundary", None),
            ("hidden-file-cfg", lambda s: "#![cfg(any())]\n" + s, "source_boundary", None),
        ]
        for name, mutate, category, function in cases:
            with self.subTest(name=name):
                report, _, _ = self.model(name, mutate(self.control))
                matched = [f for f in report["findings"] if f["category"] == category and (function is None or f.get("function") == function)]
                self.assertFalse(report["passed"], name)
                self.assertTrue(matched, json.dumps(report["findings"], ensure_ascii=False, indent=2))
                # 不是通过源文件哈希拒绝：必须得到指定树差异/未知节点及对应位置。
                if category == "function_ast":
                    self.assertTrue(all(d["c_loc"] and d["rust_loc"] for d in matched[0]["differences"]))
                self.records.append(dict(name=name, result="detected", category=category,
                                         evidence="report.json 中的源码位置与树路径", output=str(OUTPUT / name)))

    def test_04_dependency_signature_and_type_are_checked(self):
        path = OUTPUT / "changed-dependency/io.rs"
        path.parent.mkdir(parents=True, exist_ok=True)
        text = (ROOT / "libos/src/linux_api/io.rs").read_text()
        # 参数名改变不会改变本次驱动的调用 spelling，但必须违反接口约束。
        text = function_transform(text, "litex_write8", lambda chunk: re.sub(r"\breg\b", "register", chunk))
        path.write_text(text)
        api = [p if p.name != "io.rs" else path for p in self.control_api]
        report, _, _ = self.model("changed-dependency", self.control, api=api)
        self.assertTrue(any(f["category"] == "dependency_signature" and f["name"] == "litex_write8" for f in report["findings"]))
        self.records.append(dict(name="dependency-parameter-name", result="detected", category="dependency_signature"))
        irq_path = OUTPUT / "changed-type-alias/interrupt.rs"
        irq_path.parent.mkdir(parents=True, exist_ok=True)
        irq_path.write_text(replace((ROOT / "libos/src/linux_api/interrupt.rs").read_text(),
                                    "pub type irqreturn_t = i32;", "pub type irqreturn_t = i64;"))
        api = [p if p.name != "interrupt.rs" else irq_path for p in self.control_api]
        report, _, _ = self.model("changed-type-alias", self.control, api=api)
        self.assertTrue(any(f["category"] == "dependency_type_alias" and f["name"] == "irqreturn_t" for f in report["findings"]))
        self.records.append(dict(name="dependency-return-alias-width", result="detected", category="dependency_type_alias"))
        module_path = OUTPUT / "missing-macro/module.rs"
        module_path.parent.mkdir(parents=True, exist_ok=True)
        module_path.write_text(replace((ROOT / "libos/src/linux_api/module.rs").read_text(),
                                       "macro_rules! MODULE_LICENSE {", "macro_rules! MODULE_LICENSE_RENAMED {"))
        api = [p if p.name != "module.rs" else module_path for p in self.control_api]
        report, _, _ = self.model("missing-macro", self.control, api=api)
        self.assertTrue(any(f["category"] == "missing_dependency_macro" and f["name"] == "MODULE_LICENSE" for f in report["findings"]))
        self.records.append(dict(name="missing-dependency-macro", result="detected", category="missing_dependency_macro"))
        netdevice_path = OUTPUT / "unreviewed-dependency/netdevice.rs"
        netdevice_path.parent.mkdir(parents=True, exist_ok=True)
        netdevice_path.write_text(self.netdevice_path.read_text()+
            "\npub unsafe fn unreviewed_carrier(dev: *mut net_device) { (*dev).carrier = true; }\n")
        api = [p if p.name != "netdevice.rs" else netdevice_path for p in self.control_api]
        source = function_edit(self.control, "liteeth_open", "netif_carrier_on(netdev);", "unreviewed_carrier(netdev);")
        report, _, _ = self.model("unreviewed-dependency", source, api=api)
        self.assertTrue(any(f["category"] == "unsupported" and f["function"] == "liteeth_open" for f in report["findings"]))
        self.records.append(dict(name="unreviewed-dependency", result="rejected", category="unsupported"))

    def test_05_real_c_ast_is_also_reparsed(self):
        path = OUTPUT / "changed-c/litex_liteeth.c"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(replace(DEFAULT_C.read_text(),
                                "litex_read32(priv->base + LITEETH_WRITER_LENGTH)",
                                "litex_read32(priv->base + LITEETH_WRITER_SLOT)"))
        roots, _ = c_ast(path, self.environment, path.parent / "c")
        report, _, _ = self.model("changed-c", self.control, c_roots=roots, c_source=path)
        self.assertTrue(any(f["category"] == "function_ast" and f["function"] == "liteeth_rx" for f in report["findings"]))
        self.records.append(dict(name="C-side-wrong-register", result="detected", category="function_ast"))
        path = OUTPUT / "unknown-c/litex_liteeth.c"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(replace(DEFAULT_C.read_text(), "netif_carrier_off(netdev);", "while (0) {}\n\tnetif_carrier_off(netdev);"))
        roots, _ = c_ast(path, self.environment, path.parent / "c")
        report, _, _ = self.model("unknown-c", self.control, c_roots=roots, c_source=path)
        self.assertTrue(any(f["category"] == "unsupported" and f["function"] == "liteeth_stop" for f in report["findings"]))
        self.records.append(dict(name="unsupported-C-node", result="rejected", category="unsupported"))
        path = OUTPUT / "unmapped-volatile/litex_liteeth.c"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(replace(DEFAULT_C.read_text(), "\tu8 reg;", "\tvolatile u8 reg;"))
        roots, _ = c_ast(path, self.environment, path.parent / "c")
        report, _, _ = self.model("unmapped-volatile", self.control, c_roots=roots, c_source=path)
        self.assertTrue(any(f["category"] == "unsupported" and f["function"] == "liteeth_interrupt" for f in report["findings"]))
        self.records.append(dict(name="unmapped-volatile-access", result="rejected", category="unsupported"))

    def test_06_invalid_rust_is_parser_error(self):
        path = OUTPUT / "invalid-rust/liteeth.rs"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(self.control + "\nfn ???\n")
        with self.assertRaises(ParseError) as error:
            rust_ast([path], path.parent / "rust", binary=self.binary)
        (path.parent / "error.txt").write_text(str(error.exception))
        self.records.append(dict(name="invalid-Rust-syntax", result="rejected", category="parser_error"))

    @classmethod
    def tearDownClass(cls):
        (OUTPUT / "summary.json").write_text(json.dumps(dict(cases=cls.records,
            inputs={str(p.relative_to(ROOT)) if p.is_relative_to(ROOT) else str(p): digest(p) for p in [DEFAULT_C, DEFAULT_RUST]+API_FILES},
            warning="正例在 target 中显式修正类型/布局；生产驱动未被测试脚本修改。反例均从源码重新解析，不把哈希变化当成树差异。"), ensure_ascii=False, indent=2)+"\n")


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(AstComparisonTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    path = OUTPUT / "summary.json"
    summary = json.loads(path.read_text()) if path.exists() else {}
    summary.update(passed=result.wasSuccessful(), test_groups=result.testsRun,
                   failures=len(result.failures), errors=len(result.errors),
                   detected_negative_cases=sum(c["result"] in {"detected", "rejected"} for c in summary.get("cases", [])))
    path.write_text(json.dumps(summary, ensure_ascii=False, indent=2)+"\n")
    raise SystemExit(0 if result.wasSuccessful() else 1)
