"""LiteEth 的显式 C/Rust AST 映射；未知节点报错，绝不当作空语句删除。"""
import bisect
import json
from pathlib import Path
import re

from ast_frontends import ENTRIES

LOGS = {"dev_dbg", "dev_err", "netdev_err", "netdev_info"}
OPAQUE_MACROS = LOGS | {"devm_alloc_etherdev", "devm_netdev_alloc_pcpu_stats",
                       "SET_NETDEV_DEV", "memcpy_fromio", "memcpy_toio", "unlikely"}
CONSTANTS = {"NET_RX_DROP", "NETDEV_TX_OK", "NETDEV_TX_BUSY", "IRQ_HANDLED", "ENOMEM"}
MACRO_INTERFACES = LOGS | {"devm_netdev_alloc_pcpu_stats", "MODULE_DEVICE_TABLE", "module_platform_driver",
                          "MODULE_AUTHOR", "MODULE_DESCRIPTION", "MODULE_LICENSE"}
CALL_INTERFACES = set(json.loads(Path(__file__).with_name("linux_signatures.json").read_text())) | LOGS | {"devm_netdev_alloc_pcpu_stats"} | set(ENTRIES)


class Unsupported(RuntimeError):
    def __init__(self, message, location=None):
        super().__init__(message)
        self.location = location


def plain(node):
    """位置用于定位，不参加跨语言相等判断；类型、运算、参数、顺序全部保留。"""
    if isinstance(node, dict):
        return {k: plain(v) for k, v in node.items() if k != "loc"}
    if isinstance(node, list):
        return [plain(v) for v in node]
    return node


def walk(node):
    if isinstance(node, dict):
        yield node
        for value in node.values():
            yield from walk(value)
    elif isinstance(node, list):
        for value in node:
            yield from walk(value)


def ctype(raw):
    raw = raw.strip()
    raw = re.sub(r"\b(const|volatile|restrict|__restrict|__restrict__)\b", "", raw)
    raw = raw.replace("struct ", "").strip()
    if raw.endswith("*"):
        return "ptr(" + ctype(raw[:-1]) + ")"
    raw = re.sub(r"\s+", " ", raw)
    aliases = {"void": "void", "char": "i8", "signed char": "i8", "unsigned char": "u8",
               "short": "i16", "unsigned short": "u16", "int": "i32", "unsigned int": "u32",
               "long": "i64", "long int": "i64", "unsigned long": "u64", "long unsigned int": "u64",
               "long long": "i64", "unsigned long long": "u64", "__size_t": "u64", "size_t": "u64",
               "__be16": "u16", "_Bool": "bool", "enum netdev_tx": "netdev_tx_t",
               "enum irqreturn": "irqreturn_t", "typeof(pcpu_sw_netstats)": "pcpu_sw_netstats"}
    return aliases.get(raw, raw)


def rtype(raw):
    raw = re.sub(r"\s+", "", raw)
    if raw.startswith(("*mut", "*const")):
        return "ptr(" + rtype(raw[4:] if raw.startswith("*mut") else raw[6:]) + ")"
    return {"()": "void", "usize": "u64", "isize": "i64", "c_void": "void", "c_char": "i8"}.get(raw, raw)


def volatile_object(node):
    raw = node.get("type", {})
    raw = raw.get("desugaredQualType", raw.get("qualType", ""))
    # volatile T* 传递的是地址；T* volatile 则连指针对象自身的读写也有 volatile 效果。
    return re.search(r"\bvolatile\b", raw.rsplit("*", 1)[-1]) is not None


def ptr_pointee(value):
    return value[4:-1] if value.startswith("ptr(") and value.endswith(")") else None


def make(op, loc, dtype=None, **fields):
    return {"op": op, **({"type": dtype} if dtype is not None else {}), **fields, "loc": loc}


def integer(value, loc, dtype="i32"):
    return make("integer", loc, dtype, value=int(value))


def symbol(name, dtype, loc):
    return make("symbol", loc, dtype, name=name)


class Normalizer:
    def __init__(self, functions, fields, constants):
        self.functions, self.fields, self.constants = functions, fields, constants
        self.rules, self.locals, self.parameters, self.addressed = [], {}, {}, set()
        self.current_return = "void"

    def note(self, rule, loc, **detail):
        self.rules.append(dict(rule=rule, loc=loc, **detail))

    def cast(self, expression, target, loc, reason="conversion"):
        source = expression["type"]
        if source == target:
            return expression
        if expression["op"] == "string" and target in {"ptr(i8)", "string"}:
            return expression
        # 指针的 const/__iomem 是检查器的原始类型证据；不改变指针数值。
        # 这里仍保留 pointee 变化，不能把 void* 与具体结构体* 一概抹平。
        self.note(reason, loc, source_type=source, target_type=target)
        value = expression.get("value")
        if expression["op"] in {"integer", "constant"} and isinstance(value, int):
            match = re.fullmatch(r"([iu])(8|16|32|64)", target)
            if match:
                signed, bits = match[1] == "i", int(match[2])
                low, high = (-(1 << (bits-1)), (1 << (bits-1))-1) if signed else (0, (1 << bits)-1)
                if low <= value <= high:
                    return dict(expression, type=target)
        return make("convert", loc, target, expression=expression)

    def truth(self, expression, loc):
        if expression["type"] == "bool":
            return expression
        return make("truth", loc, "bool", expression=expression)

    def binary(self, operator, left, right, dtype, loc):
        if operator in {"==", "!="}:
            if right["op"] == "integer" and right["value"] == 0:
                value = self.truth(left, loc)
                return value if operator == "!=" else make("not", loc, "bool", expression=value)
        if operator in {"&&", "||"}:
            left, right, dtype = self.truth(left, loc), self.truth(right, loc), "bool"
        if operator in {"==", "!=", ">", "<", ">=", "<="}:
            dtype = "bool"
        return make("binary", loc, dtype, operator=operator, left=left, right=right)

    def member(self, base, name, loc, dtype=None):
        if base["op"] == "deref":
            base = base["expression"]
        parent = ptr_pointee(base["type"]) or base["type"]
        dtype = dtype or self.fields.get((parent, name))
        if not dtype:
            raise Unsupported(f"未知字段类型：{parent}.{name}", loc)
        if parent == "net_device" and name == "name":
            self.note("net-device-name-array-wrapper", loc, raw_type=dtype)
            dtype = "device_name"
        return make("field", loc, dtype, base=base, name=name)

    def address(self, expression, loc):
        if expression["op"] == "symbol":
            self.addressed.add(expression["name"])
        return make("address", loc, "ptr(" + expression["type"] + ")", expression=expression)

    def ptr_add(self, base, offset, loc):
        if ptr_pointee(base["type"]) != "void":
            raise Unsupported("目前只定义了 LiteEth void* 的字节偏移映射", loc)
        return make("pointer_offset", loc, base["type"], base=base,
                    offset=self.cast(offset, "u64", loc, "C-byte-pointer-offset"))

    def call(self, name, args, loc, dtype=None):
        if name not in CALL_INTERFACES:
            raise Unsupported("依赖不在已审核映射清单中："+name, loc)
        if name in LOGS:
            if len(args) < 2 or args[1]["op"] != "string":
                raise Unsupported("日志格式必须是字面量", loc)
            fmt = args[1]["value"]
            if "%" in fmt:
                remainder = re.sub(r"%[du]", "", fmt)
                if "%" in remainder:
                    raise Unsupported("未支持的 C 日志格式符", loc)
                fmt = re.sub(r"%[du]", "{}", fmt)
                self.note("printf-d-u-to-Rust-display", loc)
            if fmt.count("{}") != len(args)-2:
                raise Unsupported("日志格式与实参数量不符", loc)
            args[1] = dict(args[1], value=fmt)
            # 格式的有符号性另列义务，参数表达式、顺序仍逐项比较。
            dtype = "void"
        elif name == "devm_netdev_alloc_pcpu_stats":
            dtype = "ptr(pcpu_sw_netstats)"
        elif name in self.functions:
            definition = self.functions[name]
            expected = definition["parameters"]
            if len(expected) != len(args):
                raise Unsupported(f"{name} 实参数量 {len(args)}，定义要求 {len(expected)}", loc)
            args = [self.cast(arg, parameter["type"], arg["loc"], "call-argument-conversion")
                    for arg, parameter in zip(args, expected)]
            dtype = dtype or definition["return"]
        else:
            raise Unsupported(f"未登记的依赖调用：{name}", loc)
        # Linux 的 void* 字节缓冲接口在 Rust 清单中映射为 *mut/const u8。
        # 仅在这三个已登记接口的相应角色归一，不删除普通指针 cast 或算术 cast。
        byte_parameters = {"memcpy_fromio": [0], "memcpy_toio": [1]}
        for index in byte_parameters.get(name, []):
            arg = args[index]
            if arg["op"] == "convert" and arg["type"] == "ptr(void)" and arg["expression"]["type"] == "ptr(u8)":
                args[index] = arg["expression"]
                self.note("void-buffer-parameter-to-byte-pointer", loc, name=name, argument=index)
        if name == "skb_put" and dtype == "ptr(void)":
            dtype = "ptr(u8)"
            self.note("skb-put-void-return-to-byte-pointer", loc)
        return make("call", loc, dtype, name=name, args=args)

    def local(self, name, dtype, loc):
        if name in self.locals:
            if self.locals[name] != dtype or name in self.addressed:
                raise Unsupported(f"不能归并带地址逃逸或类型变化的同名局部变量：{name}", loc)
            self.note("scalar-shadow-matches-C-reassignment", loc, name=name)
        self.locals[name] = dtype


class CNormalizer(Normalizer):
    def __init__(self, roots, source, functions):
        self.roots, self.source = roots, Path(source).resolve()
        self.raw = self.source.read_bytes()
        self.lines = [-1] + [i for i, c in enumerate(self.raw) if c == 10]
        fields = {}
        for node in walk(roots):
            if node.get("kind") == "MemberExpr" and node.get("name"):
                base = node["inner"][0].get("type", {}).get("qualType", "")
                parent = ptr_pointee(ctype(base)) or ctype(base)
                fields[parent, node["name"]] = ctype(node["type"]["qualType"])
        constants = {}
        for node in walk(roots):
            if node.get("kind") == "EnumConstantDecl" and node.get("name", "").startswith("liteeth_ast_value_"):
                values = [x["value"] for x in walk(node.get("inner", [])) if x.get("kind") == "ConstantExpr" and "value" in x]
                if len(values) != 1:
                    raise Unsupported("Clang 未返回唯一常量值", self.loc(node))
                name = node["name"].removeprefix("liteeth_ast_value_")
                dtype = "irqreturn_t" if name == "IRQ_HANDLED" else "netdev_tx_t" if name.startswith("NETDEV_TX_") else "i32"
                constants[name] = (int(values[0]), dtype)
        super().__init__(functions, fields, constants)
        self.suppress_macro = set()

    def globals(self):
        def field_name(parent, child):
            if child.get("field"):
                return child["field"]["name"]
            a = parent["range"]["begin"]["offset"]
            begin = child["range"]["begin"]
            b = begin.get("expansionLoc", begin)["offset"]
            # 字段值来自 Clang InitListExpr；这里只取它紧前方的指定初始化器名字。
            match = re.search(rb"\.([A-Za-z_]\w*)\s*=\s*$", self.raw[a:b])
            if not match:
                raise Unsupported("Clang 初始化器无法对应到指定字段", self.loc(child))
            return match[1].decode()

        def initializer(node):
            kind = node["kind"]
            if kind == "InitListExpr":
                if node.get("field"):
                    return initializer(node["inner"][0])
                dtype = ctype(node["type"]["qualType"])
                if re.search(r"\[\d+\]$", dtype):
                    return dict(kind="array", items=[initializer(x) for x in node.get("inner", [])], loc=self.loc(node))
                fields = {}
                for child in node.get("inner", []):
                    if child["kind"] == "ImplicitValueInitExpr":
                        continue
                    name = field_name(node, child)
                    fields[name] = initializer(child)
                if dtype == "of_device_id" and "compatible" not in fields:
                    fields["compatible"] = dict(kind="string", value="", loc=self.loc(node))
                return dict(kind="record", type=dtype, fields=fields, loc=self.loc(node))
            if kind in {"ImplicitCastExpr", "ParenExpr"}:
                return initializer(node["inner"][0])
            if kind == "StringLiteral":
                return dict(kind="string", value=json.loads(node["value"]), loc=self.loc(node))
            if kind == "DeclRefExpr":
                return dict(kind="reference", name=node["referencedDecl"]["name"], loc=self.loc(node))
            raise Unsupported("未支持的 C 全局初始化器：" + kind, self.loc(node))

        result = {}
        for node in self.roots:
            if node.get("kind") == "VarDecl" and node.get("name") in {"liteeth_netdev_ops", "liteeth_of_match", "liteeth_driver"}:
                result[node["name"]] = initializer(node["inner"][0])
        registration = []
        for node in self.roots:
            name = node.get("name", "")
            if node.get("kind") == "FunctionDecl" and name in {"liteeth_driver_init", "liteeth_driver_exit"}:
                references = [x["referencedDecl"]["name"] for x in walk(node) if x.get("kind") == "DeclRefExpr"
                              and x.get("referencedDecl", {}).get("kind") == "VarDecl" and x["referencedDecl"]["name"].startswith("liteeth_")]
                registration.append(dict(operation="register" if name.endswith("_init") else "unregister", drivers=references))
        if registration and (len(registration) != 2 or registration[0]["drivers"] != registration[1]["drivers"]):
            raise Unsupported("C 生成的模块注册/注销指向不同对象，尚不能归并成驱动绑定")
        result["module_driver_binding"] = registration[0]["drivers"] if registration else []
        self.note("C-module-binding-from-generated-entrypoints", None, evidence=registration,
                  target_lifecycle_equivalence_verified=False)
        aliases = [n["name"] for n in self.roots if n.get("kind") == "VarDecl" and n.get("name", "").startswith("__mod_")]
        tables = []
        for alias in aliases:
            match = re.fullmatch(r"__mod_(\w+)__(\w+)_device_table", alias)
            if not match:
                raise Unsupported("未支持的 C 模块匹配表别名："+alias)
            tables.append(dict(bus=match[1], table=match[2]))
        result["device_table_binding"] = tables
        metadata = []
        for node in self.roots:
            if node.get("kind") == "VarDecl" and node.get("name", "").startswith("__UNIQUE_ID_"):
                values = [json.loads(x["value"]) for x in walk(node) if x.get("kind") == "StringLiteral"]
                for value in values:
                    if value.startswith(("author=", "description=", "license=")):
                        key, value = value.split("=", 1)
                        metadata.append(dict(key=key, value=value))
        result["module_metadata"] = sorted(metadata, key=lambda x: x["key"])
        return result

    def loc(self, node):
        loc = node.get("range", {}).get("begin", node.get("loc", {}))
        loc = loc.get("expansionLoc", loc)
        offset = loc.get("offset", 0)
        line = bisect.bisect_left(self.lines, offset)
        return dict(file=str(self.source), line=line, column=offset-self.lines[line-1], offset=offset)

    def macro(self, node):
        begin = node.get("range", {}).get("begin", {})
        expansion = begin.get("expansionLoc")
        if not expansion or expansion.get("isMacroArgExpansion"):
            return None
        offset = expansion["offset"]
        if offset in self.suppress_macro:
            return None
        match = re.match(rb"[A-Za-z_]\w*", self.raw[offset:])
        if not match:
            return None
        name = match[0].decode()
        if name not in OPAQUE_MACROS:
            return None
        position = offset + len(match[0])
        while self.raw[position:position+1].isspace():
            position += 1
        if self.raw[position:position+1] != b"(":
            raise Unsupported(f"宏 {name} 缺少参数列表", self.loc(node))
        # 只切分原始宏实参；实参表达式取自下面 Clang 的展开 AST，不自行解析 C 表达式。
        starts, args, depth, quote, escaped = position+1, [], 0, None, False
        for end in range(position+1, len(self.raw)):
            char = self.raw[end:end+1]
            if quote:
                if escaped:
                    escaped = False
                elif char == b"\\":
                    escaped = True
                elif char == quote:
                    quote = None
                continue
            if char in {b'"', b"'"}:
                quote = char
            elif char in {b"(", b"[", b"{"}:
                depth += 1
            elif char in {b")", b"]", b"}"}:
                if char == b")" and depth == 0:
                    args.append((starts, end))
                    break
                depth -= 1
            elif char == b"," and depth == 0:
                args.append((starts, end))
                starts = end+1
        else:
            raise Unsupported("未终止的宏调用", self.loc(node))
        normalized = []
        for start, end in args:
            while self.raw[start:start+1].isspace():
                start += 1
            while self.raw[end-1:end].isspace():
                end -= 1
            text = self.raw[start:end].decode()
            if "/*" in text or "//" in text:
                raise Unsupported("宏实参内部注释尚未支持定位", self.loc(node))
            if name == "devm_netdev_alloc_pcpu_stats" and text == "struct pcpu_sw_netstats":
                normalized.append(make("type_argument", self.loc(node), "type", name="pcpu_sw_netstats"))
                continue
            candidates = []
            for child in walk(node):
                bounds = child.get("range", {})
                left, right = bounds.get("begin", {}), bounds.get("end", {})
                a, b = left.get("spellingLoc", left), right.get("spellingLoc", right)
                if (a.get("offset") == start and b.get("offset", -1)+b.get("tokLen", 0) == end
                        and (left.get("expansionLoc", {}).get("isMacroArgExpansion") or "spellingLoc" not in left)
                        and child.get("kind", "").endswith(("Expr", "Literal", "Operator"))):
                    candidates.append(child)
            if not candidates:
                raise Unsupported(f"无法从 Clang AST 找回宏 {name} 的实参：{text}", self.loc(node))
            self.suppress_macro.add(offset)
            try:
                normalized.append(self.expr(candidates[0]))
            finally:
                self.suppress_macro.remove(offset)
        self.note("preserve-original-macro-invocation", self.loc(node), name=name,
                  expanded_kind=node["kind"], dependency_body_verified=False)
        if name == "unlikely":
            if len(normalized) != 1:
                raise Unsupported("unlikely 参数不符", self.loc(node))
            normalized[0] = self.truth(normalized[0], self.loc(node))
            return make("call", self.loc(node), "bool", name=name, args=normalized)
        return self.call(name, normalized, self.loc(node))

    def expr(self, node):
        kind, loc = node["kind"], self.loc(node)
        macro = self.macro(node)
        if macro:
            return macro
        if volatile_object(node):
            raise Unsupported("直接 volatile 对象访问尚未定义映射，不能降成普通读写", loc)
        inner = node.get("inner", [])
        dtype = ctype(node.get("type", {}).get("qualType", "void"))
        if kind in {"ParenExpr", "ConstantExpr"}:
            return self.expr(inner[0])
        if kind == "ImplicitCastExpr":
            value = self.expr(inner[0])
            conversion = node["castKind"]
            if conversion == "ArrayToPointerDecay" and value["type"] == "device_name":
                return make("array_decay", loc, "ptr(i8)", expression=value)
            if conversion in {"LValueToRValue", "FunctionToPointerDecay", "ArrayToPointerDecay", "NoOp"}:
                return value
            if conversion in {"IntegralCast", "BitCast", "IntegralToBoolean", "PointerToBoolean"}:
                return self.truth(value, loc) if dtype == "bool" else self.cast(value, dtype, loc, conversion)
            raise Unsupported(f"未支持的 C 隐式转换 {conversion}", loc)
        if kind == "DeclRefExpr":
            name = node["referencedDecl"]["name"]
            if name in self.constants:
                value, dtype = self.constants[name]
                return make("constant", loc, dtype, name=name, value=value)
            if node["referencedDecl"]["kind"] == "FunctionDecl":
                return symbol(name, "fn("+name+")", loc)
            return symbol(name, dtype, loc)
        if kind == "IntegerLiteral":
            begin = node.get("range", {}).get("begin", {})
            expansion = begin.get("expansionLoc", {})
            match = re.match(rb"[A-Za-z_]\w*", self.raw[expansion.get("offset", len(self.raw)):])
            if match and match[0].decode() in self.constants:
                name = match[0].decode()
                value, dtype = self.constants[name]
                return make("constant", loc, dtype, name=name, value=value)
            return integer(node["value"], loc, dtype)
        if kind == "StringLiteral":
            return make("string", loc, "string", value=json.loads(node["value"]))
        if kind == "MemberExpr":
            base = self.expr(inner[0])
            if not node.get("name"):
                return base
            return self.member(base, node["name"], loc, dtype)
        if kind == "CallExpr":
            callee = self.expr(inner[0])
            if callee["op"] != "symbol":
                raise Unsupported("目前只比较直接依赖调用", loc)
            return self.call(callee["name"], [self.expr(x) for x in inner[1:]], loc, dtype)
        if kind in {"BinaryOperator", "CompoundAssignOperator"}:
            operator = node["opcode"]
            left, right = self.expr(inner[0]), self.expr(inner[1])
            if operator == "=":
                return make("assign", loc, left=left, right=self.cast(right, left["type"], loc))
            if operator.endswith("=") and kind == "CompoundAssignOperator":
                return make("update", loc, operator=operator[:-1], left=left,
                            right=self.cast(right, left["type"], loc))
            if operator == "+" and ptr_pointee(left["type"]):
                return self.ptr_add(left, right, loc)
            return self.binary(operator, left, right, dtype, loc)
        if kind == "UnaryOperator":
            value, operator = self.expr(inner[0]), node["opcode"]
            if operator == "&":
                return self.address(value, loc)
            if operator == "*":
                return make("deref", loc, dtype, expression=value)
            if operator == "!":
                return make("not", loc, "bool", expression=self.truth(value, loc))
            if operator in {"++", "--"}:
                return make("update", loc, operator="+" if operator == "++" else "-", left=value,
                            right=integer(1, loc, value["type"]))
            if operator in {"-", "+", "~"}:
                return make("unary", loc, dtype, operator=operator, expression=value)
        if kind == "UnaryExprOrTypeTraitExpr" and node.get("name") == "sizeof":
            target = ctype(node["argType"]["qualType"]) if "argType" in node else ctype(inner[0]["type"]["qualType"])
            return make("sizeof", loc, "u64", target=target)
        raise Unsupported(f"未支持的 C 表达式 {kind}", loc)

    def statements(self, node):
        kind, loc = node["kind"], self.loc(node)
        inner = node.get("inner", [])
        if kind == "CompoundStmt":
            return [value for child in inner for value in self.statements(child)]
        if kind == "DeclStmt":
            result = []
            for declaration in inner:
                if declaration["kind"] != "VarDecl":
                    raise Unsupported("块内不是变量声明", loc)
                if volatile_object(declaration):
                    raise Unsupported("volatile 局部对象需要显式映射", self.loc(declaration))
                name, dtype = declaration["name"], ctype(declaration["type"]["qualType"])
                self.local(name, dtype, self.loc(declaration))
                init = [x for x in declaration.get("inner", []) if x.get("kind") != "UnusedAttr"]
                if init:
                    if len(init) != 1:
                        raise Unsupported("未支持的局部声明属性", loc)
                    result.append(make("assign", loc, left=symbol(name, dtype, self.loc(declaration)),
                                       right=self.cast(self.expr(init[0]), dtype, loc)))
            return result
        if kind == "IfStmt":
            if len(inner) not in {2, 3}:
                raise Unsupported("C if 节点形状未支持", loc)
            return [make("if", loc, condition=self.truth(self.expr(inner[0]), loc),
                         then=self.statements(inner[1]), otherwise=self.statements(inner[2]) if len(inner) == 3 else [])]
        if kind == "ReturnStmt":
            return [make("return", loc, expression=self.cast(self.expr(inner[0]), self.current_return, loc) if inner else None)]
        if kind == "GotoStmt":
            # Clang 使用 label ID；从真实 LabelStmt 映射回名字。
            return [make("goto", loc, label=self.labels[node["targetLabelDeclId"]])]
        if kind == "LabelStmt":
            return [make("label", loc, name=node["name"])] + self.statements(inner[0])
        if kind == "NullStmt":
            return [make("empty", loc)]
        value = self.expr(node)
        return [value if value["op"] in {"assign", "update"} else make("evaluate", loc, expression=value)]

    def function(self, node):
        self.locals, self.parameters, self.addressed = {}, {}, set()
        self.labels = {x["declId"]: x["name"] for x in walk(node) if x.get("kind") == "LabelStmt"}
        params = [{"name": x["name"], "type": ctype(x["type"]["qualType"]), "loc": self.loc(x)}
                  for x in node["inner"] if x["kind"] == "ParmVarDecl"]
        self.current_return = ctype(node["type"]["qualType"].split(" (", 1)[0])
        self.parameters = {x["name"]: x["type"] for x in params}
        body = [x for x in node["inner"] if x["kind"] == "CompoundStmt"]
        if len(body) != 1:
            raise Unsupported("C 函数必须有唯一函数体", self.loc(node))
        statements = self.statements(body[0])
        return dict(parameters=params, returns=self.current_return, locals=dict(sorted(self.locals.items())),
                    body=statements, loc=self.loc(node))


def rust_signature(item):
    signature = item["signature"]
    params = []
    for param in signature["parameters"]:
        if param.get("attributes"):
            raise Unsupported("参数属性可能改变签名，尚未支持", param.get("loc"))
        pattern = param.get("pattern", {})
        if pattern.get("kind") != "binding" or pattern.get("by_ref") or pattern.get("subpattern"):
            raise Unsupported("入口/依赖签名不是普通具名参数", param.get("loc"))
        params.append(dict(name=pattern["name"], type=rtype(param["type"]), loc=param["loc"]))
    return dict(parameters=params, returns=rtype(signature["return"]))


class RustNormalizer(Normalizer):
    def __init__(self, files, source):
        self.source = str(Path(source).resolve())
        self.files = files
        source_file = next(x for x in files if str(Path(x["path"]).resolve()) == self.source)
        self.items = source_file["items"]
        self.file_attributes = source_file.get("attributes", [])
        functions, fields, constants = {}, {}, {}
        self.structs, self.raw_functions, self.raw_function_paths, self.type_aliases = {}, {}, {}, {}
        self.macro_definitions = {}
        for file in files:
            for item in file["items"]:
                kind = item["kind"]
                if kind == "function":
                    name = item["signature"]["name"]
                    signature = rust_signature(item)
                    functions[name] = {"parameters": signature["parameters"], "return": signature["returns"]}
                    self.raw_functions[name] = item
                    self.raw_function_paths[name] = file["path"]
                elif kind == "struct":
                    self.structs[item["name"]] = item
                    for field in item["fields"]:
                        fields[item["name"], field["name"]] = rtype(field["type"])
                elif kind == "constant":
                    expression = item["expression"]
                    if expression["kind"] == "integer":
                        constants[item["name"]] = (int(expression["value"]), rtype(item["type"]))
                elif kind == "type_alias":
                    self.type_aliases[item["name"]] = dict(type=item["type"], attributes=item.get("attributes", []), loc=dict(file=file["path"], **item["loc"]))
                elif kind == "item_macro" and item.get("name"):
                    self.macro_definitions[item["name"]] = dict(tokens=item["definition_tokens"], attributes=item.get("attributes", []), loc=dict(file=file["path"], **item["loc"]))
        super().__init__(functions, fields, constants)
        self.label_stack = []

    def loc(self, node):
        return {"file": self.source, **node["loc"]}

    def expr(self, node, expected=None):
        kind, loc = node["kind"], self.loc(node)
        if kind in {"paren", "group"}:
            return self.expr(node["expression"], expected)
        if kind == "path":
            parts = node["path"]["segments"]
            if node.get("qself") or node["path"]["generics"] or len(parts) != 1:
                raise Unsupported("未支持的 Rust 表达式路径", loc)
            name = parts[0]
            if name in self.constants:
                value, dtype = self.constants[name]
                if name == "IRQ_HANDLED" and dtype == "i32":
                    self.note("IRQ-HANDLED-i32-to-positive-enum-domain", loc, value=value)
                    dtype = "irqreturn_t"
                return make("constant", loc, dtype, name=name, value=value)
            if name in self.locals or name in self.parameters:
                return symbol(name, self.locals.get(name, self.parameters.get(name)), loc)
            if name in self.functions:
                return symbol(name, "fn("+name+")", loc)
            if name in {"liteeth_netdev_ops", "liteeth_of_match", "liteeth_driver"}:
                return symbol(name, {"liteeth_netdev_ops": "net_device_ops", "liteeth_of_match": "of_device_id[]",
                                     "liteeth_driver": "platform_driver"}[name], loc)
            raise Unsupported(f"未知的 Rust 标识符 {name}", loc)
        if kind == "integer":
            dtype = rtype(node["suffix"]) if node["suffix"] else expected or "i32"
            primitive = rtype(self.type_aliases[dtype]["type"]) if dtype in self.type_aliases else dtype
            match = re.fullmatch(r"([iu])(8|16|32|64)", primitive)
            if not match:
                raise Unsupported("整数字面量不能推导为非整数类型："+dtype, loc)
            signed, bits = match[1] == "i", int(match[2])
            maximum = (1 << (bits-1 if signed else bits))-1
            if int(node["value"]) > maximum:
                raise Unsupported("Rust 整数字面量超出已推导类型范围", loc)
            return integer(node["value"], loc, dtype)
        if kind in {"string", "c_string"}:
            return make("string", loc, "string", value=node["value"])
        if kind == "boolean":
            return make("boolean", loc, "bool", value=node["value"] == "true")
        if kind == "field":
            return self.member(self.expr(node["base"]), node["field"], loc)
        if kind == "cast":
            return self.cast(self.expr(node["expression"]), rtype(node["type"]), loc, "Rust-as")
        if kind == "reference":
            return self.address(self.expr(node["expression"]), loc)
        if kind == "unary":
            value, operator = self.expr(node["expression"], expected), node["operator"]
            if operator == "*":
                dtype = ptr_pointee(value["type"])
                if not dtype:
                    raise Unsupported("Rust 解引用对象不是已知指针", loc)
                return make("deref", loc, dtype, expression=value)
            if operator == "!":
                if value["type"] != "bool":
                    raise Unsupported("Rust 位反操作尚未定义对应规则", loc)
                return make("not", loc, "bool", expression=value)
            if operator == "-":
                return make("unary", loc, value["type"], operator=operator, expression=value)
            raise Unsupported(f"未知 Rust 一元操作 {operator}", loc)
        if kind == "assign":
            left = self.expr(node["left"])
            return make("assign", loc, left=left,
                        right=self.expr(node["right"], left["type"]))
        if kind == "binary":
            operator = node["operator"]
            left = self.expr(node["left"], expected)
            if operator in {"+=", "-=", "*=", "/=", "%="}:
                return make("update", loc, operator=operator[:-1], left=left,
                            right=self.expr(node["right"], left["type"]))
            right = self.expr(node["right"], left["type"])
            dtype = left["type"]
            if operator in {"+", "-", "*", "/", "%"} and left["type"] != right["type"]:
                raise Unsupported("Rust 算术操作数类型不一致", loc)
            return self.binary(operator, left, right, dtype, loc)
        if kind == "method":
            receiver = self.expr(node["receiver"])
            method, args = node["name"], node["args"]
            generics = node["generics"]
            if method == "is_null" and not args and not generics and ptr_pointee(receiver["type"]):
                return make("not", loc, "bool", expression=self.truth(receiver, loc))
            if method == "cast" and not args and ptr_pointee(receiver["type"]):
                target = "ptr(" + rtype(generics[0]) + ")" if len(generics) == 1 else expected
                if not target or not ptr_pointee(target):
                    raise Unsupported("无法确定 Rust 指针 cast 的目标类型", loc)
                return self.cast(receiver, target, loc, "Rust-pointer-cast")
            if method == "byte_add" and len(args) == 1 and not generics:
                self.note("Rust-byte-add-to-C-void-pointer-add", loc)
                offset = self.expr(args[0], "u64")
                if offset["type"] != "u64":
                    raise Unsupported("byte_add 缺少到 usize 的显式转换，不能补出 C 的隐式转换", loc)
                return self.ptr_add(receiver, offset, loc)
            if method == "as_ptr" and not args and not generics:
                if receiver["op"] == "string":
                    self.note("CStr-as-ptr-to-C-string-literal", loc)
                    return receiver
                if receiver["type"] == "device_name":
                    return make("array_decay", loc, "ptr(i8)", expression=receiver)
            raise Unsupported(f"未支持的 Rust 方法调用 {method}", loc)
        if kind == "call":
            callee = node["callee"]
            if callee["kind"] != "path" or callee.get("qself"):
                raise Unsupported("未支持的 Rust 间接调用", loc)
            path = callee["path"]
            if path["segments"] == ["core", "mem", "size_of"] and len(path["generics"]) == 1 and not node["args"]:
                return make("sizeof", loc, "u64", target=rtype(path["generics"][0]))
            if len(path["segments"]) != 1 or path["generics"]:
                raise Unsupported("未支持的 Rust 依赖调用路径", loc)
            name = path["segments"][0]
            if name in self.locals or name in self.parameters:
                raise Unsupported("同名局部变量遮蔽了依赖函数，不能按直接调用处理", loc)
            if name not in self.functions:
                raise Unsupported(f"缺少 Rust 依赖定义：{name}", loc)
            definition = self.functions[name]
            if len(definition["parameters"]) != len(node["args"]):
                raise Unsupported(f"Rust {name} 调用参数数量与定义不符", loc)
            args = [self.expr(x, p["type"]) for x, p in zip(node["args"], definition["parameters"])]
            for arg, parameter in zip(args, definition["parameters"]):
                a, b = arg["type"], parameter["type"]
                if a == b or (arg["op"] == "string" and b == "ptr(i8)") or (a.startswith("fn(") and b == "irq_handler_t"):
                    continue
                a_primitive = rtype(self.type_aliases[a]["type"]) if a in self.type_aliases else a
                b_primitive = rtype(self.type_aliases[b]["type"]) if b in self.type_aliases else b
                if a_primitive != b_primitive:
                    raise Unsupported(f"Rust {name} 实参类型 {a} 与 {b} 不符，不能自动补 C 的隐式转换", arg["loc"])
            return self.call(name, args, loc)
        if kind == "macro":
            path = node["path"]["segments"]
            if node.get("unsupported_arguments"):
                raise Unsupported("无法把 Rust 宏参数解析成 AST", loc)
            args = node["args"]
            if path in [["ptr", "addr_of"], ["ptr", "addr_of_mut"]] and len(args) == 1:
                self.note("addr-of-macro-to-C-address-of", loc)
                return self.address(self.expr(args[0]), loc)
            if len(path) != 1:
                raise Unsupported("未支持的 Rust 宏路径", loc)
            name = path[0]
            if name == "devm_netdev_alloc_pcpu_stats" and len(args) == 2:
                type_arg = args[1]
                if type_arg.get("kind") != "path" or type_arg["path"]["segments"] != ["pcpu_sw_netstats"]:
                    raise Unsupported("pcpu 统计类型参数不符", loc)
                return self.call(name, [self.expr(args[0]), make("type_argument", self.loc(args[1]), "type", name="pcpu_sw_netstats")], loc)
            if name in LOGS:
                return self.call(name, [self.expr(arg) for arg in args], loc)
            raise Unsupported(f"未支持的 Rust 宏 {name}", loc)
        raise Unsupported(f"未支持的 Rust 表达式 {kind}", loc)

    def statements(self, block, tail_returns=False):
        result = []
        statements = block["statements"]
        for index, node in enumerate(statements):
            loc = self.loc(node)
            kind = node["kind"]
            if node.get("attributes"):
                raise Unsupported("语句属性可能改变条件编译或行为，尚未支持", loc)
            if kind == "let":
                pattern = node["pattern"]
                if node.get("else"):
                    raise Unsupported("禁止用 let-else 隐藏原分支", loc)
                expected = None
                if pattern["kind"] == "typed_pattern":
                    expected, pattern = rtype(pattern["type"]), pattern["pattern"]
                if pattern["kind"] == "wildcard":
                    value = self.expr(node["init"])
                    if value["op"] != "symbol" or value["name"] not in self.parameters or not re.fullmatch(r"[iu](8|16|32|64)", value["type"]):
                        raise Unsupported("只允许丢弃未使用的标量入口参数", loc)
                    self.note("discard-unused-scalar-parameter", loc, name=value["name"])
                    continue
                if pattern["kind"] != "binding" or pattern.get("by_ref") or pattern.get("subpattern") or node["init"] is None:
                    raise Unsupported("未支持的 Rust 局部变量声明", loc)
                value = self.expr(node["init"], expected)
                name = pattern["name"]
                dtype = expected or value["type"]
                self.local(name, dtype, loc)
                result.append(make("assign", loc, left=symbol(name, dtype, loc), right=value))
                continue
            if kind not in {"expression_statement", "macro_statement"}:
                raise Unsupported("未支持的 Rust 语句 " + kind, loc)
            expression = node["expression"]
            if expression["kind"] == "label_block":
                label = expression["label"]
                if not label:
                    raise Unsupported("新增块作用域尚未证明不改变生命周期", loc)
                self.label_stack.append(label)
                result.extend(self.statements(expression["block"]))
                self.label_stack.pop()
                self.note("C-goto-label-to-Rust-labeled-break", loc, label=label)
                result.append(make("label", loc, name=label))
            elif expression["kind"] == "if":
                otherwise = expression["else"]
                if otherwise and (otherwise["kind"] != "label_block" or otherwise["label"]):
                    raise Unsupported("Rust else-if 需要单独显式映射", loc)
                result.append(make("if", loc, condition=self.truth(self.expr(expression["condition"]), loc),
                                   then=self.statements(expression["then"]),
                                   otherwise=self.statements(otherwise["block"]) if otherwise else []))
            elif expression["kind"] == "break":
                label = expression["label"]
                if label not in self.label_stack or expression["expression"] is not None:
                    raise Unsupported("只支持对应 C goto 的具名块 break", loc)
                result.append(make("goto", loc, label=label))
            elif expression["kind"] == "return":
                result.append(make("return", loc, expression=self.expr(expression["expression"], self.current_return) if expression["expression"] else None))
            else:
                value = self.expr(expression, self.current_return if tail_returns and index == len(statements)-1 and not node["semicolon"] else None)
                if tail_returns and index == len(statements)-1 and not node["semicolon"] and self.current_return != "void":
                    self.note("Rust-tail-expression-to-C-return", loc)
                    result.append(make("return", loc, expression=value))
                else:
                    result.append(value if value["op"] in {"assign", "update"} else make("evaluate", loc, expression=value))
        return result

    def function(self, node):
        signature = rust_signature(node)
        self.locals, self.addressed = {}, set()
        self.parameters = {p["name"]: p["type"] for p in signature["parameters"]}
        self.current_return = signature["returns"]
        attributes = [a for a in node["attributes"] if not a.startswith("# [doc")]
        if attributes or not node["signature"]["unsafe"] or node["signature"]["generics"] or node["signature"]["async"] or node["signature"]["variadic"] or node["signature"]["abi"]:
            raise Unsupported("入口出现未登记的属性/泛型/async/ABI", self.loc(node))
        body = self.statements(node["body"], tail_returns=True)
        return dict(parameters=[dict(p, loc=self.loc(p)) for p in signature["parameters"]],
                    returns=self.current_return, locals=dict(sorted(self.locals.items())), body=body, loc=self.loc(node))

    def globals(self):
        constants = {x["name"]: x for x in self.items if x["kind"] == "constant"}

        def initializer(node):
            kind, loc = node["kind"], self.loc(node)
            if kind == "reference":
                return initializer(node["expression"])
            if kind == "struct_value":
                if node["rest"] or len(node["path"]["segments"]) != 1 or node["path"]["generics"]:
                    raise Unsupported("未支持的 Rust 全局结构初始化", loc)
                return dict(kind="record", type=node["path"]["segments"][0],
                            fields={f["name"]: initializer(f["expression"]) for f in node["fields"]}, loc=loc)
            if kind == "array":
                return dict(kind="array", items=[initializer(x) for x in node["items"]], loc=loc)
            if kind in {"string", "c_string"}:
                return dict(kind="string", value=node["value"], loc=loc)
            if kind == "path" and len(node["path"]["segments"]) == 1 and not node["path"]["generics"]:
                name = node["path"]["segments"][0]
                if name in constants:
                    return initializer(constants[name]["expression"])
                return dict(kind="reference", name=name, loc=loc)
            raise Unsupported("未支持的 Rust 全局初始化器：" + kind, loc)

        result = {}
        for node in self.items:
            if node["kind"] == "static":
                if node["name"] not in {"liteeth_netdev_ops", "liteeth_of_match", "liteeth_driver"}:
                    raise Unsupported("新增全局存储尚未审核", self.loc(node))
                result[node["name"]] = initializer(node["expression"])
        registration, tables, metadata = [], [], []
        for item in self.items:
            if item["kind"] != "item_macro":
                continue
            node = item["macro"]
            if len(node["path"]["segments"]) != 1 or node.get("unsupported_arguments"):
                raise Unsupported("新增全局宏尚未支持", self.loc(item))
            name, args = node["path"]["segments"][0], node["args"]
            if name == "module_platform_driver" and len(args) == 1:
                value = initializer(args[0])
                if value["kind"] != "reference":
                    raise Unsupported("模块驱动参数不是对象引用", self.loc(item))
                registration.append(value["name"])
                self.note("Rust-module-macro-argument-only", self.loc(item), body_verified=False)
            elif name == "MODULE_DEVICE_TABLE" and len(args) == 2:
                a, b = initializer(args[0]), initializer(args[1])
                if a["kind"] != "reference" or b["kind"] != "reference":
                    raise Unsupported("设备匹配表不是具名引用", self.loc(item))
                tables.append(dict(bus=a["name"], table=b["name"]))
            elif name in {"MODULE_AUTHOR", "MODULE_DESCRIPTION", "MODULE_LICENSE"} and len(args) == 1:
                value = initializer(args[0])
                if value["kind"] != "string":
                    raise Unsupported("模块元数据不是字符串", self.loc(item))
                metadata.append(dict(key=name.removeprefix("MODULE_").lower(), value=value["value"]))
            else:
                raise Unsupported("未支持的模块级宏：" + name, self.loc(item))
        result.update(module_driver_binding=registration, device_table_binding=tables,
                      module_metadata=sorted(metadata, key=lambda x: x["key"]))
        return result
