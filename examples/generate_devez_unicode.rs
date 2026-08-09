use unicode_width::UnicodeWidthChar;

fn ranges(target: usize) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut end = 0;
    for codepoint in 0..=0x10ffff {
        let width = char::from_u32(codepoint)
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(0);
        if width == target {
            start.get_or_insert(codepoint);
            end = codepoint;
        } else if let Some(first) = start.take() {
            ranges.push((first, end));
        }
    }
    if let Some(first) = start {
        ranges.push((first, end));
    }
    ranges
}

fn javascript_ranges(ranges: &[(u32, u32)]) -> String {
    ranges
        .iter()
        .map(|(start, end)| format!("[{start},{end}]"))
        .collect::<Vec<_>>()
        .join(",")
}

fn main() {
    let version = unicode_width::UNICODE_VERSION;
    let zero = javascript_ranges(&ranges(0));
    let wide = javascript_ranges(&ranges(2));
    println!(
        "/* Generated from unicode-width 0.2.2 Unicode {}.{}.{} (MIT OR Apache-2.0). */\n!function(g){{const z=[{zero}],w=[{wide}];function h(c,r){{let l=0,u=r.length-1;while(l<=u){{const m=(l+u)>>1;if(c<r[m][0])u=m-1;else if(c>r[m][1])l=m+1;else return true}}return false}}function x(p){{return p>>1&3}}class A{{constructor(b){{this.b=b}}activate(t){{const v=this.b&&this.b._provider15Graphemes;if(!v)throw new Error('Unicode grapheme provider unavailable');const f=c=>h(c,z)?0:h(c,w)?2:1;t.unicode.register({{version:'devez-{}.{}.{}-graphemes',wcwidth:f,charProperties(c,p){{const a=v.charProperties(c,p),j=a&1,k=a>>3,s=f(c),o=x(p);let q=x(a);if(!j)q=s;else if(c===65038&&((p>>3)&15)===11)q=1;else q=Math.max(q,o,s);return(k<<3)|((q&3)<<1)|j}}}})}}dispose(){{}}}}g.DevezUnicodeAddon={{DevezUnicodeAddon:A}}}}(globalThis);",
        version.0, version.1, version.2, version.0, version.1, version.2
    );
}
