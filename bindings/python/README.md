# `engram` — Python SDK

Native PyO3 embedded SQLite engine (`engram.open`) + zero-dependency HTTP REST/SSE client (`engram.EngramClient`) for **Engram (`mem`)**.

## 5-Line Quickstart (Embedded Single-File SQLite)

```python
import engram

mem = engram.open("memory.db")
mem.create_branch("exp/test", from_rev="main")
mem.write("/semantic/tip.md", "Use SQLite WAL mode.", branch="exp/test")
hits = mem.search("WAL", branch="exp/test")
```

## Remote Daemon (`mem-daemon`) Client

```python
from engram import EngramClient

client = EngramClient("http://127.0.0.1:7700", token="optional-secret")
client.write("/core/goal.md", "Ship reliable AI agents.", importance=1.0)
client.commit("update core goal")
print(client.compile_core()["rendered"])

for event in client.stream_events(branch="main"):
    print(event)
```

## Build from Source

```bash
# Option A: maturin (recommended)
pip install maturin
maturin develop --release -m bindings/python/Cargo.toml

# Option B: pure cargo + PYTHONPATH
cargo build --release --manifest-path bindings/python/Cargo.toml
cp bindings/python/target/release/lib_native.so bindings/python/python/engram/_native.so
PYTHONPATH=bindings/python/python python3 -m unittest discover -s bindings/python/tests -v
```
