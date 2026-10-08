import io
p = 'crates/mica-core/tests/capture_claude.rs'
s = io.open(p, encoding='utf-8').read()
old = '''        if all.len() > 120_000 {
            break;
        }
    }'''
new = '''        if all.len() > 120_000 {
            break;
        }
        // 信任对话框(trust this folder)→ 回车,继续录欢迎页
        if all.windows(8).any(|w| w == b"trust th") && !sent_enter.get() {
            sent_enter.set(true);
            let _ = session.write(b"\r");
        }
    }'''
assert s.count(old) == 1
s = s.replace(old, new)
old2 = '    let mut answered = 0usize;'
new2 = '    let mut answered = 0usize;\n    let mut sent_enter = std::cell::Cell::new(false);'
assert s.count(old2) == 1
s = s.replace(old2, new2)
io.open(p, 'w', encoding='utf-8').write(s)
print('ok')
