# jrif (Python)

Read-path Python SDK for the JSON Range Index Format (JRIF) v0.

```python
import jrif

with open("data.json.jrif", "rb") as f:
    sidecar = f.read()

index = jrif.open(sidecar, payload="data.json")
root = index.root

# Operator-overloaded navigation: feels like a dict/list, but only fetches
# the byte ranges it actually needs.
name = root["records"][0]["name"].as_str()
count = len(root["records"])
for rec in root["records"]:
    print(rec["name"].as_str(), int(rec["id"]))
```

Navigation (`cursor[key]`, `cursor[ordinal]`, attribute access) is infallible
and does no I/O. Bytes are pulled only when a leaf accessor materializes a
value (`.value()`, `.bytes()`, `.as_str()`, `int(cursor)`, iteration, …).

See `docs/spec.md` for the format specification.
