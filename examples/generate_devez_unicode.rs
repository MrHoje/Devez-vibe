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
        "/* Generated from unicode-width 0.2.2 Unicode {}.{}.{} (MIT OR Apache-2.0). */\n!function(g){{const z=[{zero}],w=[{wide}];function h(c,r){{let l=0,u=r.length-1;while(l<=u){{const m=(l+u)>>1;if(c<r[m][0])u=m-1;else if(c>r[m][1])l=m+1;else return true}}return false}}function x(p){{return p>>1&3}}class A{{activate(t){{t.unicode.register({{version:'devez-{}.{}.{}',wcwidth:c=>h(c,z)?0:h(c,w)?2:1,charProperties(c,p){{let q=this.wcwidth(c),j=q===0&&p!==0;if(j){{const v=x(p);if(v===0)j=false;else if(v>q)q=v}}return(q&3)<<1|(j?1:0)}}}})}}dispose(){{}}}}g.DevezUnicodeAddon={{DevezUnicodeAddon:A}}}}(globalThis);",
        version.0, version.1, version.2, version.0, version.1, version.2
    );
}
