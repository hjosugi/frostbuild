# ビルドキャッシュ研究メモ v4（統合版）

最終更新: 2026-07-20
統合元: (a) v1メモ + ソース直読検証ログ, (b) ChatGPT deep research v2「一次資料検証版」,
(c) Lua予備実験, (d) URL実地検証, (e) **v4で追加: DeltaCDC本体の実測（大artifact・
chunk-level residual delta）、V4先行研究網羅調査、CPU実測、selector ablation**

**このv4が唯一の真実源。** v3からの変更点は§17に集約。要点:

- **P0の3件すべてが完了**（大artifact residual delta / 先行研究網羅調査 / CPU実測）
- **中心的発見: artifactサイズで結論が反転し、DeltaCDCだけが両領域で勝つ**（§10.1）
- **v3の§10.0発見#4「delta形式の選択が本質・zstdはaddress shiftを追えない」は誤り**。
  zstdをcache圧縮レベル(3)で測ったことによる測定アーティファクトだった（§17.1）
- **sketch索引は不要**という強いnegative result（§10.2）。提案のprotocol surfaceが大幅に縮小

## 0. 結論

| 問い | 結論 | 論文上の扱い |
|---|---|---|
| Snowflakeの原理はビルドへ応用できるか | できる。優先順: 実行actionを減らす → 転送byteを減らす → 残りをcritical path優先で並列化 | 研究全体の設計原理 |
| 最新のtreeアルゴリズムだけで速くなるか | ならない。ビルドはDAGで、全走査ならO(V+E)が下界。走査対象の削減が主戦場 | 背景 |
| hashをvectorへ置換できるか | 不可。cache hitの正しさは等価関係を要求。vector/sketchは候補選択・prefetch・eviction・schedulingのみ | 中心的な安全原則 |
| hashを多次元化できるか | 可。次元別exact hash + edgeのread setで健全な部分無効化。ただしABI分離・red-green・DICEが既存 | 別論文候補 |
| 関数型は使えるか | 基盤そのもの。純粋性・依存追跡・memoization・demand-driven増分計算 | 新規性ではなく基礎 |
| 量子は使えるか | schedulingのQUBO化は可能だが、build advantageの証拠なし。比較対象に限定 | related work 1段落 |
| verified delta transportは新規か | 広い形では新規でない。REAPI v2.12.0がSplitBlob/SpliceBlobでCDC + digest検証を標準化済（検証済） | 旧中心命題を撤回 |
| 何に新規性が残るか | **CDCでmissになったchunkを類似baseからbinary deltaで再構成し、chunkとblobのdigestで検証する「DeltaCDC」** + cost-aware base選択 + 実workload評価 | 本命。予備実験は支持（§10.0） |

中心命題（v3確定版）:

> Exact digestをvectorへ置き換えるのではない。exact digestの下でCDCによる
> exact chunk再利用を行い、さらに一致しないchunkだけを類似baseからdelta再構成する。
> 候補選択は距離が行い、正しさは各chunkと最終blobのdigest検証が守る。

## 1. 状態タグ

| タグ | 意味 |
|---|---|
| [仕様確認] | 標準仕様または査読論文の本文を直接確認 |
| [実装確認] | 現行の公式source/公式docを直接確認（commit hash付きは検証ログ参照） |
| [運用報告] | ベンダー自身のengineering report。一次的だが査読なし |
| [運用報告・実読済] | 上記のうち、本文を実際に読んで数値と条件を確認したもの |
| [新規仮説] | 提案。網羅調査または実験が未完 |
| [要追試] | 公開値はあるが独立再現が必要 |
| [予備実験済] | 本リポジトリのharnessで測定済（results/参照） |
| [棄却] | 本論の中心には置かない |

## 2. Snowflake原理のビルドへの写像

| 分析DB | ビルドシステム | 消える仕事 |
|---|---|---|
| partition pruning | 変更影響範囲・dependency pruning・early cutoff | 実行不要なaction |
| column pruning | ABI/interface projection・query粒度fingerprint | 読まれない入力次元 |
| result cache | action cache | action実行全体 |
| micro-partition / 圧縮列 | CAS blob / CDC chunk | 不要な転送byte |
| cost-based optimizer | execution-vs-download・base選択 | 遅いmaterialization plan |
| MPP execution | local/remote execution | 残存actionの実行時間 |

優先順位: (1) 到達しないnodeを評価しない (2) 変わっていないactionを再実行しない
(3) 出力が同じなら伝播を止める (4) 既知artifactを丸ごと再転送しない
(5) 残りをcritical pathと資源制約で並列化。

DAG補足: Build Systems à la Carte はscheduler×rebuilderで設計空間を形式化し
early cutoffを扱う。Buck2 DICEはcomputation依存を追跡。Rust red-greenは
query fingerprintで再計算伝播を抑える。Bazel Skyframeは "change pruning" として
early cutoffを実装している（InvalidatingNodeVisitor.java 等、[実装確認] bazel@3cdd083）。

## 3. 2026年の前提: REAPIのCDC標準化

### 3.1 Action CacheとCASを分ける

```
Action digest a → Action Cache lookup → ActionResult R → output digest d は既知
  → CASからbytesをmaterialize（←ここが本研究の対象）
```

- AC hit = actionを再実行しなくてよい
- CAS materialization = 既知digestのbytesをどの経路で得るか（別問題）
- 真のAC missでは出力digestが未知。既存artifactから最終出力を「作った」と
  主張するにはactionを実行するか、compiler固有の健全なincremental protocolが必要

旧v1メモの「lookup全体をcost最小化へ一般化」は粗すぎた。正確な対象は
**既知digestに対するmaterialization plan最適化**。

### 3.2 SplitBlob / SpliceBlob [仕様確認・実装確認]

- REAPI PR #282（2025-07-09 merge, Sascha Roloff）で導入。**release v2.12.0 に含まれることを
  ローカルgit tagで確認済**（`git tag --contains 9ef19c6` → v2.12.0）
- 後続: #337, #353, #357（ChunkingFunction enum: FAST_CDC_2020 / REP_MAX_CDC）
- SpliceBlobは期待digest必須、サーバがsplice結果のdigest一致を検証
- Bazel実装: `--experimental_remote_cache_chunking`、`FastCdcChunker.java`、
  `ChunkedBlobDownloader/Uploader.java`。Downloaderは再構成後にclient側digest検証
  （bazel@3cdd083で確認）
- パラメータ既定: avg 512 KiB / min 128 KiB / max 2 MiB / normalization 2 / seed 0
- 進行中の変化: streaming版Split/Spliceの提案（remote-apis PR #377）、
  RepMaxCDCのBazel実装PR #30131 — **protocolはまだ動いている。追跡必須**

### 3.3 運用証拠

| 報告 | 条件 | 結果 | 状態 |
|---|---|---|---|
| EngFlow (2026-07-15, Armando Montanez) | LLVM toolchain（hermetic-llvm）、Bazel 9.1.1、FastCDC2020、EngFlowクラスタ | download_outputs=all + disk cacheで**baseline比58%減、disk-cache-only比36%減**。初回buildのupload削減は6%のみ、**LLVM version bump後のCAS uploadで50%削減**。disk_cacheなしではCDCは効果なし（chunkのlocal cacheが前提）。minimalでは効果小 | [運用報告・実読済] |
| BuildBuddy (2026-05-01) | BuildBuddy repo、production window | upload 40%減、eligible write bytesの約85%をdedup、2週間で約300 TiB回避、2 MiB超はあるtestでobjectの約4.2% | [運用報告]（本文未読、要確認） |

EngFlow記事から得た重要な条件依存性:
1. **CDCはlocal chunk cacheがないと効かない**（disk_cacheなしでは同量を別RPCでDLするだけ）
2. **minimal download政策では効果が小さい**
3. 効果はartifactの冗長性に依存し、保証された勝ちではない

### 3.4 正しさは仕様だけでは成立しない [実装確認]

**2つの独立したインシデントを確認した（両方実在）:**

1. **Bazel 9.1.0は再構成blobのdigest検証を欠いたまま出荷された。** EngFlowが明記:
   9.1.0はCDC有効での使用不可、9.1.1のfix（PR #29593）でdigest validationが入った
2. **Bazel issue #29544**（2026-05-15, tyler-french, closed）: 9.1.0で
   chunking + disk_cache + toplevel downloadの組合せが、**大きな出力を単一CDC chunkに
   切り詰めたままbuild成功を報告**。実行ファイルはExec format errorに。
   根本原因: chunkごとのdownloadに最終出力のpath-backed streamを渡しており、
   disk cacheのpath fast-path（CAS objectを直接pathへcopy）が、chunk個別DLの度に
   最終出力を上書きした。fix: PR #29614

教訓（H5の実例として最強）:
- digest verificationを設計に書くだけでは不十分。**全materialization経路**で
  実行されることをfailure injectionで試験する必要がある
- 「誤予測は時間だけを失う」は、検証実装が全経路で走る場合に限る
- 我々のDeltaCDC設計では、chunk単位とblob単位の二重検証 + fallbackを
  型で強制する（§6.4）

## 4. 本命: DeltaCDC

### 4.1 既存CDCの境界

```
old blob: [A][B ][C][D]
new blob: [A][B'][C][D]

CDC reuse:     A, C, D
full transfer: B'          ← ここにdeltaを入れる

missing B' → candidate base選択 → min(delta(B→B'), full(B'))
          → reconstruct → H(B')検証 → 連結後 H(blob)検証
```

**chunk-level deltaの構造的利点（v3で明確化）:** whole-blob bsdiffはsuffix sortの
CPU・メモリコストがblobサイズに従い増大するが、chunk単位（≤ max chunk 2 MiB）に
限定すればコストが有界化し、chunkごとに並列化できる。予備実験で未測定だった
CPUコスト問題への構造的回答になっている。

### 4.2 形式化

Action digest a、R = AC[a]、要求output digest d、local store L、remote CAS S。

```
π*(d,L,S) = argmin_{π ∈ Π(d,L,S)} Ĵ(π)
制約: H(Materialize(π)) = d

J(π) = α·B_net + β·T_cpu + γ·N_rpc + η·B_store + ρ·P_fallback

missing chunk c, candidate bases B(c):
b* = argmin_{b ∈ B(c) ∪ {∅}} (C_lookup + C_transfer + C_patch + C_verify)
```

- b = c: exact chunk hit（cost 0 / local read）
- b ≠ c: delta転送 + patch
- b = ∅: full transfer
- 予測costが悪ければfull transferへfallback

### 4.3 健全性命題の草案

**命題。** 衝突耐性hash H、正しいpatch interpreter、全chunkと完成blobへのdigest検証、
検証失敗時のfull-transfer fallbackを仮定する。このときcandidate selectorが任意の
baseを選んでも、成功として返るblobは期待digest dを持つ。selectorの誤りは
余分なlookup・delta生成・fallback時間を生むが、未検証blobの受理を生まない。

注意: (a) 誤hit率ゼロではなく衝突無視可能の仮定下の健全性 (b) non-hermetic actionの
AC健全性は別問題 (c) 実装bug・digest-check bypass・TOCTOU・cache poisoningは
形式仮定の外（§3.4の2件が実例）。試験とthreat modelが必要。

### 4.4 既知成果と残る差分

| 要素 | 既知 | 残る差分 |
|---|---|---|
| rolling hash / CDC | rsync, LBFS, FastCDC, REAPI Split/Splice | algorithm/parameter/thresholdのbuild artifact比較 |
| exact chunk reuse | REAPI v2.12.0, Bazel, BuildBuddy, EngFlow | 差分なし |
| binary delta | Git pack, bsdiff, xdelta系, Ddelta | 差分なし |
| executable-aware transform | Courgette | 差分なし |
| similarity search | LSH, MinHash, SimHash, dedup文献 | 差分なし |
| **CDC miss chunkへのverified delta** | 現行protoに該当RPC/fieldなし（proto全文grepで確認） | **本論。統合protocol・cost-aware routing・実workload評価** |

### 4.5 実装上の難所

1. base発見: path / target / 直前version / content sketch / server-side index
2. 双方向性: DLはserverがtarget・clientがbase、ULは逆。base availability交渉が必要
3. compressed artifact（ZIP/JAR）: 小変更でbyte差分が拡大。decompress-normalize-repackは
   署名・再現性を壊しうる
4. CPU対network: fast networkではdelta計算が損。cost modelにencode/decode時間を含める
5. delta chain: depth 1を原則（read amplificationと欠損伝播の抑制）
6. multi-tenancy: cross-tenant dedupは内容存在のtiming漏洩リスク。tenant境界越えbase選択は既定禁止
7. eviction: base TTLとmappingの整合。missing baseは通常経路へfallback

## 5. Hash、vector、多次元hash

### 5.1 三種類を混ぜない

| 表現 | 数学的役割 | 用途 | 誤り許容 |
|---|---|---|---|
| exact digest | 等価性の強い代理 | CAS identity・verification | 許容しない |
| dimension-wise exact hashes | 部分等価 | dependency pruning・ABI/impl分離 | read宣言が正しければ健全 |
| sketch / embedding | 距離・順位 | base選択・prefetch・eviction・scheduling | fallback時間に限定 |

### 5.2 健全な多次元化（別論文候補）

```
H(module) = { api, impl, res, tool }   （各次元exact hash）
edge(consumer → module).reads = {api, tool}
invalidate(consumer) ⇔ changed_dims ∩ reads ≠ ∅
```

既存: Bazel ijar（README確認済: method code・privateメンバ除去のinterface jar）、
Rust red-green、Buck2 DICE、ccache（**注意: direct / preprocessor / depend modesが
正確な仕様。「whitespace/comment除去のsemantic hash」という要約は不正確** — v2の指摘を採用）。

自動次元分割の定式化:
```
min_{P,R} E[invalidated work(P,R)] + λ·tracking overhead(P,R) + μ·unsoundness risk(P,R)
```
学習元はcommit history・query trace・depfile・ABI trace。学習結果は直接信頼せず、
静的解析またはinstrumented read traceで保守的にover-approximateする。
**DeltaCDCとは独立の研究。同一論文に詰め込まない。**

### 5.3 過剰主張の修正（v2の指摘を採用）

- 「semantic hashはRiceで計算不能」と一言で済ませない。限定言語・型・ABI・正規化・
  安全な近似では計算可能な領域がある
- LtHashをMerkle DAGの直接代替としない（set hashと順序付き構造digestは別物）→ 保留
- NCDはoracle分析のみに降格（高コストで実delta costとずれる）
- 「古典solverは数千taskを必ずmsで解く」→ 撤回（出典なし一般化）

## 6. 関数型プログラミング

### 6.1 結論

関数型は追加の魔法ではなく、再利用可能性と増分性を説明する意味論。ただし
純関数だけでcache健全性は得られない:

```
deterministic action + complete explicit inputs + hermetic env/toolchain
+ platform/config capture + canonical serialization + collision-resistant digest
```

H(function, inputs)はaction identityを作るが、**実行せずにoutput digestを予言しない**。

### 6.2 確立済の貢献（新規性ではない）

| 概念 | 効果 | 先行研究 |
|---|---|---|
| pure/deterministic transformation | memoizationとCAS再利用の前提 | Nix thesis (2006) |
| applicative task | 静的dependency graph | Build Systems à la Carte |
| monadic task | dynamic dependency | 同上、Shake (ICFP 2012) |
| demand-driven増分計算 | 必要な結果だけ再評価 | Adapton (PLDI 2014) |
| from-scratch consistency | 増分結果 = scratch再計算 | Adapton系（**証明構造を借りる**） |
| change structure / derivative | 入力差分→出力差分 | ILC (PLDI 2014) |
| 代数的incrementalization | 差分計算の統一 | DBSP (PVLDB 2023) |

### 6.3 DeltaCDCでの関数型の役割

```
Pure core:      split(blob) → chunks / patch(base, delta) → candidate
                verify(expected, candidate) → VerifiedBlob | Failure
Effectful policy: discoverBases / estimateNetwork / choosePlan / fetch / fallback
```

型で UnverifiedBytes と VerifiedBlob を分け、CAS登録をVerifiedBlobに限定すれば
digest-check bypass（#29544型のbug）を型レベルで防げる。型安全性を主貢献にするなら
formal model + mechanized proofが別途必要。

## 7. 量子プログラミング

- 使えない場所: DAG traversal（線形時間問題、入力読み出しが下界）、hashing、
  compilation/linking/patch、cache correctness
- 可能性: resource-constrained scheduling のQUBO/CQM化。ただし変数爆発と
  online状態変化（cache hit・worker availability）でoffline scheduleがすぐ古くなる
- 文献: Trummer & Koch PVLDB 2016は**MQOでありjoin orderingではない**。
  join orderingは Ready to Leap (2023)、hybrid optimizerは PVLDB 2025。
  JSSP×D-Wave: Scientific Reports 2022
- 判断: build advantageの証拠なし。量子は差し替え可能なpolicy plug-inとしてのみ設計し、
  related work 1段落 + small offline comparisonまで

## 8. Related work 対照表

| 主張・部品 | 主要先行研究 | 差分判定 |
|---|---|---|
| build = scheduler × rebuilder | Build Systems à la Carte | 完全に既出 |
| monadic dynamic dependency | Shake、à la Carte | 完全に既出 |
| SAC / demand-driven増分 | Acar, Adapton, ILC, DBSP | 完全に既出。証明方法を借りる |
| computation粒度invalidation | Rust red-green, Buck2 DICE | 完全に既出 |
| ABI/impl分離 | Bazel ijar, Buck Java ABI | 完全に既出 |
| CDC | rsync, LBFS, FastCDC, RepMaxCDC | 完全に既出 |
| build outputのexact-chunk CDC | REAPI v2.12.0, Bazel, BuildBuddy, EngFlow | 2026年に標準・実装が成立（検証済） |
| binary delta | Git pack, bsdiff, xdelta, Ddelta | 完全に既出 |
| format-aware executable delta | Courgette | 完全に既出 |
| similarity index | LSH, SimHash, MinHash, dedup | 一般技術として既出 |
| **CDC miss chunkへのverified delta** | 現行標準に未確認 | **[新規仮説][予備実験済(部分)]** |
| history-aware cost selector | storage/delta文献に類似多数 | build固有objective + E2E実証が差分候補 |
| 自動semantic次元発見 | compiler/incremental文献に隣接 | [新規仮説]。別論文 |
| quantum build scheduling | JSSP×QUBO隣接研究あり | build固有新規性は弱い |

## 9. Research questions

- **RQ1**: CDCの後にも利用可能な類似性が残るか
  **H1**: CDC exact reuse後のmissing chunkのうち、deltaがfull chunkの50%未満になる
  chunkが有意な割合で存在する → **§10.0で部分的に支持済（小artifact領域）**
- **RQ2**: selectorはoracleへ近づけるか
  **H2**: 直前version / sketch / ANNの少なくとも1つがoracle best-deltaの90%以内
- **RQ3**: E2Eで速くなるか
  **H3**: 帯域×RTT×entropyの一定領域でp50/p95 materialization timeとbytesを削減
  （bytesが減ってもwall timeが悪化するならstorage optimizationに主張を限定）
- **RQ4**: artifact形式で効果は違うか
  **H4**: linker output・非圧縮archive・container layerで高く、圧縮済bundle・
  nondeterministic binaryで低い
- **RQ5**: 正しさと障害耐性
  **H5**: corruption・missing base・eviction・partial RPC・wrong neighbor注入下でも
  誤blobを成功として返さない（#29544が「返した」実例 → 動機として引用）

## 10. 実験計画

### 10.0 予備実験の結果（2026-07-19 実施済）[予備実験済]

詳細: `results/SUMMARY.md`、raw: `results/*.json`。harness: BazelのFastCdcChunker.javaを
GEARテーブルごとbit-compatibleに移植（`harness/fastcdc.py`）。

対象: lua/lua 連続40コミット × 2構成、成果物36個/コミット。測定対象は
「前バージョンが手元にあるcache miss」のみ。

| corpus | FastCDC avg | misses | 1-chunk率 | zstd(MB) | cdc+zstd(MB) | bsdiff(MB) | bsdiff/cdc+zstd |
|---|---|---|---|---|---|---|---|
| lua -O2 | 512 KiB (Bazel既定) | 113 | 100.0% | 8.08 | 8.08 | 0.29 | **3.6%** |
| lua -O2 | 16 KiB | 113 | 27.4% | 8.08 | 3.74 | 0.29 | **7.8%** |
| lua -g  | 512 KiB (Bazel既定) | 192 | 67.2% | 26.34 | 21.47 | 0.33 | **1.5%** |
| lua -g  | 16 KiB | 192 | 7.3% | 26.34 | 10.75 | 0.33 | **3.0%** |

読み取り:
1. **Bazel既定パラメータ（min 128 KiB）では、小さいリリースbuild成果物にCDCが完全に無効**。
   全missが1チャンク = CDC転送量はraw転送と同一。**これ自体がPaper Aの発見**
   （BuildBuddyも2 MiB超はobjectの約4.2%と述べており、artifact size分布は本質的）
2. CDCに有利なパラメータ（avg 16 KiB）でもbsdiffが1桁以上優位
3. debug buildで差が最大（1.5%）。debug情報のoffset/行番号が全域に散り、
   chunk完全一致前提のCDCが最も苦手とする変化パターン
4. **delta形式の選択が本質**: lua-g の .a で zdelta 4.14 MB vs bsdiff 0.051 MB（約80倍）。
   zstd dictionary matchは大きなaddress shiftを追えず、suffix sortベースのbsdiffが吸収

**v2のRQ1 go/no-go（oracle +10%以上の削減がなければ中止）への回答: 小artifact領域では
大幅にクリア（削減92〜98%）。GO。**

粒度の注意: この測定はwhole-blob delta。1-chunk blob（≤2 MiB）ではwhole-blob delta =
chunk-level deltaなので、**小artifact領域のDeltaCDCの直接的証拠**になっている。
一方、EngFlowが示す通り**LLVM規模のartifactではCDCは実際に効く**（version bump後の
CAS upload 50%削減）。したがって残るPhase Aの焦点は:
**大きいartifact（マルチchunk blob）でのchunk-level residual delta測定**。

未測定: CPU時間（bsdiffのsuffix sortコスト）。ただしchunk-level化で構造的に有界化される
（§4.1）。それでも実測は必須。

### 10.0b 大artifactでのDeltaCDC実測（2026-07-20 実施済）[予備実験済]

詳細: `results/DELTACDC.md`、raw: `results/deltacdc/rg-debug_avg512.json`。
harness: `harness/deltacdc.py`（chunk-level residual delta を測る本命ハーネス）。

対象: ripgrep 連続25コミット、debug binary 約50MB。**全blobがマルチchunk**
（1-chunk率 0.0）で、Luaでは測れなかった「CDCが本来効く領域」。
24 miss / 残余1403 chunk。Bazel既定パラメータ。

| scheme | MB | CDC比 |
|---|---|---|
| CDC + zstd（Bazel今日） | 196.5 | 100% |
| DeltaCDC, sketch選択 | 155.0-157.0 | 79% |
| **DeltaCDC, positional選択** | **93.2-99.9** | **47-51%** |
| whole-blob delta | 77.4 (bsdiff) / 164.2 (zstd) | 39% / 84% |

全1403件の再構成をdigest検証、不一致ゼロ。

**RQ1/H1 に対する回答: 大artifact領域でもGO。** CDCの約半分まで落ちる。

**§4.5-1「base発見」の結論: sketch索引は不要。** super-feature sketchは1403中
549 chunkでしかbaseを見つけられず、両者が候補を持つ場合でもpositional selector
（同一artifactの前バージョンでbyte範囲が最も重なるchunk）に負けた（79% vs 51%）。
build graphの局所性が資料的類似性に勝つ。protocolのbase指定は
(artifact, 前バージョン) で足り、clientが既に持つ情報で表現できる。

**§10.0発見#4の撤回が必要:** 「delta形式の選択が本質、zstdはaddress shiftを
追えない」は、zstdをcache圧縮レベル(3)で測ったアーティファクトだった。
chunk-levelでは zstd --patch-from (47.5%) が bsdiff (50.8%) を上回る。
REAPIは既にzstdを持つので、transportに新しいcompressorは要らない。

**chunk-level化の根拠も訂正:** このcorpusでは whole-blob delta (39%) の方が
chunk-level (47-51%) より転送量が少ない。3コミット分の切片で「whole-blobが2倍
負ける」と読めたのは、baseを持たない初回コミットが全体を支配していたため。
chunk-levelの根拠は転送量ではなく**コストの有界性**（bsdiffは50MB blobで50MBを
suffix sortするが、chunkはmax 2MiBで打ち止め・並列化可能。実測CPUは
whole-blob 700s vs chunk-level 542s で、blobが大きいほど差が開く）。

### 10.1 Phase A（残り）: 大規模corpusのtrace replayとoracle study

1. 大artifactを持つ再現可能なOSSを選ぶ（LLVM/hermetic-llvm最有力。EngFlowと同条件で
   比較可能になる。次点: Go製静的リンクバイナリ、Rust、container layer）
2. 数百commitのbuild replayでdigest・bytes・metadataを保存
3. FastCDC2020とRepMaxCDCを適用、exact reuse後のmissing chunkに対し
   過去base chunkをoracle全探索
4. delta size・encode/decode time・base age・artifact kindを記録

Go/no-go: (a) eligible bytesが僅少ならworkload再選定 (b) oracleでもCDC-only比
追加10%削減がなければhybrid delta中止 (c) 圧縮済/nondeterministic artifactに阻害される
ならformat-aware trackへ

### 10.2 Phase B: REAPI proxy prototype

既存CASの前段proxy。標準CDCはそのまま、missing chunkだけdepth-1 delta転送。
protocol標準化は急がず、client/server双方を制御できる実験系で実証。

### 10.3 Baselines

| ID | Baseline | harness状況 |
|---|---|---|
| B0 | whole-blob、無圧縮 | 実装済 |
| B1 | whole-blob + zstd | 実装済 |
| B2 | REAPI CDC + FastCDC2020 | 実装済（bit-compatible移植） |
| B3 | REAPI CDC + RepMaxCDC | 未実装（buildbarn/go-cdc参照） |
| B4 | CDC + same-target previous-version delta | whole-blob版のみ実装済 |
| B5 | CDC + sketch/ANN selected **chunk-level** delta（提案） | 未実装 |
| B6 | CDC + oracle best base（上限） | 未実装 |

delta engineは高速型と高圧縮型の2種以上（Git-style / bsdiff系 / xdelta・VCDIFF系 /
Ddelta系）を比較し、単一algorithmに結論を依存させない。

### 10.4 Workload軸

repository（C++/Java/Rust/Go/LLVM）× artifact（.o/.a/.so/exe/JAR/bundle）×
change種別（comment/impl/API/dep/toolchain）× download policy（all/toplevel/minimal）×
local state（cold/warm/partial）× network（帯域/RTT/loss）× determinism（on/off）

**EngFlowの知見を反映: disk cache条件とdownload policyは効果を支配する第一級の軸。**

### 10.5 Metrics

主: network bytes（UL/DL）、E2E materialization time p50/p95/p99、CAS physical storage
副: CPU encode/decode/hash、RPC数、local disk I/O、exact chunk reuse率、
delta ratio分布、selector recall@k、oracle gap、fallback率、index cost
正しさ: chunk/blob digest failure数、corrupted blob accepted数（期待0）、
missing baseからのfallback成功率、crash/retry後のidempotence

### 10.6 統計とfailure injection

paired comparison、warm-up分離、順序randomize、絶対値+CI報告、
byte削減をspeedupと言い換えない。
注入: delta 1bit破損、base欠損、誤base、chunk順序入替、partial RPC、
parameter不一致、単一chunkのみのlocal cache、cross-tenant candidate。
**#29544の再現シナリオ（path-backed stream × chunk DL）を試験項目に含める。**

## 11. 論文戦略

### Paper A: Content-Defined Chunking for Remote Build Caches: An Empirical Study

貢献: (1) REAPI CDCの独立評価（複数repo・artifact・policy） (2) FastCDC vs RepMaxCDC
(3) 効果を決める条件（**小artifactでの無効性を含む — 予備実験済**）
(4) CDC後のresidual delta opportunityのoracle上限。
最低リスク。negative resultでも価値。MSR / ICPE / systems workshop。

### Paper B: DeltaCDC

タイトル候補: DeltaCDC: Digest-Verified Similarity Transfer for Remote Build Artifacts /
Beyond Exact Chunks: Cost-Aware Delta Materialization for Remote Build Caches

主張: (1) materialization = digest制約付きplan最適化 (2) missing chunkへのdepth-1
verified delta (3) selectorをcorrectnessから分離 (4) 実commit historyでCDC-only比の
byte/storage/latency削減 (5) failure injection下の安全性（#29544を動機に）

Abstract skeleton:
```
Remote build caches avoid action execution, while content-defined chunking
avoids transferring unchanged chunks of large outputs. However, chunks that
change are still transferred in full, even when a similar chunk is locally
available. We formulate artifact materialization as cost minimization subject
to an exact digest constraint and present DeltaCDC, which reconstructs missing
chunks from selected bases using binary deltas. Candidate selection affects
performance only; every chunk and final blob is digest-verified, with full
transfer as a fallback. Across [N] commits from [M] repositories, DeltaCDC
reduces [metric] by [X] relative to REAPI CDC while adding [Y] CPU/RPC overhead.
Failure injection confirms that corruption and missing bases are detected and
fall back without accepting an incorrect artifact.
```

## 12. v2から採用・修正・却下した主張（v3判定）

| 元の主張 | 判定 |
|---|---|
| 最速のtaskは実行されなかったtask → 最速のbyteは転送されなかったbyte | 採用 |
| exact digest / dimension hash / sketch の三層分離 | 採用（v1と同一） |
| lookup一般化をknown-digest materializationへ限定 | 採用（v1のCase A/Bと同旨） |
| 「build cacheのverified deltaは未適用」の撤回 | 採用（v1検証ログと一致） |
| 純関数でもoutput digestは実行前に得られない | 採用 |
| ccacheの正確な仕様（3 modes） | 採用・v1を修正 |
| Rice定理の一言済ませ回避 | 採用・v1を修正 |
| LtHash保留 / NCD降格 / 古典solver主張の撤回 | 採用 |
| Comonad / Langevin / SDE | 却下維持 |
| Trummer & Koch = MQO | 採用（v1のV9と一致） |
| **Phase A未実施としてのgo/no-go設定** | **修正: 小artifact領域は実施済・GO判定（§10.0）** |
| fix PR番号 | 修正: digest validation欠如のfixは#29593、#29544（切り詰めbug）のfixは#29614。別物 |

## 13. 検証キュー（v3更新）

| 優先 | 項目 | 状態 |
|---|---|---|
| P0 | 大artifactでのresidual delta opportunity（Phase A残り、LLVM推奨） | **一部済**: ripgrep 25コミットで実測（§10.0b）。LLVMでのEngFlow比較は未 |
| P0 | remote build cacheでのchunk-level binary delta先行例の網羅検索 | **済**: `results/v4-survey.md`。CDC系の全公開資料にdelta比較は存在せず。ただしDolstra 2005 / backup系resemblance delta / elfshaker / REAPI issue #272 と対照してスコープ限定が必須 |
| P0 | CPU時間測定（encode/decode）をharnessへ追加 | **済**: 全schemeで計測（§10.0b、DELTACDC.md） |
| P1 | BuildBuddy記事の本文確認（数値・条件） | 未 |
| P1 | remote-apis PR #377（streaming Split/Splice）とBazel PR #30131（RepMaxCDC）の追跡 — protocol変更リスク | 未 |
| P1 | FastCDC2020 vs RepMaxCDC再現比較 | 未 |
| P1 | 双方向protocol仕様化・threat model定義 | 未 |
| P2 | selector ablation（predecessor/sketch/ANN/oracle） | **一部済**: positional / sketch / hybrid を実測（§10.0b）。oracleはサンプル数不足で未確定 |
| P2 | 量子scheduler small offline study | 未（本体と分離） |

## 14. 一次資料と公式実装

（v2のリスト33件を踏襲。検証済に印）

**現行remote cache / CDC:**
1. REAPI remote_execution.proto — SplitBlob/SpliceBlob/capabilities ✅ソース確認 @becdd8f
2. REAPI releases — v2.12.0にPR #282 ✅git tagで確認
3. Bazel `--experimental_remote_cache_chunking` ✅ソース確認 @3cdd083
4. Bazel GrpcCacheClient / FastCdcChunker / ChunkedBlobDownloader ✅ソース確認
5. Bazel issue #29544 ✅本文確認（root cause含む）。fix #29614
6. Bazel PR #29593（digest validation fix, 9.1.1）— EngFlow経由で確認
7. Bazel issue/PR #30131-2（RepMaxCDC）— 未読
8. BuildBuddy CDC report — 未読
9. EngFlow LLVM CDC report ✅本文確認
10. remote-apis PR #377（streaming提案）— EngFlow経由で認知、未読

**Build / incremental:** Build Systems à la Carte (ICFP18/JFP20), Shake (ICFP12),
Nix thesis (2006), Adapton (PLDI14), Acar SAC thesis (2005), ILC (PLDI14),
DBSP (PVLDB23), Buck2 Modern DICE, Rust red-green, Bazel ijar ✅README確認,
ccache 4.13.6 manual

**CDC / delta / similarity:** rsync tech report, LBFS (SOSP01), FastCDC (ATC16),
FastCDC2020 (TPDS20), Git pack format, Courgette, Ddelta, Charikar (STOC02),
Broder (1997)

**Quantum:** Trummer & Koch (PVLDB16, MQO), Ready to Leap (2023),
Quantum-augmented Query Optimizer (PVLDB25), JSSP on D-Wave (Sci Rep 2022)

## 15. 最終判断

```
Correctness:       exact action / chunk / blob digest
Reuse granularity: computation/query → semantic dimension → CDC chunk → byte delta
Policy:            vector/sketch → base選択・routing・prefetch・scheduling
```

優先度:
1. 大artifactでのresidual opportunity測定（Phase A残り。LLVMでEngFlowと比較可能に）
2. CPU時間測定の追加
3. chunk-level depth-1 verified delta prototype（DeltaCDC本体）
4. 先行例の網羅検索（V4/P0）
5. 多次元semantic invalidationは別論文、量子は比較実験まで

最初に書くのはPaper A。小artifactでのCDC無効性（実測済）+ 大artifactでの
residual opportunityで一本になる。それが強ければDeltaCDCを本体論文へ。

## 16. 検証ログ

### 2026-07-19 (1): V1検証 — remote-apis / bazel ソース直読

対象commit: remote-apis @ becdd8f (2026-03-31) / bazel @ 3cdd083 (2026-07-19)

1. 近傍参照のdelta encoding（bsdiff/vcdiff/xdelta系）はREAPIにもBazelにも存在しない
   （proto全文・remoteモジュール全文grep）
2. per-blob圧縮は存在（compressed-blobs path、supported_compressors、
   `--remote_cache_compression`）
3. SplitBlob/SpliceBlob RPC確認（PR #282, 2025-07-09, Sascha Roloff。
   後続 #337/#353/#357。ChunkingFunction: FAST_CDC_2020 / REP_MAX_CDC）。
   SpliceBlobは期待digest必須・サーバ検証
4. Bazel実験実装確認（フラグ・FastCdcChunker・ChunkedBlobDownloaderの
   `Utils.verifyBlobContents` によるclient側検証）
5. Skyframe "change pruning"（early cutoff実装）確認。粒度はSkyValueノード
6. ijar README確認（V8完了）

### 2026-07-19 (2): 予備実験 — Lua corpus

§10.0参照。harness: fastcdc.py（bit-compatible移植）、collect_corpus.sh、analyze.py。

### 2026-07-19 (3): v2引用の実地検証

1. **v2.12.0**: ローカルgit tagで `git tag --contains 9ef19c6` → v2.12.0。確認
2. **Bazel issue #29544**: 実在・closed。9.1.0でchunking+disk_cache+toplevel DLが
   大出力を単一chunkへ切り詰めたまま成功報告。root cause: per-chunk DLに
   path-backed final output streamを渡し、disk cacheのpath fast-pathがchunkごとに
   最終出力を上書き。fix #29614
3. **EngFlow記事** (2026-07-15): 実在・本文確認。Bazel 9.1.1 + FastCDC2020 + EngFlow。
   数値（58%/36%/50%/初回6%）、条件依存性（disk_cache必須・minimalで効果小）、
   9.1.0のdigest validation欠如とfix #29593、streaming提案 PR #377、
   RepMaxCDC PR #30131 を確認
4. **BuildBuddy記事**: 未読。数値は[運用報告]のまま扱う
