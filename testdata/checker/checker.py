import sys
from pathlib import Path


if len(sys.argv) != 4:
    print("expected: checker.py <input> <actual> <expected>", file=sys.stderr)
    raise SystemExit(2)

Path(sys.argv[1]).read_text(encoding="utf-8")
actual = int(Path(sys.argv[2]).read_text(encoding="utf-8").split()[0])
expected = int(Path(sys.argv[3]).read_text(encoding="utf-8").split()[0])

if abs(actual) == abs(expected):
    raise SystemExit(0)

print(f"|actual|={abs(actual)} differs from |expected|={abs(expected)}", file=sys.stderr)
raise SystemExit(1)
