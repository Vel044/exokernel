"""固定 Linux 驱动的代码 token；允许已有中文注释与排版差异。"""
import hashlib,re
TOKEN=re.compile(r'/\*.*?\*/|//[^\n]*|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\\n])\'|\w+|[^\s]',re.S)
CODE_HASH='8a529b65b903970915b08691ec9ee807c661887c2be9fb4a6c510d540f0dc74f'
def tokens(text):return [t for t in TOKEN.findall(text) if not t.startswith(('/*','//'))]
def code_hash(path):return hashlib.sha256(' '.join(tokens(path.read_text())).encode()).hexdigest()
def verify_source(path):
    actual=code_hash(path)
    if actual!=CODE_HASH:raise RuntimeError(f'Linux 驱动代码 token 已变化，需重新逐项审核：{path}: {actual}')
    return actual
