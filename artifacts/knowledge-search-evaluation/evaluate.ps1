param([string]$ProjectRoot = (Resolve-Path "$PSScriptRoot/../..").Path)
$ErrorActionPreference = 'Stop'
$knowledgeRoot = Join-Path $ProjectRoot '.knowledge'
$indexPath = Join-Path $knowledgeRoot 'knowledge-index.md'
$indexRows = @(Get-Content -LiteralPath $indexPath -Encoding UTF8 | Where-Object { $_ -match '^\| \[' })
$documents = @($indexRows | ForEach-Object {
    if ($_ -match '\]\(([^)]+)\)') { Join-Path $knowledgeRoot $Matches[1] }
})
# 질문에서 뽑은 검색어를 고정한 소규모 점검이다. 실제 LLM의 선택률을 측정하지 않는다.
$cases = @(
    @{ question='새 버전을 배포할 때 무엇을 바꿔야 하나?'; pattern='배포|버전'; expected='배포-버전-갱신.md' },
    @{ question='토큰 사용량 단가는 어디에서 바꾸나?'; pattern='토큰|단가'; expected='토큰사용량-단가-갱신.md' },
    @{ question='세션을 재개하면 컨텍스트 사용량이 왜 0인가?'; pattern='재개|컨텍스트'; expected='컨텍스트-표시-복원.md' },
    @{ question='링크 뒤 문장이 늦게 나타나는 이유는?'; pattern='링크|늦게'; expected='스트리밍-링크-지연.md' },
    @{ question='한글이 다른 글자로 깨져 보이면 어떻게 진단하나?'; pattern='한글|깨져'; expected='한글-글리프-깨짐-진단.md' },
    @{ question='사용자 정의 역할을 추가할 때 주의점은?'; pattern='사용자 정의|역할'; expected='에이전트-역할-확장-주의.md' },
    @{ question='Claude SDK를 올릴 때 호환성을 어떻게 확인하나?'; pattern='Claude.*SDK'; expected='Claude-Agent-SDK-호환성-업데이트.md' },
    @{ question='비동기 질문에 답하지 않았는데 완료로 표시되는 이유는?'; pattern='비동기|질문|완료'; expected='Windows-호스트-통합-주의.md' },
    @{ question='한글 입력 뒤 영문이 덧붙는 현상은 왜 생기나?'; pattern='영문|덧붙'; expected='Windows-호스트-통합-주의.md' },
    @{ question='재배선 뒤 상태줄이 빈 채로 남는 이유는?'; pattern='재배선|상태줄'; expected='Windows-호스트-통합-주의.md' }
)
$results = @(foreach ($case in $cases) {
    $indexCandidates = @($indexRows | Where-Object { $_ -match $case.pattern } | ForEach-Object {
        if ($_ -match '\]\(([^)]+)\)') { $Matches[1] }
    })
    $raw = @(& rg -l -- $case.pattern @documents)
    if ($LASTEXITCODE -gt 1) { throw "rg 검색 실패: $($case.question)" }
    $bodyCandidates = @($raw | ForEach-Object { Split-Path $_ -Leaf })
    $evidence = @(& rg -n -m 1 -- $case.pattern (Join-Path $knowledgeRoot $case.expected))
    if ($LASTEXITCODE -gt 1) { throw "rg 근거 조회 실패: $($case.question)" }
    [pscustomobject]@{
        question=$case.question; pattern=$case.pattern; expected=$case.expected
        indexCandidates=$indexCandidates; indexFound=($indexCandidates -contains $case.expected)
        bodyCandidates=$bodyCandidates; bodyFound=($bodyCandidates -contains $case.expected)
        evidence=$evidence
    }
})
$report = [pscustomobject]@{
    documentCount=$documents.Count
    indexSha256=(Get-FileHash -LiteralPath $indexPath -Algorithm SHA256).Hash
    cases=$results
    indexFound=@($results | Where-Object indexFound).Count
    bodyFound=@($results | Where-Object bodyFound).Count
    scope='작성자가 구성한 고정 검색어의 후보 포함 여부. 순위·실제 제공자 준수율·임베딩 비교 아님.'
}
$report | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath "$PSScriptRoot/results-current.json" -Encoding utf8
$results | Select-Object question,indexFound,bodyFound,@{n='본문후보수';e={$_.bodyCandidates.Count}} | Format-Table -AutoSize
"문서=$($report.documentCount), 색인=$($report.indexFound)/$($cases.Count), 본문=$($report.bodyFound)/$($cases.Count)"
