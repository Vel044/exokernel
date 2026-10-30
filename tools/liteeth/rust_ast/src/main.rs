//! 将真实 syn AST 导出为 JSON；本层只解析，不替 Python 比较器判断相等。
//! 未支持的语法保留节点、位置及 token，比较范围内遇到它必须拒绝继续。
use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{self, Write},
    process,
};
use syn::{
    parse::Parser, punctuated::Punctuated, spanned::Spanned, Expr, Item, Pat, Stmt, Token, Type,
};

fn tokens<T: ToTokens>(t: &T) -> String {
    t.to_token_stream().to_string()
}
fn loc(span: Span) -> Value {
    let a = span.start();
    let b = span.end();
    json!({"line":a.line,"column":a.column+1,"end_line":b.line,"end_column":b.column+1})
}
fn node<T: Spanned>(t: &T, kind: &str, mut fields: Value) -> Value {
    fields["kind"] = json!(kind);
    fields["loc"] = loc(t.span());
    fields
}
fn unknown<T: Spanned + ToTokens>(t: &T) -> Value {
    node(t, "unsupported", json!({"tokens":tokens(t)}))
}
fn ty(t: &Type) -> String {
    tokens(t).replace(' ', "")
}
fn name(s: &str) -> String {
    s.strip_prefix("r#").unwrap_or(s).to_owned()
}
fn generic(args: &syn::PathArguments) -> Vec<String> {
    match args {
        syn::PathArguments::None => vec![],
        syn::PathArguments::AngleBracketed(a) => a.args.iter().map(tokens).collect(),
        other => vec![tokens(other)],
    }
}
fn path(p: &syn::Path) -> Value {
    json!({"segments":p.segments.iter().map(|s| name(&s.ident.to_string())).collect::<Vec<_>>(),
        "generics":p.segments.last().map(|s|generic(&s.arguments)).unwrap_or_default()})
}
fn pattern(p: &Pat) -> Value {
    match p {
        Pat::Ident(i) => node(
            p,
            "binding",
            json!({"name":name(&i.ident.to_string()),"mutable":i.mutability.is_some(),"by_ref":i.by_ref.is_some(),"subpattern":i.subpat.as_ref().map(|(_,p)|pattern(p))}),
        ),
        Pat::Wild(_) => node(p, "wildcard", json!({})),
        Pat::Type(t) => node(
            p,
            "typed_pattern",
            json!({"pattern":pattern(&t.pat),"type":ty(&t.ty)}),
        ),
        _ => unknown(p),
    }
}
fn macro_args(ts: TokenStream) -> Value {
    // 宏 token 用 syn 重新解析为参数 AST；不对整个宏做字符串替换。
    let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
    match parser.parse2(ts.clone()) {
        Ok(args) => json!({"args":args.iter().map(expr).collect::<Vec<_>>()}),
        Err(e) => json!({"unsupported_arguments":e.to_string(),"tokens":ts.to_string()}),
    }
}
fn mac(m: &syn::Macro) -> Value {
    let mut fields = macro_args(m.tokens.clone());
    fields["path"] = path(&m.path);
    node(m, "macro", fields)
}
fn block(b: &syn::Block) -> Value {
    node(
        b,
        "block",
        json!({"statements":b.stmts.iter().map(stmt).collect::<Vec<_>>()}),
    )
}
fn stmt(s: &Stmt) -> Value {
    match s {
        Stmt::Local(l) => node(
            l,
            "let",
            json!({"attributes":l.attrs.iter().map(tokens).collect::<Vec<_>>(),"pattern":pattern(&l.pat),"init":l.init.as_ref().map(|i|expr(&i.expr)),"else":l.init.as_ref().and_then(|i|i.diverge.as_ref().map(|(_,e)|expr(e)))}),
        ),
        Stmt::Expr(e, semi) => node(
            e,
            "expression_statement",
            json!({"expression":expr(e),"semicolon":semi.is_some()}),
        ),
        Stmt::Macro(m) => node(
            m,
            "macro_statement",
            json!({"attributes":m.attrs.iter().map(tokens).collect::<Vec<_>>(),"expression":mac(&m.mac),"semicolon":m.semi_token.is_some()}),
        ),
        Stmt::Item(i) => node(i, "item_statement", json!({"item":item(i)})),
    }
}
fn expr(e: &Expr) -> Value {
    let attrs: &[syn::Attribute] = match e {
        Expr::Path(x) => &x.attrs,
        Expr::Lit(x) => &x.attrs,
        Expr::Paren(x) => &x.attrs,
        Expr::Group(x) => &x.attrs,
        Expr::Binary(x) => &x.attrs,
        Expr::Unary(x) => &x.attrs,
        Expr::Assign(x) => &x.attrs,
        Expr::Field(x) => &x.attrs,
        Expr::Call(x) => &x.attrs,
        Expr::MethodCall(x) => &x.attrs,
        Expr::Cast(x) => &x.attrs,
        Expr::Reference(x) => &x.attrs,
        Expr::Return(x) => &x.attrs,
        Expr::If(x) => &x.attrs,
        Expr::Block(x) => &x.attrs,
        Expr::Break(x) => &x.attrs,
        Expr::Macro(x) => &x.attrs,
        Expr::Struct(x) => &x.attrs,
        Expr::Array(x) => &x.attrs,
        Expr::Tuple(x) => &x.attrs,
        Expr::Repeat(x) => &x.attrs,
        Expr::Index(x) => &x.attrs,
        _ => &[],
    };
    if !attrs.is_empty() {
        return node(
            e,
            "unsupported",
            json!({"reason":"表达式属性需要显式审核","attributes":attrs.iter().map(tokens).collect::<Vec<_>>(),"tokens":tokens(e)}),
        );
    }
    match e {
        Expr::Path(p) => node(
            e,
            "path",
            json!({"path":path(&p.path),"qself":p.qself.as_ref().map(|q|json!({"type":ty(&q.ty),"position":q.position}))}),
        ),
        Expr::Lit(l) => {
            let (kind, value, suffix) = match &l.lit {
                syn::Lit::Int(v) => (
                    "integer",
                    v.base10_digits().to_owned(),
                    v.suffix().to_owned(),
                ),
                syn::Lit::Str(v) => ("string", v.value(), String::new()),
                syn::Lit::CStr(v) => (
                    "c_string",
                    String::from_utf8_lossy(v.value().to_bytes()).into_owned(),
                    String::new(),
                ),
                syn::Lit::Bool(v) => ("boolean", v.value.to_string(), String::new()),
                _ => return unknown(e),
            };
            node(e, kind, json!({"value":value,"suffix":suffix}))
        }
        Expr::Paren(p) => node(e, "paren", json!({"expression":expr(&p.expr)})),
        Expr::Group(g) => node(e, "group", json!({"expression":expr(&g.expr)})),
        Expr::Binary(b) => node(
            e,
            "binary",
            json!({"operator":tokens(&b.op),"left":expr(&b.left),"right":expr(&b.right)}),
        ),
        Expr::Unary(u) => node(
            e,
            "unary",
            json!({"operator":tokens(&u.op),"expression":expr(&u.expr)}),
        ),
        Expr::Assign(a) => node(
            e,
            "assign",
            json!({"left":expr(&a.left),"right":expr(&a.right)}),
        ),
        Expr::Field(f) => node(
            e,
            "field",
            json!({"base":expr(&f.base),"field":tokens(&f.member)}),
        ),
        Expr::Call(c) => node(
            e,
            "call",
            json!({"callee":expr(&c.func),"args":c.args.iter().map(expr).collect::<Vec<_>>()}),
        ),
        Expr::MethodCall(m) => node(
            e,
            "method",
            json!({"receiver":expr(&m.receiver),"name":name(&m.method.to_string()),"generics":m.turbofish.as_ref().map(|g|g.args.iter().map(tokens).collect::<Vec<_>>()).unwrap_or_default(),"args":m.args.iter().map(expr).collect::<Vec<_>>()}),
        ),
        Expr::Cast(c) => node(
            e,
            "cast",
            json!({"expression":expr(&c.expr),"type":ty(&c.ty)}),
        ),
        Expr::Reference(r) => node(
            e,
            "reference",
            json!({"expression":expr(&r.expr),"mutable":r.mutability.is_some()}),
        ),
        Expr::Return(r) => node(
            e,
            "return",
            json!({"expression":r.expr.as_ref().map(|e|expr(e))}),
        ),
        Expr::If(i) => node(
            e,
            "if",
            json!({"condition":expr(&i.cond),"then":block(&i.then_branch),"else":i.else_branch.as_ref().map(|(_,e)|expr(e))}),
        ),
        Expr::Block(b) => node(
            e,
            "label_block",
            json!({"label":b.label.as_ref().map(|l|l.name.ident.to_string()),"block":block(&b.block)}),
        ),
        Expr::Break(b) => node(
            e,
            "break",
            json!({"label":b.label.as_ref().map(|l|l.ident.to_string()),"expression":b.expr.as_ref().map(|e|expr(e))}),
        ),
        Expr::Macro(m) => mac(&m.mac),
        Expr::Struct(s) => node(
            e,
            "struct_value",
            json!({"path":path(&s.path),"fields":s.fields.iter().map(|f|json!({"name":tokens(&f.member),"expression":expr(&f.expr),"loc":loc(f.span())})).collect::<Vec<_>>(),"rest":s.rest.as_ref().map(|e|expr(e))}),
        ),
        Expr::Array(a) => node(
            e,
            "array",
            json!({"items":a.elems.iter().map(expr).collect::<Vec<_>>()}),
        ),
        Expr::Tuple(t) => node(
            e,
            "tuple",
            json!({"items":t.elems.iter().map(expr).collect::<Vec<_>>()}),
        ),
        Expr::Repeat(r) => node(
            e,
            "repeat",
            json!({"expression":expr(&r.expr),"count":expr(&r.len)}),
        ),
        Expr::Index(i) => node(
            e,
            "index",
            json!({"expression":expr(&i.expr),"index":expr(&i.index)}),
        ),
        _ => unknown(e),
    }
}
fn signature(s: &syn::Signature) -> Value {
    let params = s
        .inputs
        .iter()
        .map(|a| match a {
            syn::FnArg::Typed(t) => {
            json!({"pattern":pattern(&t.pat),"type":ty(&t.ty),"attributes":t.attrs.iter().map(tokens).collect::<Vec<_>>(),"loc":loc(t.span())})
            }
            syn::FnArg::Receiver(r) => json!({"receiver":tokens(r),"loc":loc(r.span())}),
        })
        .collect::<Vec<_>>();
    let ret = match &s.output {
        syn::ReturnType::Default => "()".to_owned(),
        syn::ReturnType::Type(_, t) => ty(t),
    };
    json!({"name":name(&s.ident.to_string()),"parameters":params,"return":ret,"unsafe":s.unsafety.is_some(),"async":s.asyncness.is_some(),"generics":tokens(&s.generics),"variadic":s.variadic.is_some(),"abi":s.abi.as_ref().map(tokens)})
}
fn imports(tree: &syn::UseTree, prefix: Vec<String>) -> Vec<Value> {
    match tree {
        syn::UseTree::Path(p) => {
            let mut path = prefix;
            path.push(p.ident.to_string());
            imports(&p.tree, path)
        }
        syn::UseTree::Name(n) => {
            let mut path = prefix;
            path.push(n.ident.to_string());
            vec![json!({"path":path,"alias":null,"glob":false})]
        }
        syn::UseTree::Rename(n) => {
            let mut path = prefix;
            path.push(n.ident.to_string());
            vec![json!({"path":path,"alias":n.rename.to_string(),"glob":false})]
        }
        syn::UseTree::Glob(_) => vec![json!({"path":prefix,"alias":null,"glob":true})],
        syn::UseTree::Group(g) => g
            .items
            .iter()
            .flat_map(|t| imports(t, prefix.clone()))
            .collect(),
    }
}
fn item(i: &Item) -> Value {
    match i {
        Item::Fn(f) => node(
            f,
            "function",
            json!({"signature":signature(&f.sig),"body":block(&f.block),"attributes":f.attrs.iter().map(tokens).collect::<Vec<_>>()}),
        ),
        Item::Struct(s) => node(
            s,
            "struct",
            json!({"name":s.ident.to_string(),"attributes":s.attrs.iter().map(tokens).collect::<Vec<_>>(),"fields":s.fields.iter().map(|f|json!({"name":f.ident.as_ref().map(|i|name(&i.to_string())),"type":ty(&f.ty),"attributes":f.attrs.iter().map(tokens).collect::<Vec<_>>(),"loc":loc(f.span())})).collect::<Vec<_>>()}),
        ),
        Item::Const(c) => node(
            c,
            "constant",
            json!({"name":c.ident.to_string(),"type":ty(&c.ty),"attributes":c.attrs.iter().map(tokens).collect::<Vec<_>>(),"expression":expr(&c.expr)}),
        ),
        Item::Static(s) => node(
            s,
            "static",
            json!({"name":s.ident.to_string(),"type":ty(&s.ty),"attributes":s.attrs.iter().map(tokens).collect::<Vec<_>>(),"expression":expr(&s.expr)}),
        ),
        Item::Macro(m) => node(
            m,
            "item_macro",
            json!({"name":m.ident.as_ref().map(ToString::to_string),"attributes":m.attrs.iter().map(tokens).collect::<Vec<_>>(),"macro":mac(&m.mac),"definition_tokens":m.mac.tokens.to_string()}),
        ),
        Item::Type(t) => node(
            t,
            "type_alias",
            json!({"name":t.ident.to_string(),"type":ty(&t.ty),"attributes":t.attrs.iter().map(tokens).collect::<Vec<_>>()}),
        ),
        Item::Use(u) => node(
            u,
            "use",
            json!({"imports":imports(&u.tree,vec![]),"attributes":u.attrs.iter().map(tokens).collect::<Vec<_>>()}),
        ),
        // use/impl 等仍在原始 syn 导出中留痕；比较器不得把范围内的未知表达式视为成功。
        _ => node(i, "other_item", json!({"tokens":tokens(i)})),
    }
}
fn main() {
    let result = (|| -> Result<Value, Box<dyn std::error::Error>> {
        let mut files = Vec::new();
        for filename in env::args().skip(1) {
            let source = fs::read_to_string(&filename)?;
            let parsed = syn::parse_file(&source).map_err(|e| {
                let pos = e.span().start();
                format!("{}:{}:{}: {}", filename, pos.line, pos.column + 1, e)
            })?;
            files.push(
                json!({"path":filename,"attributes":parsed.attrs.iter().map(tokens).collect::<Vec<_>>(),"items":parsed.items.iter().map(item).collect::<Vec<_>>()}),
            );
        }
        if files.is_empty() {
            return Err("需要至少一个 Rust 源码路径".into());
        }
        Ok(json!({"frontend":"syn","version":"2.0.119","typechecked":false,"files":files}))
    })();
    match result {
        Ok(v) => {
            serde_json::to_writer(io::stdout().lock(), &v).unwrap();
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "Rust AST 解析失败：{e}");
            process::exit(2);
        }
    }
}
