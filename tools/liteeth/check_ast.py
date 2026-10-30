#!/usr/bin/env python3
"""用 Clang/syn AST 比较 LiteEth；不同或未知均非通过，不宣称形式化语义证明。"""
import argparse
import json
from pathlib import Path
import re
import sys

from ast_frontends import (ROOT, DEFAULT_LINUX, ENTRIES, ParseError, clang_environment,
                           c_ast, rust_ast, digest)
from ast_normalize import (CONSTANTS, MACRO_INTERFACES, CNormalizer, RustNormalizer, Unsupported, ctype,
                           rtype, plain, walk)

DEFAULT_C = DEFAULT_LINUX / "drivers/net/ethernet/litex/litex_liteeth.c"
DEFAULT_RUST = ROOT / "libos/src/drivers/liteeth.rs"
SIGNATURES = ROOT / "tools/liteeth/linux_signatures.json"
API_FILES = sorted({ROOT / x["path"] for x in json.loads(SIGNATURES.read_text()).values()} |
                   {ROOT / "libos/src/linux_api/module.rs"})


def difference(left, right, path="$", left_loc=None, right_loc=None):
    """保留树上的具体路径和两侧源码位置；列表按顺序比较，不做排序抹平。"""
    if isinstance(left, dict):
        left_loc = left.get("loc", left_loc)
    if isinstance(right, dict):
        right_loc = right.get("loc", right_loc)
    if isinstance(left, dict) and isinstance(right, dict):
        result = []
        for key in sorted((left.keys() | right.keys()) - {"loc"}):
            if key not in left or key not in right:
                result.append(dict(path=path+"."+key, c=plain(left.get(key)), rust=plain(right.get(key)), c_loc=left_loc, rust_loc=right_loc))
            else:
                result += difference(left[key], right[key], path+"."+key, left_loc, right_loc)
        return result
    if isinstance(left, list) and isinstance(right, list):
        result = []
        if len(left) != len(right):
            result.append(dict(path=path+".length", c=len(left), rust=len(right), c_loc=left_loc, rust_loc=right_loc))
        for index, (a, b) in enumerate(zip(left, right)):
            result += difference(a, b, f"{path}[{index}]", left_loc, right_loc)
        return result
    return [] if left == right else [dict(path=path, c=left, rust=right, c_loc=left_loc, rust_loc=right_loc)]


def function_type(raw):
    # 读取 Clang 已解析的函数类型；当前驱动没有返回函数指针的依赖。
    left = raw.find("(")
    if left < 0 or not raw.endswith(")"):
        raise Unsupported("无法读取 Clang 函数类型：" + raw)
    ret = ctype(raw[:left].strip())
    args, start, depth = [], left+1, 0
    for index in range(left+1, len(raw)-1):
        char = raw[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif char == "," and depth == 0:
            args.append(raw[start:index].strip())
            start = index+1
    last = raw[start:-1].strip()
    if last and last != "void":
        args.append(last)
    return dict(parameters=[dict(name=f"argument_{i}", type=ctype(x)) for i, x in enumerate(args)], return_type=ret)


def c_functions(roots):
    functions = {}
    for node in walk(roots):
        declaration = node.get("referencedDecl", {}) if node.get("kind") == "DeclRefExpr" else node
        if declaration.get("kind") == "FunctionDecl":
            signature = function_type(declaration["type"]["qualType"])
            functions[declaration["name"]] = {"parameters": signature["parameters"], "return": signature["return_type"]}
    # 原始宏接口的展开是独立依赖，不能要求目标驱动也展开成 Linux 内部实现。
    functions["devm_alloc_etherdev"] = {"parameters": functions["devm_alloc_etherdev_mqs"]["parameters"][:2], "return": "ptr(net_device)"}
    functions["SET_NETDEV_DEV"] = {"parameters": [dict(name="net", type="ptr(net_device)"), dict(name="pdev", type="ptr(device)")], "return": "void"}
    for name in ["memcpy_fromio", "memcpy_toio"]:
        functions[name] = functions["__"+name]
    return functions


def dependencies(rust):
    expected = json.loads(SIGNATURES.read_text())
    result = []
    for name, definition in expected.items():
        item = rust.raw_functions.get(name)
        actual = None
        if item:
            signature = item["signature"]
            actual = dict(parameters=",".join(p["pattern"].get("name", "?")+":"+p["type"] for p in signature["parameters"]),
                          returns=signature["return"])
            special = [a for a in item["attributes"] if not a.startswith("# [doc")]
            if special or signature["async"] or signature["generics"] or signature["variadic"] or signature["abi"] or signature["unsafe"] != (name not in {"IS_ERR", "PTR_ERR", "unlikely", "net_ratelimit"}):
                actual["unreviewed_declaration"] = signature
        wanted = dict(parameters=definition["parameters"], returns=definition["return"])
        result.append(dict(name=name, path=definition["path"], equal=actual == wanted, expected=wanted, actual=actual,
                           loc=dict(file=rust.raw_function_paths[name], **item["loc"]) if item else None,
                           scope="真实 syn 签名与人工审核的 C/Rust 类型映射清单；未比较依赖函数体"))
    return result


def source_boundary(rust):
    """约束当前子集的名字来源，防止只比调用拼写而漏掉同名替换/遮蔽。"""
    errors = []
    for attribute in rust.file_attributes:
        normalized = re.sub(r"\s+", "", attribute)
        if not normalized.startswith("#![doc") and normalized != "#![allow(dead_code,non_camel_case_types,non_upper_case_globals)]":
            errors.append(dict(message="未审核的文件级属性", attribute=attribute, loc=dict(file=rust.source, line=1)))
    api_modules = {"error", "interrupt", "io", "module", "netdevice", "platform", "printk", "skbuff"}
    macro_names = {"dev_dbg", "dev_err", "devm_netdev_alloc_pcpu_stats", "module_platform_driver",
                   "netdev_err", "netdev_info", "MODULE_AUTHOR", "MODULE_DESCRIPTION", "MODULE_DEVICE_TABLE", "MODULE_LICENSE"}
    for item in rust.items:
        attributes = [a for a in item.get("attributes", []) if not a.startswith("# [doc")]
        if item["kind"] == "struct":
            attributes = [a for a in attributes if re.fullmatch(r"#\s*\[repr\s*\(\s*C\s*\)\s*\]", a) is None]
            for field in item["fields"]:
                if any(not a.startswith("# [doc") for a in field.get("attributes", [])):
                    errors.append(dict(message="字段属性尚未审核", loc=rust.loc(field)))
        if item["kind"] != "function" and attributes:
            runtime_export = item["kind"] == "use" and item["imports"] == [dict(path=["super", "liteeth_runtime", "Driver"], alias=None, glob=False)]
            if not (runtime_export and [re.sub(r"\s+", "", a) for a in attributes] == ['#[cfg(feature="linux-driver-api")]']):
                errors.append(dict(message="模块项属性尚未审核", loc=rust.loc(item), attributes=attributes))
        if item["kind"] == "use":
            for imported in item["imports"]:
                path = imported["path"]
                allowed = (not imported["alias"] and
                           ((imported["glob"] and len(path) == 3 and path[:2] == ["crate", "linux_api"] and path[2] in api_modules)
                            or (not imported["glob"] and len(path) == 2 and path[0] == "crate" and path[1] in macro_names)
                            or (not imported["glob"] and path in [["core", "ffi", "c_void"], ["core", "ptr"]])
                            or (not imported["glob"] and path == ["super", "liteeth_runtime", "Driver"])))
                if not allowed:
                    errors.append(dict(message="未登记的导入或别名；需要先检查名字绑定", import_item=imported, loc=rust.loc(item)))
        elif item["kind"] == "struct" and item["name"] != "liteeth":
            errors.append(dict(message="新增/遮蔽的驱动结构体尚未审核", loc=rust.loc(item)))
        elif item["kind"] not in {"function", "struct", "static", "constant", "item_macro"}:
            errors.append(dict(message="新增模块项尚未支持："+item["kind"], loc=rust.loc(item)))
    return errors


def compare_models(c_roots, c_source, r_data, r_source):
    rust = RustNormalizer(r_data["files"], r_source)
    clang = CNormalizer(c_roots, c_source, c_functions(c_roots))
    c_defs = {n["name"]: n for n in c_roots if n.get("kind") == "FunctionDecl"
              and any(x.get("kind") == "CompoundStmt" for x in n.get("inner", []))
              and n.get("name") not in {"liteeth_driver_init", "liteeth_driver_exit"}}
    r_defs = {n["signature"]["name"]: n for n in rust.items if n["kind"] == "function"}
    inventories = dict(c=list(c_defs), rust=list(r_defs))
    findings, normalized_c, normalized_r, functions = [], {}, {}, []
    for error in source_boundary(rust):
        findings.append(dict(category="source_boundary", **error))
    if set(c_defs) != set(ENTRIES) or set(r_defs) != set(ENTRIES):
        findings.append(dict(category="entry_inventory", c=inventories["c"], rust=inventories["rust"]))
    for name in ENTRIES:
        record = dict(name=name)
        for side, normalizer, definitions, outputs in [("c", clang, c_defs, normalized_c), ("rust", rust, r_defs, normalized_r)]:
            if name not in definitions:
                record[side+"_error"] = "缺少函数定义"
                continue
            try:
                outputs[name] = normalizer.function(definitions[name])
            except Unsupported as error:
                record[side+"_error"] = dict(message=str(error), loc=error.location)
        if "c_error" in record or "rust_error" in record:
            record["status"] = "unsupported"
            findings.append(dict(category="unsupported", function=name, **{k: v for k, v in record.items() if k.endswith("_error")}))
        else:
            diffs = difference(normalized_c[name], normalized_r[name], name)
            record.update(status="equal" if not diffs else "different", differences=diffs)
            if diffs:
                findings.append(dict(category="function_ast", function=name, differences=diffs))
        functions.append(record)
    # 常量比较由 Clang 求值与 syn 整数字面量构建，不只核对符号拼写。
    c_constants = {name: value for name, (value, _) in clang.constants.items()}
    r_constants = {name: value for name, (value, _) in rust.constants.items() if name.startswith("LITEETH_") or name in CONSTANTS}
    c_name = next(n for n in c_roots if n.get("kind") == "VarDecl" and n.get("name") == "liteeth_ast_drv_name")
    c_constants["DRV_NAME"] = json.loads(next(n["value"] for n in walk(c_name) if n.get("kind") == "StringLiteral"))
    r_name = next((n for n in rust.items if n["kind"] == "constant" and n["name"] == "DRV_NAME"), None)
    r_constants["DRV_NAME"] = r_name["expression"]["value"] if r_name and r_name["expression"]["kind"] == "string" else None
    for name in sorted(c_constants.keys() | r_constants.keys()):
        if c_constants.get(name) != r_constants.get(name):
            findings.append(dict(category="constant_value", name=name, c=c_constants.get(name), rust=r_constants.get(name)))
    for item in rust.items:
        if item["kind"] == "constant" and item["name"].startswith("LITEETH_") and item["type"] != "usize":
            findings.append(dict(category="constant_type_mapping", name=item["name"], expected="usize", actual=item["type"], loc=rust.loc(item)))
    # 字段定义与顺序参加约束 4；repr(Rust) 不能被当作 C 布局保证。
    c_struct = next((n for n in c_roots if n.get("kind") == "RecordDecl" and n.get("name") == "liteeth"), None)
    r_struct = next((n for n in rust.items if n["kind"] == "struct" and n["name"] == "liteeth"), None)
    c_fields = [dict(name=f["name"], type=ctype(f["type"]["qualType"])) for f in c_struct.get("inner", []) if f["kind"] == "FieldDecl"] if c_struct else []
    r_fields = [dict(name=f["name"], type=rtype(f["type"])) for f in r_struct["fields"]] if r_struct else []
    if c_struct is None or r_struct is None:
        findings.append(dict(category="struct_inventory", message="缺少 liteeth 结构体定义"))
    field_diffs = difference(c_fields, r_fields, "liteeth.fields")
    if field_diffs:
        findings.append(dict(category="struct_fields", differences=field_diffs))
    repr_c = r_struct is not None and any(re.fullmatch(r"#\s*\[repr\s*\(\s*C\s*\)\s*\]", a) for a in r_struct["attributes"])
    if not repr_c:
        findings.append(dict(category="layout", name="liteeth", message="Rust 缺少 #[repr(C)]，字段列表相同不能保证字段偏移与 C 相同"))
    signatures = dependencies(rust)
    for item in signatures:
        if not item["equal"]:
            findings.append(dict(category="dependency_signature", **item))
    # 名义类型必须连同底层定义核对，避免只保留别名而偷偷更换整数宽度/回调签名。
    alias_contracts = {"irqreturn_t": "i32", "netdev_tx_t": "u32",
                       "irq_handler_t": "unsafefn(i32,*mutc_void)->irqreturn_t"}
    alias_evidence = []
    for name, expected in alias_contracts.items():
        actual = rust.type_aliases.get(name)
        item = dict(name=name, expected=expected, actual=actual,
                    equal=actual is not None and actual["type"] == expected and
                    all(a.startswith("# [doc") for a in actual.get("attributes", [])))
        alias_evidence.append(item)
        if not item["equal"]:
            findings.append(dict(category="dependency_type_alias", **item))
    macro_evidence = []
    for name in sorted(MACRO_INTERFACES):
        definition = rust.macro_definitions.get(name)
        attributes = [re.sub(r"\s+", "", a) for a in definition.get("attributes", [])] if definition else []
        present = definition is not None and "#[macro_export]" in attributes and all(a == "#[macro_export]" or a.startswith("#[doc") for a in attributes)
        macro_evidence.append(dict(name=name, present=present, body_verified=False, definition=definition))
        if not present:
            findings.append(dict(category="missing_dependency_macro", name=name))
    globals_c, globals_r = {}, {}
    for side, normalizer in [("c", clang), ("rust", rust)]:
        try:
            if side == "c":
                globals_c = normalizer.globals()
            else:
                globals_r = normalizer.globals()
        except Unsupported as error:
            findings.append(dict(category="global_unsupported", side=side, message=str(error), loc=error.location))
    if globals_c and globals_r:
        global_diffs = difference(globals_c, globals_r, "globals")
        if global_diffs:
            findings.append(dict(category="global_ast", differences=global_diffs))
    normalized_c["$globals"], normalized_r["$globals"] = globals_c, globals_r
    report = dict(schema=1, passed=not findings, semantic_equivalence_proven=False,
                  strict_constraints_fully_satisfied=False,
                  parser=dict(c="Clang semantic AST", rust="syn syntax AST", rust_typechecked=False),
                  functions=functions, entry_inventory=inventories, constants=dict(c=c_constants, rust=r_constants),
                  struct=dict(c=c_fields, rust=r_fields, repr_c=repr_c),
                  dependency_signatures=signatures, type_aliases=alias_evidence, dependency_macros=macro_evidence, findings=findings,
                  globals=dict(c=globals_c, rust=globals_r),
                  normalization_rules=dict(c=clang.rules, rust=rust.rules),
                  limitations=["Rust 使用真实语法 AST，尚无 rustc 的名称解析、借用检查和完整类型推导；只推导当前子集的表达式类型。",
                               "Linux 宏保留原始接口调用和实参 AST；展开树保存在原始 AST 中，宏依赖实现的语义未自动证明。",
                               "模块宏只比较声明实参、回调和匹配表引用；不声称 Rust 宏实现了 Linux 的注册/注销或模块别名导出。",
                               "依赖仅检查普通接口签名，不证明 IRQ 上下文、网络栈、资源释放、分配器或多核行为相等。",
                               "约束 4 当前核对 LiteEth 字段及 C 布局标记；分配连续性、清零、失败及释放范围仍需独立核对。",
                               "结果仅适用于固定 Linux 源码、ARM64 配置和列出的显式映射；AST 相同不是完整驱动正确性证明。"])
    return report, normalized_c, normalized_r


def write_result(output, report, c_tree, r_tree):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    for name, data in [("report.json", report), ("c.normalized.json", c_tree), ("rust.normalized.json", r_tree)]:
        (output / name).write_text(json.dumps(data, ensure_ascii=False, indent=2)+"\n")
    lines = ["# LiteEth AST 比较结果", "", "结果："+("已覆盖的规范化检查通过。" if report["passed"] else "尚不能判定两侧 AST 相同。"),
             "", "这不是依赖实现或完整驱动语义等价证明。", "",
             "| 函数 | 结果 | 树差异数 |", "| --- | --- | --- |"]
    for item in report["functions"]:
        lines.append(f"| `{item['name']}` | {item['status']} | {len(item.get('differences', []))} |")
    for item in report["findings"]:
        lines += ["", "## "+item["category"]+" "+item.get("function", item.get("name", "")), ""]
        if "differences" in item:
            for diff in item["differences"]:
                def reference(side):
                    loc = diff.get(side+"_loc")
                    return f"[{side}:{loc['line']}]({loc['file']}:{loc['line']})" if loc else side
                left, right = json.dumps(diff["c"], ensure_ascii=False), json.dumps(diff["rust"], ensure_ascii=False)
                # 完整子树在 JSON；这里控制单行长度，方便先定位再点开源码。
                lines += [f"- `{diff['path']}`：{reference('c')} `{left[:140]}` → {reference('rust')} `{right[:140]}`"]
        else:
            lines.append(item.get("message", json.dumps(item, ensure_ascii=False)))
    lines += ["", "## 范围", ""] + ["- "+text for text in report["limitations"]]
    (output / "report.md").write_text("\n".join(lines)+"\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux", type=Path, default=DEFAULT_LINUX)
    parser.add_argument("--c", type=Path)
    parser.add_argument("--rust", type=Path, default=DEFAULT_RUST)
    parser.add_argument("--output", type=Path, default=ROOT / "target/liteeth-ast/current")
    args = parser.parse_args()
    source = args.c or args.linux / "drivers/net/ethernet/litex/litex_liteeth.c"
    try:
        environment = clang_environment(args.linux)
        c_tree, c_metadata = c_ast(source, environment, args.output / "c")
        r_data = rust_ast([args.rust]+API_FILES, args.output / "rust")
        report, c_normalized, r_normalized = compare_models(c_tree, source, r_data, args.rust)
        report["inputs"] = dict(c=c_metadata, rust=r_data["inputs"], signatures_sha256=digest(SIGNATURES))
        write_result(args.output, report, c_normalized, r_normalized)
    except (ParseError, Unsupported, ValueError, OSError) as error:
        args.output.mkdir(parents=True, exist_ok=True)
        (args.output / "report.json").write_text(json.dumps(dict(passed=False, parser_error=str(error)), ensure_ascii=False, indent=2))
        print(str(error), file=sys.stderr)
        return 2
    counts = {s: sum(f["status"] == s for f in report["functions"]) for s in ["equal", "different", "unsupported"]}
    print(f"AST：{counts['equal']} 函数相同，{counts['different']} 函数不同，{counts['unsupported']} 函数未支持；{len(report['findings'])} 项发现")
    print(args.output / "report.json")
    if any(f["category"] in {"unsupported", "global_unsupported", "source_boundary"} for f in report["findings"]):
        return 2
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
