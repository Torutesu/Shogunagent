# ShogunAgent の方針

**AI エージェントのための状態レイヤー。**

> Memory remembers. State knows.
> 記憶は覚える。状態は知っている。

[English version / 英語版](strategy.md)

> **この文書について**：2026 年 10 月まで README に載せていた方針を、README を導入手順に切り替えた 2026 年 11 月 1 日にここへ移しました。方向性と決定の記録であり、いま使えるものの一覧ではありません。現在の提供状況は [README](../README.ja.md) を見てください。§12 のテスト件数とベンチマークの数値は 2026 年 10 月 26 日時点のものです。最新のリビジョンについては CI を見てください。

このリポジトリは ShogunAgent の方針・調査・コードを置く場所です。ShogunAgent は [ShogunAI](https://github.com/Torutesu/ShogunAI-) の中にある状態推定と権限のエンジンを、人間ではなく AI エージェント向けにサービスとして提供するものです。以下の決定は 2026 年 10 月時点のもので、`reports/` 内の初期ドラフトより優先します。

**現状**：ロードマップのフェーズ 1〜5 はコードとして実装済みで、このリポジトリで検証が通っています。ここでの「実装済み」は作ってテストしたという意味で、リリース済みではありません。デプロイ、npm / PyPI への公開、レジストリやディレクトリへの提出はまだ行っていません。いずれも §12 に挙げたオーナーの判断を待っています。

---

## 1. 何であるか

多くの AI エージェントを動かすチームに対して、ShogunAgent は全エージェントに「ユーザーの仕事の校正済みモデル」（人・プロジェクト・コミットメント・オープンループ）を 1 つ与える共有状態レイヤーです。全ての事実に根拠（provenance）が付き、結果を伴う全ての行動に人間のゲートが掛かります。

設計の柱は **型付き・権限階層付き・ローカルファースト・可搬**です。他のメモリ・コンテキスト製品と重なる機能もあるため、カテゴリ全体ではなく製品と提供形態ごとに比較します。

ShogunAgent は人・プロジェクト・コミットメント・オープンループという現在の仕事の状態に注力します。Mem0、Zep / Graphiti、Letta などのメモリ・コンテキスト製品と重なる用途もあります。カテゴリ名だけでなく、具体的な機能と配置要件で評価します。

## 2. バリュープロポジション（3 本柱。増やさない）

| 柱 | 主張 | コードベース上の証明 | 比較上の注意 |
|---|---|---|---|
| **Typed state, not text memory** | 全ての事実は型付きの行で、元イベントからの逐語 quote と confidence を持つ。新しい事実が古い事実を置き換え、矛盾は明示され、confidence は時間で減衰する | `state_provenance` が空の書き込みはリポジトリ層が拒否。High / Medium / Low のバンドを `shogun-fusion/confidence.rs` の 1 箇所で強制。loop consolidator は quote の完全一致と confidence 0.84 以上が無ければ保存しない | [Graphiti](https://github.com/getzep/graphiti) も型付きのエンティティ・エッジ、元エピソードへの provenance、時間に基づく無効化を提供。型付き状態と根拠は ShogunAgent だけの機能ではない |
| **Permission tiers on every action** | 読み取りは即時（L1）。ユーザーデータに触る書き込みは一押し確認（L2）。外部送信と決済は明示的な人間承認（L3）。型で強制され、どのエージェントにも同じ | `shogun-agents` の `SendAction` は型で L3。MCP の `readOnlyHint` / `destructiveHint` / `openWorldHint` と MRTR の `input_required` / URL elicitation に 1 対 1 で写像 | 製品ごとに承認対象の行動と強制方法を比較する。このリポジトリは競合の権限が read / write の 2 値のみであることを立証していない |
| **Local-first, open, portable** | データは既定で端末を出ない。クラウド経路は本文を保持しない。エンジンは OSS。状態は公開形式でエクスポートできる | ゼロ保持の中継（traceability log に本文カラムが無い）。秘密情報の redaction は recall 1.0、誤検知 0。ユーザー保持鍵 | [Mem0](https://github.com/mem0ai/mem0) には OSS ライブラリとセルフホストのサーバーがあり、[Letta](https://docs.letta.com/agent-sdk/memory) はセルフホストの Git メモリに対応。配置と可搬性は製品ごとに比較する |

**ルーターの位置づけ（1 行）**：エージェントのための 1 つのエンドポイント。ツールを見つけ、権限階層の下で実行し、学んだことを自分の状態として持つ。

**価格（1 行）**：読み取りは無料。取り込んだ分と実行した分だけ払う。

### メッセージの階層

- **3 語**：The state layer for agents.
- **1 文**：Calibrated truth about your work (people, projects, commitments, open loops) with provenance on every fact and a human gate on every send, shared across all your agents.
- **開発者向け 30 秒**：あなたのエージェントは毎セッションをゼロから始め、聞かずに行動する。ShogunAgent は MCP 越しに、型付きで根拠付きのユーザーの仕事のモデルを 1 つ与え、全行動に 3 段階の権限を掛け、データを既定で端末に留め、全てを公開形式でエクスポートする。読み取りは無料、取り込みと実行の分だけ払う。
- **CTO 向け 30 秒**：エージェントの失敗は文脈の失敗と権限の失敗である。ShogunAgent はエージェントが仕事について信じていることの system of record で、全事実の裏に quote、全行に confidence、全外部送信に人間の承認がある。許可なく端末を出るものは無く、ロックインも無い。

### 使わない言葉

「AI OS」、「ワールドモデル」（ロボティクスの用法と衝突）、カテゴリ名としての「メモリ」、「ベンチマーク #1」（信頼を失った言葉。同一ハーネスの結果だけを公開する）、地域固有のコンプライアンス文脈。

## 3. 状態の取得と供給

![図 1. 生データから ShogunAgent エンジンを経てエージェントの文脈へ](diagrams/architecture.svg)

*図 1. 生データは principal ごとの暗号化ストアに入り、Dream Cycle が全行に quote と confidence を持つ型付き状態に変える。エージェントは context pack・ファイル・ツールとして受け取り、全ての行動は L1/L2/L3 のゲートを通る。エージェントの書き込みとルーターの実行はイベントとして戻る。*

取得は 5 系統で、全て同じ append-only の `event_log`（dedup、redaction、instruction-shaped ゲート）に落ちます。

| 系統 | 中身 | 必要なもの |
|---|---|---|
| エージェントからの ingest（主経路） | 会話ターン、ツール結果、タスク進行。`memory.ingest` と Anthropic memory tool 互換の `/memories` 書き込み | 無し。全エージェント |
| コネクタ pull | Gmail、Calendar、Slack、GitHub、Linear、Notion。サインインは人間が完了させる。トークンは principal 自身のストアにだけ置く。更新はコアが行い、提供元での取り消しは提供元が許す範囲で行う（Notion は不可、GitHub はアプリの secret があるときだけ） | `shogun-integrations`、ホスト型の接続フロー |
| プロバイダの webhook | GitHub・Linear・Slack の配信はそのままイベントになる。Notion・Calendar・Gmail は変更通知だけを送り、コアが principal 自身のトークンで本文を取りに行く（Notion はページのみ。添付と画像は取らない） | プロバイダごとに登録したフック |
| ルーター実行ログ | ルーターを通る全 run の tool × task × outcome | ルーターを持つこと |
| 端末取得（premium） | Accessibility API のテキスト、Visual recall の OCR、会議の文字起こし | ShogunAI Mac アプリ |
| インポート | ChatGPT / Claude / Cursor のエクスポート、export v1 / v2 のパッケージ | `shogun-memory-import/v1` |
| ブラウザ拡張 | 表示中のページのテキスト、URL、タイトルだけ。スクリーンショットは送らない | 仕様のみで未実装。用いる書き込み専用の `ingest` キーは実装済み |

推定は ShogunAI のパイプラインそのまま：毎時の Dream Cycle（セッション化、prefilter、スキーマ強制の抽出）→ quote と confidence 付きの state tables → consolidator → entity pages と期間要約。

供給は 3 形状。2026 年のエージェントが実際に呼ぶ形だからです：予算付き context pack（`get_context_pack`。High は事実、Medium は「possibly」、Low は除外）、仮想ファイルシステム（`/memories/people/<name>.md`、`/memories/open_loops.md`。Anthropic の memory tool が統合ゼロで読む）、MCP ツール（`state.*`、`recall`、`search`）と A2A Agent Card。エージェント間の引き継ぎでは、コアが組み立てた型付きの「仕事の状態」オブジェクト（`handoff.create` / `handoff.accept`）を渡し、受け手は自分の scratch 名前空間に保存する。

全ての機能は MCP・REST・CLI のどれでも同じで、権限レベル・結果・応答の封筒（`usage`、`latency_ms`、`next_action`）が一致する。

ShogunAgent は model-agnostic です。自社の LLM 利用は抽出と統合だけで、安い Batch レーンで行います。検索はキーワードが先（FTS5 trigram と、日本語・中国語向けの 2 文字索引）。ローカル埋め込みは任意で、既定は nomic-embed-text、多言語の multilingual-e5-small は選択式です。埋め込みをクラウド API に送ることはありません。

## 4. モデルレイヤー各社との関係

**各社が確実に取り、我々が争わない領域**：自社製品内の個人化記憶、自社 API 内のセッション状態、保存の単価競争。

**我々が注力する領域**：ベンダー横断の仕事の状態、データの可搬性、異なるベンダーのエージェントを跨ぐ人間承認の統治、端末の文脈。これは製品方針であり、モデル提供者が実現できないという主張ではありません。

**付き合い方**：顧客であり供給者であり、競合しない。Anthropic の memory tool はストレージ非依存なので我々がその裏側になる。Claude Code プラグイン、Anthropic directory、MCP Registry に載る。OpenAI：Plugin directory と Agents SDK ミドルウェア。Google：ADK と Vertex の memory インターフェース。AWS：AgentCore の self-managed memory 枝の外部ストア。Microsoft Copilot の communication memory が最も近い比較対象で、M365 の外のエージェントに同じものを提供する。

**警戒**：Anthropic Cowork / Claude Desktop の端末文脈への進出、OpenAI Dreaming の個人化から仕事の状態への拡張、Microsoft が communication memory を Foundry 経由で開くこと。いずれも 1 ベンダー内の拡張であり、ユーザーが混ぜるベンダーが増えるほど我々の価値は上がる。

## 5. 競合の層別

| 層 | プレイヤー | 差 | 立場 |
|---|---|---|---|
| メモリ・コンテキスト製品 | Mem0、Zep / Graphiti、Letta、supermemory、Cognee、Engram | 仕事の状態スキーマ、承認、配置、根拠、エクスポートを製品ごとに比較。機能は重なる | 用途が重なる。実際の製品と提供形態を評価する |
| ルーター・レジストリ | Monid、Composio、Arcade + Smithery、Glama、Coinbase Agentic Market | ShogunAgent はルーティングと仕事の状態・権限エンジンを組み合わせる。連携ごとに比較 | 連携候補でありルーティング用途の競合 |
| 企業コンテキスト基盤 | Microsoft Copilot、Glean、Jedify、Interloom | 対応データ源、エージェントのインターフェース、統治を提供形態ごとに比較 | 仕事の文脈という用途の比較対象 |
| 取得層 | Screenpipe、Limitless（Meta）、Rewind | 取得元、推定機能、現在の提供状況を個別に比較 | 入力元の候補であり文脈を扱う用途で重なる |

### 一次資料による比較メモ

2026-10-27 に確認。以下は文書に記載された機能であり、比較ベンチマークや各製品の動作検証ではありません。

- **Mem0**：[公式リポジトリ](https://github.com/mem0ai/mem0) は OSS ライブラリ、セルフホストのサーバー、クラウドを区別しています。[スコープ付き追加](https://docs.mem0.ai/core-concepts/memory-operations/add)、[OSS のメタデータフィルター](https://docs.mem0.ai/open-source/features/metadata-filtering)、[削除](https://docs.mem0.ai/core-concepts/memory-operations/delete) の文書があります。提供形態ごとの仕様を確認します。
- **Graphiti と Zep**：[Graphiti](https://github.com/getzep/graphiti) は OSS フレームワークであり、Zep のマネージド製品とは区別します。Pydantic の型付きエンティティ・エッジ、元エピソード、時間的有効性と無効化、ハイブリッド検索が記載されています。両者を自由文ストアと呼ぶだけではこれらを見落とします。
- **Letta**：[メモリの文書](https://docs.letta.com/agent-sdk/memory) は永続的な Git メモリ、コンテキスト内・オンデマンドのメモリ、共有リポジトリ、セルフホストの Git remote を説明しています。ホスト型の共有リポジトリとセルフホストでは運用要件が異なります。

ShogunAgent が目指す強みは仕事に特化した状態・根拠・行動統治の組み合わせです。再現可能な例で示し、カテゴリ名だけから独占的な機能、全製品のロックイン、優位性を推定しません。

## 6. MOAT（強い順、正直に）

1. **二面市場での位置**。ツール供給側はルーターに、エージェントは MCP に接続する。コネクタが増える → エージェントが増える → 実行データが増える → `discover` が良くなる → エージェントが増える。このループには状態層が必要で、ルーター単体では回らない。
2. **複利する状態**。動かし続けるほど状態の精度が上がる。事実は自由にエクスポートできるが、校正された confidence と学習パターンは観測の継続からしか生まれない。ロックインではなく継続の価値。
3. **信頼**。ゼロ保持、全行の provenance、6 種の評価ハーネス、書き込み経路での OWASP ASI06 緩和。企業の信頼は監査と時間で稼ぐもので、買えない。
4. **端末レベルの真実**。ラボもメモリ各社も持たない Accessibility と Visual recall の取得層。プラットフォームを足すほど強くなる。
5. **標準**。エクスポート形式と権限階層の分類を W3C memory interop CG と MCP アノテーションに持ち込み、参照実装になる。
6. **工学の先行**。約 37 万行の Rust、校正ルール、加算のみの 105 本のマイグレーション、評価ハーネス 6 種（うち 1 つが open-loop ベンチ）、選択 eval、多言語検索スイート。壁ではなくリード。

**MOAT ではないもの**：ルーターのコード（MIT、780 フォーク）、メモリ API の形、ベンチマークのスコア、モデルへのアクセス。

## 7. プロダクト上の決定

### 「OpenRouter for Agents」を入口として建てる

オーナー決定。調査の警告（コネクタ・コードはコモディティ、中央値 $0.0015/call、Arcade と Stripe による統合、自社製品を自分で順位付けする利益相反、記憶はスポット市場に合わない）は事実のまま残り、設計条件になります。

1. **二層構造**。Router（discover / inspect / run、従量、ゼロ保持プロキシ）は粗利ほぼゼロで良い。収益は ingest と状態で測る。ルーターの KPI は「ルーター経由エージェントのうち状態層を使い始めた割合」。
2. **コールドスタートは借りる**。Monid 互換のコネクタ定義で既存の端点定義を取り込み、Monid の hosted カタログをパススルー。自前コネクタは差別化する領域（記憶・状態、承認を要する書き込み）だけ。
3. **全 run に L1/L2/L3**。読み取りは L1、ユーザーデータの書き込みは L2、外部送信と決済は L3。`scope.rs` の「表に無い操作は拒否、ExternalSend は必ず L3」をそのまま載せる。
4. **ルーターはユーザー所有の状態で学ぶ**。tool × task × outcome を ShogunAgent の状態に記録し、`discover` をユーザー・テナント別に個別化する。可搬で、中央集権しない。
5. **自己優遇しない**。スコア内訳に価格を含めて公開、first-party ラベル、ディレクトリ提出物とルーターの SKILL.md を分離。

### 価格：完全従量

固定サブスク無し。プリペイド・クレジット、残高 0 で `402`、キー別・エージェント別のハード上限を初日から。

下の単価はゲートウェイが現在実装している値です（`apps/api/src/pricing.ts`、`/.well-known/pricing.json`）。ローンチ時点の推定のままで、design partner の実測で改定します。

| 計量項目 | 単価 | 備考 |
|---|---|---|
| ツール実行（他社プロバイダ） | プロバイダ価格 + 5% | テイクは 1 回、ウォレットが 1 度だけ課金。失敗・0 件は $0。Monid 掲載ツールは Monid の価格にこのテイクを 1 回足すだけで、二重にはしない |
| ツール実行（自社コネクタ） | $0.002 / call | テイク無し。書き込みは L2/L3 を経て同額。自社の状態読み取りは $0 |
| 取り込み（ingest） | $0.50 / 1K イベント | 抽出 LLM 原価を含む。webhook の配信は実際に保存したイベント数で精算 |
| モデル起動 op | $0.10 / 1K | consolidation、AI 検索、entity page |
| 状態の読み取り | $0 | `state.*`、`get_context_pack`、`recall`、`search`、`handoff.create`、`connections.list` |
| 遠隔 L2/L3 承認、行動のキュー投入、支払い委任の申請 | $0 | |
| 委任の範囲内での支払い | 決済レールの手数料 | 現在は $0。レールはお金を動かさないスタブ |
| ホスト保存 | $0.10 / GB-月 | 削除・エクスポートは常時無料 |
| 無料枠 | 自己登録エージェントごとに $5 | |
| Local（OSS） | $0 | |
| Enterprise | 同じ従量、年間コミットで 10〜25% 割引 | BYOC / ZDR / SSO / 監査を含む。座席課金はしない |

ゲートウェイは、コアが「実際に何をしたか」を報告した量で課金します。拒否された呼び出し、エラー、冪等な再送は無料です。

粗利目標：ingest は原価の 3〜5 倍、run は 5% テイクで損益ゼロ近傍。design partner の実測で改定する。

### 建てないもの

価格だけで競う汎用ルーター。独自決済基盤（支払いは差し替え可能なレール経由。現在はスタブ）。本番での x402 受け入れ（判断済み：`X402_ENABLED` の裏に実装し既定 off。`docs/decisions/0002-x402.md`）。グラフ DB（SQLite の型付き関係行で足りる）。Postgres 移植（再利用率が 60〜70% から 25% に落ちる）。独自埋め込み研究。チャット個人化メモリ API。LoCoMo のスコア追い。取り込み API より先の Windows / モバイル取得層。消費者向けアプリ。デスクトップ固有ツールのサーバー公開（サーバービルドが並べるのは core、files、handoff、payments、governance、import、state、skills、wiki の各グループ。Mac 固有のグループは `desktop` ビルドでだけ並ぶ）。

## 8. GTM：開発者と個人から、OSS を装置に

**誰に、どの順で**：(1) コーディングエージェントのパワーユーザー（Claude Code、Codex、Cursor）、(2) エージェント製品を作る開発者（Agents SDK、Vercel AI SDK、Mastra、LangGraph）、(3) 複数エージェントを回す小チーム、(4) 2〜3 から昇格した企業。消費者向け記憶製品は全て退場したので、一般消費者ではなく「毎日エージェントを動かす人」に売る。ShogunAI の既存 Mac ユーザーは初日のコホート。彼らのエージェントには既に read 接続が自動投入されている。

**開く**：エンジン（state tables、confidence、consolidator、Dream Cycle）、ローカル MCP / REST / CLI、コネクタ定義とルーターのエンジン、SKILL.md、エクスポート形式、評価ハーネス 6 種。ライセンスは Apache-2.0。**閉じる**：hosted sync、遠隔承認、ウォレットと課金、hosted ルーターカタログとプロバイダ契約。ファネルは「ローカルとホストで同じ API、`baseURL` を変えるだけ」。エンジンそのものを開く。ライセンスとホスト型サービスの提供状況は製品ごとに比較する。

**OSS ローンチの実務**：10 秒で動く README の GIF（`npx shogunagent` で、あなたのコーディングエージェントがオープンループを知っている）。Show HN は機能一覧ではなく再現可能な主張（同一ハーネス結果、監査型の記事）で。ベンチマークのハーネスを公開し、競合の数値を再現可能にする。「open-loop tracking」ベンチをコミュニティに提供する。コネクタ定義を貢献の低摩擦な入口にする。

**エージェントが自分で導入する経路**：SKILL.md、`npx shogunagent help --json`、docs MCP、`llms.txt`。エージェントが自分で導入し、自分のプリペイド予算内で使う。

**プラットフォームの棚（全部、英語で、最初に）**：Claude Code プラグイン → Anthropic directory。公式 MCP Registry。Cursor / Windsurf の MCP 設定。OpenAI Plugin directory。Glama、Smithery、Composio toolkit、Monid（記憶カテゴリができたら）。

**1 行統合**：`withShogunAgent()` を Vercel AI SDK、OpenAI Agents SDK、Claude Agent SDK、Mastra、LangGraph、CrewAI に。これらのアダプターでエージェントと ShogunAgent の連携作業を減らす。

**やらないこと**：有料広告、SEO 先行、カンファレンスのブース、企業営業の先行。

**転換の階段**：OSS ローカル（$0）→ hosted sync（2 台目の端末やエージェント）→ ウォレット入金（コネクタ ingest、ルーター run）→ チーム（共有状態、遠隔承認）→ 企業（BYOC、ZDR、監査）。最初の 2 段は無料で信頼と量を作り、収益は 3 段目から。

## 9. ロードマップ（順序のみ。期間は置かない）

AI 前提で開発する。フェーズは順序と出口条件だけで定義し、日数や月数は書かない。下の全フェーズのコードはこのリポジトリにあり、検証が通っている。「コード」列の「実装済み」は作ってテストしたという意味で、リリース済みではない。出口条件は市場での計測なので、ローンチ後にしか満たせない。

| フェーズ | 到達点 | コード | 出口条件（推定） |
|---|---|---|---|
| **1. コアとルーター骨格** | `shogunagent-core` を Linux で単体起動（principal ごとに SQLCipher ファイル 1 つ）。Streamable HTTP MCP core 21 本の凍結。API key 認証。`/memories` 射影。エクスポート形式 v1。OSS 公開。ルーター：discover / inspect / run、Monid 互換コネクタ、hosted パススルー、記憶・状態カテゴリ、キー別上限、全 run に L1/L2/L3。プリペイド・クレジットと 402。SKILL.md、Claude Code プラグイン、MCP Registry 用エントリ（検証済み、未提出）。Apache-2.0 で OSS 公開できる状態。同一ハーネス結果は `docs/benchmarks/` | 実装済み | 外部エージェントからの `tools/call` が継続的に流れる。design partner 契約。ベンチマーク公開 1 本。ルーター → 状態層の転換率が計測できている |
| **2. 承認・SDK・課金・カタログ** | 遠隔 L2/L3 承認（Web、Slack、webhook）。A2A Agent Card。TS/Python SDK と `withShogunAgent()`。agent-scratch 名前空間と ACL・監査。Stripe 従量課金とウォレット。エージェント向け OAuth 2.1。個別化 `discover`。プロバイダのセルフサーブ掲載。ディレクトリ提出物一式。ローカル埋め込み | 実装済み | 有料ウォレット利用者が増え続ける。ingest 粗利が原価の 3 倍以上。LLM 原価 ≤ 売上の 15%。転換率が改善 |
| **3. Enterprise・標準・決済** | 承認済み行動を 1 回だけ実行。永続する承認台帳。ハッシュ連鎖の監査エクスポート。テナント、SSO（OIDC、SAML）、SCIM、リージョン。顧客管理の暗号鍵（AWS、GCP、Vault）。Helm と Compose。エクスポート形式 v2 と相互変換。open-loop ベンチ v1。プロバイダ支払いと公開カタログ。x402 の判断 | 実装済み | Enterprise 契約。NRR > 110%。同一ハーネスで multi-session / temporal 上位 3。自前コネクタ比率の上昇 |
| **4. ローンチ準備** | ルーター実行をウォレットで 1 回だけ課金し、精算は永続アウトボックス経由。hosted `/mcp` を principal ごとのサイドカーへ中継し、コアの報告量で課金。handoff をコアで組み立て三面に。MCP 適合スイート。実フレームワーク内の SDK テスト。本文を持たないメトリクスとトレース。負荷試験。脅威モデル、ファジング、シークレット走査。手動のみの公開・デプロイ workflow。本番手順書 | 実装済み | ホスト型サービスの稼働。デプロイ先 URL で適合スイートが通る。最初のディレクトリ掲載が通る |
| **5. 拡張** | 多言語埋め込みと検索スイート。プロバイダの webhook と本文取得。ホスト型コネクタのサインイン・更新・状態・切断（Gmail、Calendar、Slack、Linear、GitHub、Notion）と、提供元が許す範囲での取り消し。自社の Gmail・Notion 読み取りコネクタ。エージェントの支払い委任。export v2 の CG 向け草案と JSON-LD context。受動態・日本語・別スレッドの open-loop 規則 | 実装済み | 非英語の recall が英語と同水準。Mac 以外の取得元から状態が育つ。エージェントの支払いが L3 と上限の中で完結する |

## 10. ShogunAI から持ち越すもの

行数で 30〜40%、難しい部分で 70% 以上。

再利用：`shogun-memory`（state tables、`state_provenance`、loops、entity pages、federated search、sanitize、traceability、import。V1〜V91 を SQLite のまま使い、`tenant_id` / `uid` / `actor` を加算マイグレーションで足す）、`shogun-fusion`、`shogun-agents`、`shogun-redact`、`shogun-license`（verify）、`shogun-mcp`（`MemoryBackend`、`ApiLevel`、toolsets、source policy、rate limit、REST routing。Keychain 依存の設定と launcher check は環境変数と `apps/api` のトークンに差し替え）、`shogun-core` の中立部（dreamcycle、llm、ai_search、context、`shogun_api` / `shogun_mcp` バイナリ）、`apps/api`（Ed25519 認証、reserve-then-settle 計量、ゼロ保持ロガー）。

持ち越さない：macOS の取得層、Keychain 依存の秘密、Tauri デスクトップ、単一テナント前提。

実際にこの形で作った：`shogun-api --multi` が principal ごとの SQLCipher ファイルを扱い、前段の `apps/api` が認証・計量・402 を受け持つ。デスクトップ固有のコードは cargo feature `desktop`（既定 off）の裏にある。何を移したか、なぜかは `docs/MIGRATION.md` に記録した。

## 11. エージェント・コマース：Day 1 から「エージェントに選ばれる」設計

買い手は人ではなくエージェントになっていく。エージェントはサービスを見つけ、価格・レイテンシ・健全性・信頼で比較し、自分で導入し、予算の中で払い、最初の数回の呼び出しの結果で再選択か離脱かを決める。この全段階を最初のリリースから設計対象にする。

### 原則

1. **設定ゼロの初回呼び出し。** キーの無いエージェントでも `discover`、`inspect`、狭い範囲の読み取りは呼べる。最初の書き込みや従量の実行で `402` を返し、進むのに必要な情報を全部載せる。価値を見る前に人を要求しない。
2. **全レスポンスが自己記述。** 各結果に `usage`（コストと単位）、`latency_ms`、エラー時の `next_action`（何が失敗したかではなく何をすべきか）、`hints`（より安い・より適した代替。ルーター内の他社コネクタも含む）を付ける。エージェントの選択ループは不透明な失敗を罰する。正直さが継続率の仕組みになる。
3. **全ての well-known な場所で機械可読。** エージェントが見るあらゆる場所に同じ identity・同じツール名・同じスキーマを置く。人向けドキュメントは機械向けから生成し、逆はしない。
4. **説明文はマーケティングではなくランキングのために書く。** レジストリとルーターは説明文で順位を付ける。ツールの説明はエージェントがやろうとしているタスクを名指しし（「ユーザーが何を約束したかを探す」「この人との間で何が保留か」「ツールを実行して結果を状態として残す」）、除外も書く（「チャットの逐語保存には使わない」）。説明文は推測ではなく計測で決める（下記の選択 eval）。
5. **人の承認は最大 1 回、金のときだけ。** カードは URL モードの elicitation で一度だけ紐付け、以降はエージェントが上限内で使う。読み取りは一切聞かない。権限階層（L1/L2/L3）はユーザーの世界への行動を、予算上限は支出を統治する。二つは別物で、どちらも行動前にエージェントから見える。
6. **安定性はエージェントが計測できる機能。** wire name の凍結、公開された非推奨ポリシー、全書き込みの idempotency key、一覧の `ttlMs`、`Retry-After` の遵守、公開されたインシデント履歴。一度焼かれたエージェントは戻ってこない。

### Day 1 の成果物

| 面 | 出すもの | エージェントがそれを必要とする理由 |
|---|---|---|
| `/.well-known/mcp/server-card.json` | MCP Server Card：エンドポイント、認証、アノテーション付きツール一覧 | ハンドシェイク無しの段階的発見 |
| MCP Registry の `server.json` | 正規のレジストリ登録 | Claude Code、Cursor、ゲートウェイが最初に見る場所 |
| `/.well-known/agent-card.json` | 署名付き A2A Agent Card | 組織間の引き渡しと identity |
| `llms.txt` と `llms-full.txt` | エージェント可読のドキュメント索引 | エージェントは人より先にドキュメントを読む |
| `SKILL.md` | トリガー条件と非トリガー例外、setup、認証、優先順位、コスト警告、ステータスの意味、L2/L3 の意味、番号付きルール | エージェントが自分で導入し運用する |
| `openapi.json` | REST 面の OpenAPI 3.1 | コードを生成するエージェント |
| `/.well-known/pricing.json` | 機械可読の料金表、無料枠、上限 | 初回呼び出し前の価格比較 |
| `/health` と `/.well-known/status.json` | ライブの p50 / p95、エラー率、インシデント履歴 | 健全性で重み付けするランキング |
| `benchmarks.json` | reader、judge、seed、1 回答あたりコストを含む同一ハーネス結果 | 再実行できる信頼シグナル |
| `quote` エンドポイントと全従量呼び出しの `dryRun` | 確定前の正確なコスト | 予算を持つエージェントは不明なコストを拒む |
| 402 ペイロード | 金額、単位、受け付ける決済手段、人の承認 URL、`Retry-After` | サポート無しのセルフ入金 |
| `POST /v1/agents` | エージェントの自己登録：スコープ付きキーと無料枠を返す。レート制限と不正対策あり、メール不要 | フォーム無しの導入 |
| キー内の紹介属性 | `ref` を紹介したエージェントまたは開発者のウォレットに還元 | エージェントがチャネルになる |

これらは全てコードから生成している（不変条件 12）。well-known 文書、料金、見積もり、登録は `apps/api` が配信し、`server.json`・`SKILL.md`・`benchmarks.json` はこのリポジトリ内のファイル。部分的なものが 2 つある：`ref` は登録時に記録するがまだ還元していない。`server.json` は MCP Registry のスキーマ検証を通るがまだ提出していない。

### 決済の設計

Stripe で精算するプリペイド・ウォレット。`402` レスポンスは x402 の payment-required 形式に従い、ウォレットを持つエージェントがそのまま読めるようにする。x402 での精算の受け入れはフラグの裏に実装済みで、本番では off のまま（ADR 0002）。キー別・エージェント別のハード上限（日次・月次）は必須で、既定は保守的な値。レシートは呼び出しごとに機械可読。失敗と 0 件は無料。異議申し立ては API であり、メールではない。

エージェントは**支払い委任**の範囲でユーザーの代わりに支払える。人間が範囲を決めて許可する（支払先または支払先の種類、1 回と合計の上限、通貨、期限、目的）。委任の付与・拡大は L3 の人間承認。範囲内の支払いは事前承認済みで、レールを呼ぶ前に記録され、二重に払われない。範囲外は 1 回ごとの L3 承認に戻る。取り消しと縮小はどの面からでも即時に効く。

### 選択 eval：本当に重要な指標

CI から回すハーネス。タスク記述を与えたとき、Claude・GPT・Gemini・オープンモデルは、Mem0・supermemory・Zep・ルーター内の代替と並んだ ShogunAgent を、MCP Registry の掲載文・Claude Code プラグインの説明・OpenAI Plugin の掲載文・ルーターの `discover` のそれぞれでどれだけ選ぶか。結果は面ごと・モデルごとの選択率。説明文の変更はこの数字が落ちないときだけ出す。これは明示した評価条件での発見されやすさを測るものであり、他社が同様の評価をしていないことを立証するものではない。

### 自前ルーターの中で選ばれること

ルーターは他社ツールと自社コネクタを同じ公開スコア（タスク適合、価格、健全性、レイテンシ）で順位付けし、first-party はラベルで区別し、自己優遇しないポリシーを文書化する。状態層は競合ツールとして掲載しない。全ての run が書き込む基盤である。エージェントはルーターの中でツールを選び、結果がユーザーの状態として残るからルーターを選ぶ。

## 12. 実装の現状

`main` の最新コミットで検証済み：

| チェック | 結果 |
|---|---|
| Rust ワークスペースのテスト | 5,113 件成功、失敗 0 |
| サーバー機能付きコア | 2,283 件成功、失敗 0 |
| MCP サーバー（wire freeze を含む） | 939 件成功、失敗 0 |
| feature matrix、全マイグレーション（V1〜V105）のアップグレード、不変条件ガード 7 種 | すべて成功 |
| ゲートウェイ（`apps/api`） | Node 828 件、Workers 24 件 |
| ルーター（`apps/router`） | 130 件 |
| SDK | TypeScript ユニット 87 件・実フレームワーク 16 件、Python 88 件 |
| ローカルサーバーに対する MCP 適合スイート | 成功 33、失敗 0、スキップ 1 |
| selection-eval のゲート | 成功。落ちた面は無い |

ベンチマーク（注意書きとともに読むこと。`docs/benchmarks/`）：

- **ゴールデン検索評価**（132 問、大半が日本語、キーワード検索）：recall@5 0.985、MRR 0.948、誤答 0.000。
- **多言語検索スイート**（英語、日本語、スペイン語、ドイツ語、フランス語、中国語）：答えの無い質問は全言語で何も返さない。既定のキーワード検索では recall@5 0.552 で、言い換えと言語をまたぐ質問には埋め込みが要る。決定的なテスト用埋め込み（実モデルではない）では 0.966。実際の多言語モデルはまだハッシュ固定も閾値調整もしていない。
- **open-loop 追跡**：ジェネレータ自身のテンプレートでは 1.000 だが、規則はそのテンプレートを見て書いたので上限として扱う。別に書いた言い回しでは総合スコア 0.665（0.489 から改善）、精度 1.000、誤って閉じたループは 0。

残りはコードの外：

1. 社名・登記住所・法人番号・公開 API のホスト名・セキュリティ／プライバシー／サポートの連絡先を 1 つに決める。現在はファイルごとに食い違っている。
2. プライバシーポリシー、利用規約、DPA、SLA、ゼロ保持条件の法務レビュー。
3. ゲートウェイから principal ごとのサーバーへの経路（推奨：同じ pod に Node 版ゲートウェイ）。
4. 承認後に実行する送信に別途手数料をかけるか。
5. ChatGPT に掲載する範囲（推奨：無料の読み取りツールだけ、課金は自社サイトへのリンク）。
6. Google（Gmail の制限付きスコープの審査を含む）・Slack・Linear・GitHub・Notion の OAuth アプリ、サーバーからの通信許可、プロバイダごとのステージング確認。
7. GitHub の `release`・`staging`・`production` 環境、npm と PyPI の trusted publishing。
8. 多言語埋め込みモデルのハッシュ固定と、モデルのあるマシンでの閾値調整。
9. 公開・デプロイ・ディレクトリ提出の実行の承認。

## リポジトリ構成

```
README.md                  本文書（英語）
README.ja.md               本文書（日本語）
CLAUDE.md                  このリポジトリで作業する際のルール
SKILL.md                   エージェント向けスキル：使いどころ、導入、費用、権限階層
SECURITY.md                脆弱性の報告方法
crates/                    Rust ワークスペース：エンジン、MCP/REST/CLI、評価とベンチ（memory、loop、pii、dedup、extract、fusion）
apps/api/                  ゲートウェイと中継（Cloudflare Workers + Postgres 上の Hono）：認証、計量、402、ウォレット、承認、enterprise、コネクタ、取り込みフック
apps/router/               ルーター：discover / inspect / run、自社コネクタ、Monid パススルー、精算アウトボックス
apps/docs/                 公開の Memory API ドキュメント
packages/sdk-ts, sdk-py    withShogunAgent() を持つ SDK
packages/cli               npx shogunagent
packages/conformance       MCP 適合スイート
packages/interop           損失報告付きの export v2 変換
packages/selection-eval    選択 eval のハーネスとゲート
deploy/                    Helm chart、Compose、BYOC の手順
bench/load/                負荷試験スクリプト
fuzz/                      cargo-fuzz のターゲット
integrations/monid/        Monid 掲載の準備
scripts/                   不変条件ガードと生成スクリプト
docs/implementation-plan.md  フェーズごとのエピックとタスク、状況
docs/MIGRATION.md          ShogunAI から何を移行し、何を移行せず、なぜか
docs/api/                  エージェント向け API 文書
docs/deploy/production.md  本番手順書
docs/security/             脅威モデル
docs/standards/            export v2 の CG 向け草案（未提出）
docs/benchmarks/           ベンチマーク結果と再現手順
docs/migrations/           マイグレーションごとのロールバック手順
reports/                   方針レポート全文（Markdown と日英 HTML）
research_notes/            出典付きの調査ノート
```

## 未解決の問い

- Mac アプリを持たない開発者は、取り込み API・webhook・コネクタだけで十分な状態を得られるか
- ルーター経由のエージェントは、ルーターを正当化する率で状態層に転換するか
- W3C の相互運用プロセスでどのエクスポート形式が勝ち、我々のものがそれになれるか
- 実ユーザーの言い回しに対して、open-loop の規則はモデルのレビュー無しでどこまで通用するか

4 つとも、追加の調査ではなく design partner の実測で答えが出る。
