"""Check local Markdown links and complete knowledge-index coverage."""
from pathlib import Path
import json
import re
import sys

root = Path(__file__).resolve().parents[2]
knowledge = root / '.knowledge'
documents = sorted(knowledge.rglob('*.md'))
index = knowledge / 'knowledge-index.md'
entries = re.findall(r'^\| \[([^\]]+)\]\(([^)]+)\)', index.read_text(encoding='utf-8-sig'), re.M)
indexed = [(knowledge / path).resolve() for _, path in entries]
errors = []
for document in documents:
    if document != index and indexed.count(document.resolve()) != 1:
        errors.append(f'색인 누락 또는 중복: {document.relative_to(root)}')
for title, path in entries:
    target = knowledge / path
    if not target.is_file():
        errors.append(f'색인 대상 없음: {path}')
    else:
        heading = re.search(r'^# (.+)$', target.read_text(encoding='utf-8-sig'), re.M)
        if not heading or title != heading[1]:
            errors.append(f'색인 제목 불일치: {path}')
checked = 0
scope = documents + sorted((root / 'docs').rglob('*.md')) + [root / 'CLAUDE.md', root / 'README.md']
for document in scope:
    text = document.read_text(encoding='utf-8-sig')
    # Fenced code contains example Markdown, not navigable links.
    text = re.sub(r'^```[^\n]*\n.*?^```\s*$', '', text, flags=re.M | re.S)
    for match in re.finditer(r'\[[^\]\n]*\]\(([^)\n]+)\)', text):
        target = match[1].strip('<>')
        if re.match(r'^[a-zA-Z][a-zA-Z0-9+.-]*:', target) or target.startswith('#'):
            continue
        path = target.split('#', 1)[0]
        if not path:
            continue
        checked += 1
        if not (document.parent / path).exists():
            errors.append(f'로컬 링크 없음: {document.relative_to(root)} -> {path}')
report = {'knowledgeDocuments': len(documents), 'indexedDocuments': len(entries),
          'localLinksChecked': checked, 'errors': errors,
          'scope': '마크다운 파일 링크·색인 제목·등록 범위. 외부 URL과 문서 내부 앵커는 제외.'}
print(json.dumps(report, ensure_ascii=False, indent=2))
sys.exit(bool(errors))
