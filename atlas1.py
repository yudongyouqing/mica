import io
p = 'crates/mica-render/src/font/dwrite.rs'
s = io.open(p, encoding='utf-8').read()
def rep(o, n, cnt=1):
    global s
    assert s.count(o) == cnt, (s.count(o), o[:60])
    s = s.replace(o, n)

rep('    atlas: GlyphAtlas,',
'''    /// 共享图集(Rc<RefCell>):终端路由与 strip UI 字体路由**共写同一
    /// 图集**——单次 draw 只绑一张纹理的硬约束(2026-10-12 共享图集
    /// 重构;此前每路由私有一份,strip 换 UI 字体必然两张图集)
    atlas: std::rc::Rc<std::cell::RefCell<GlyphAtlas>>,''')
rep('            atlas: GlyphAtlas::new(512, 512),',
    '            atlas: std::rc::Rc::new(std::cell::RefCell::new(GlyphAtlas::new(512, 512))),')
rep('        &self.atlas\n    }', '        &self.atlas.borrow()\n    }')
rep('        self.atlas.revision()', '        self.atlas.borrow().revision()')
rep('        self.atlas.version()', '        self.atlas.borrow().version()')
rep('        let rect = self.atlas.insert(&bmp);', '        let rect = self.atlas.borrow_mut().insert(&bmp);')
rep('            let rect = self.atlas.insert(&bmp);', '            let rect = self.atlas.borrow_mut().insert(&bmp);')
io.open(p, 'w', encoding='utf-8').write(s)
print('ok')
