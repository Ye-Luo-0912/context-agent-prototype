# -*- coding: utf-8 -*-
"""String-aware brace depth trace for cold_bounds.rs (temp diagnostic)."""
import io

p = 'crates/context-simple/src/tests/cold_bounds.rs'
s = io.open(p, encoding='utf-8', newline='').read()
lines = s.split('\n')

depth = 0
in_str = False
in_char = False
line_depth = []

for line in lines:
    line_depth.append(depth)
    i = 0
    while i < len(line):
        ch = line[i]
        if in_str:
            if ch == '\\':
                i += 2
                continue
            if ch == '"':
                in_str = False
        elif in_char:
            if ch == '\\':
                i += 2
                continue
            if ch == "'":
                in_char = False
        else:
            if line[i:i+2] == '//':
                break
            if ch == '"':
                in_str = True
            elif ch == "'":
                in_char = True
            elif ch == '{':
                depth += 1
            elif ch == '}':
                depth -= 1
        i += 1

for n, line in enumerate(lines, 1):
    if 285 <= n <= 312:
        print('L', n, 'depth', line_depth[n-1], '|', line[:60])
    if 'async fn' in line or '#[tokio::test]' in line or 'mod tests' in line:
        print('line', n, 'depth-before', line_depth[n-1], '|', line.strip()[:60])
print('EOF depth', depth)
