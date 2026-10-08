import io
BS = chr(92)
p = 'crates/mica-core/tests/capture_claude.rs'
s = io.open(p, encoding='utf-8').read()
old = '            let _ = session.write(b"' + BS + 'r");'
new = ('            // win32 输入模式(9001h):Enter 须按记录格式\n'
       '            // ESC[<vk>;<sc>;<uc>;<kd>_ (vk=13, sc=28),down+up 成对\n'
       '            let _ = session.write(b"' + BS + 'x1b[13;28;13;1_' + BS + 'x1b[13;28;13;0_");')
assert s.count(old) == 1, s.count(old)
s = s.replace(old, new)
io.open(p, 'w', encoding='utf-8').write(s)
print('ok')
