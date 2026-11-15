"""Refresh or verify the presenter deck's source excerpts and offline references."""
import argparse
import html
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DECK = ROOT / "workflow-orchestration/index.html"
# Start and end tokens select actual statements, including multi-line changes.
SNIPPETS = {
    "adapters/restate/src/product.rs": [("Ok(ctx", ".into_inner())", "async fn clock(")],
    "adapters/restate/src/product/commands.rs": [('if let Some(receipt)', "}", 'return Err(conflict("idempotency_key_reused"));')],
    "adapters/restate/src/product/dispatch.rs": [("let provider = ctx", ".send_after(Duration::from_millis(deadline));", None)],
    "adapters/restate/src/interactive.rs": [("let (awakeable_id, callback)", "ctx.awakeable::<Json<Callback>>();", None),
                                           ("restate_sdk::select! {", "}", "result = ctx.sleep(budget)")],
    "adapters/restate/src/checks.rs": [("tokio::time::timeout(", ".await", None)],
    "adapters/webhook-events/src/delivery.rs": [("let result = client", ".await;", None)],
}

def line_number(lines, token, start=0):
    return next(i for i in range(start, len(lines)) if token in lines[i])

def snippet(match):
    caption = match[1]
    path = re.search(r'href="\.\./([^"]+)"',caption)[1]
    lines = (ROOT / path).read_text().splitlines()
    parts, ranges = [], []
    for first, last, context in SNIPPETS[path]:
        start = line_number(lines,first,line_number(lines,context) if context == "async fn clock(" else 0)
        end_after = start
        if context and context != "async fn clock(":
            end_after = line_number(lines,context,start) + 1
        end = line_number(lines,last,end_after)
        segment = lines[start:end+1]
        indent = min(len(line)-len(line.lstrip()) for line in segment if line.strip())
        if parts: parts.append('<span class="cl omitted"><i>···</i><span>⋯</span></span>')
        for i, line in enumerate(segment,start+1):
            css = "cl hot" if any(x in line for x in (".run(", "ctx.get", ".send_after(", "awakeable", "provider.check(", "Idempotency-Key")) else "cl"
            parts.append(f'<span class="{css}"><i>{i}</i><span>{html.escape(line[indent:],quote=False)}</span></span>')
        ranges.append(str(start+1) if start == end else f'{start+1}–{end+1}')
    caption = re.sub(r'(?<=>)[0-9–, ]+ ↗(?=</a>)',', '.join(ranges)+' ↗',caption)
    return caption+'<pre><code>'+''.join(parts)+'</code></pre>'

def synchronize():
    original = DECK.read_text()
    body = re.sub(r'(<figure class="source">\s*<figcaption>.*?</figcaption>)\s*<pre><code>.*?</code></pre>',snippet,original,flags=re.S)
    start = '<script id="checkoutSources" type="application/json">'
    if start in body: body = re.sub(re.escape(start)+r'.*?</script>',start+'</script>',body,flags=re.S)
    # Include only files actually cited by the deck; no workspace or credentials lookup.
    paths = sorted(set(re.findall(r'["\']\.\./([\w./-]+)["\']',body)))
    files = {key:(ROOT/key).read_text() for key in paths}
    encoded = json.dumps(files, ensure_ascii=False, separators=(",",":")).replace("<",r"\u003c")
    return original,body.replace(start+'</script>',start+encoded+'</script>')

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write",action="store_true")
    args = parser.parse_args()
    original, updated = synchronize()
    if args.write: DECK.write_text(updated)
    elif updated != original: raise SystemExit("Deck citations have changed. Run python3 scripts/deck_sources.py --write and review the excerpts.")
