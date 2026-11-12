// Seven scene-by-scene explainers. Each one opens with a chapter card, walks
// through three to five beats with burned-in captions (there is no voice-over),
// and closes on the repository. Claims follow README.md and docs/api/*; where the
// README lists a limit, the caption says it.
import { L, a, logo, icon, chat, userMsg, agentMsg, toolChip, note, store, stateCard, terminal, cmd, out, pill } from './ui.js';
import { STATE_CARDS, SOURCE_NOTES, hub, ladder, approvalCard } from './launch.js';

const TOTAL = 7;

const intro = (n, title, summary) => ({
  id: 'intro',
  dur: 3.6,
  bare: true,
  html: (t) => `<div class="ex-intro">
      <span class="ex-of" ${a('fade', 0.2, 0.6)}>${t(L(`Explainer ${n} of ${TOTAL}`, `解説 ${n} / ${TOTAL}`))}</span>
      <span class="ex-num" ${a('rise', 0.3, 0.8)}>${String(n).padStart(2, '0')}</span>
      <h1 ${a('rise', 0.6, 0.8)}>${t(title)}</h1>
      <p ${a('rise', 1.1, 0.8)}>${t(summary)}</p>
    </div>`,
});

const outro = (next) => ({
  id: 'outro',
  dur: 4.6,
  bare: true,
  html: (t) => `<div class="title-card cta-card ex-outro">
      ${logo('title-logo small', 0.2)}
      <div class="title-word" ${a('rise', 0.8, 0.7)}>ShogunAgent</div>
      <div class="cta-links" ${a('rise', 1.2, 0.8)}><span class="btn solid big">github.com/Torutesu/Shogunagent</span></div>
      <p class="cta-note" ${a('fade', 1.7, 0.8)}>${next ? t(L(`Next: ${next.en}`, `次の解説：${next.ja}`)) : t(L('Developer preview · Build from source · Apache-2.0', '開発者プレビュー · ソースからビルド · Apache-2.0'))}</p>
    </div>`,
});

const beatTitle = (t, n, text, start = 0.3) =>
  `<div class="beat-title" ${a('fade', start, 0.6)}><b>${n}</b>${t(text)}</div>`;

const QUOTE = L(
  'Let’s keep it short and direct. Alex should review before we publish. Friday could work, but we haven’t agreed on a date.',
  '短く、率直にいきましょう。公開前に Alex のレビューを。金曜でもよさそうですが、日付はまだ決めていません。',
);

const TITLES = [
  L('Save what matters', '大事なことを保存する'),
  L('Recall, with sources', '出典つきで呼び出す'),
  L('From sources to state', '出典から、状態へ'),
  L('Three faces, many agents', '3 つの入口、たくさんのエージェント'),
  L('Who gets to act', '誰が実行できるのか'),
  L('Local-first and portable', 'ローカルファースト、持ち運べる'),
  L('Run it in five steps', '5 つの手順で動かす'),
];

// ───────────────────────── 01 Save ─────────────────────────
const save = [
  {
    id: 'save-a',
    dur: 9.6,
    captions: [
      [0.4, L('At a useful checkpoint, your agent hands over what changed.', '区切りのよいところで、エージェントは「変わったこと」を渡します。')],
      [5.0, L('Not the whole transcript. Only the turns that changed something.', '会話の全文ではありません。何かを変えた発言だけです。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 1, L('A checkpoint, not a recording', '録音ではなく、区切りでの保存'))}
        ${chat({ cls: 'w760', attrs: a('rise', 0.5, 0.7), name: t(L('Planning agent', '企画エージェント')), sub: t(L('Monday · working session', '月曜日 · 作業セッション')), rows: [
          userMsg(t(QUOTE), a('rise', 0.8, 0.4), '0.9 70'),
          agentMsg(`<p>${t(L('Noted. I’ll save the decision, the next step, and the open question.', '了解です。決定、次の一手、未解決の点を保存します。'))}</p>`, a('rise', 3.4, 0.5)),
        ] })}
        <div class="keep">
          <div class="keep-box" ${a('rise', 4.0, 0.6)}><span class="eyebrow">${t(L('Saved', '保存するもの'))}</span>
            <ul>${[L('The decision', '決定'), L('The next step', '次の一手'), L('What’s still open', 'まだ決まっていないこと')].map((x, i) => `<li ${a('left', 4.4 + i * 0.3, 0.5)}>${icon.check}${t(x)}</li>`).join('')}</ul></div>
          <div class="keep-box off" ${a('rise', 5.6, 0.6)}><span class="eyebrow">${t(L('Left out', '保存しないもの'))}</span>
            <ul><li>${icon.cross}${t(L('The whole transcript', '会話の全文'))}</li><li>${icon.cross}${t(L('Small talk and retries', '雑談ややり直し'))}</li></ul></div>
        </div>
      </div>`,
  },
  {
    id: 'save-b',
    dur: 13,
    captions: [
      [0.4, L('Every write names its actor. A write without one is rejected.', 'すべての書き込みは、誰が書いたか（actor）を名乗ります。なければ拒否されます。')],
      [4.6, L('It also carries an idempotency key. Send it twice, and it is stored once.', '冪等キーも付きます。2 回送っても、保存されるのは 1 回です。')],
      [9.0, L('The events carry the source text: the words that prove a fact later.', 'events には出典の文章そのものが入ります。あとで事実を裏づける言葉です。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 2, L('What gets sent', '送られるもの'))}
        <div class="code w820" ${a('rise', 0.5, 0.7)}>
          <div class="code-bar"><span class="verb">POST</span>/v1/memory/ingest</div>
          <pre><span class="ln">{</span>
<span class="ln">  "source": "planning-agent",</span>
<span class="ln hl" data-v="hl 1.4 0.5; off 4.4 0.4">  "actor": "agent:planner",</span>
<span class="ln">  "thread_key": "website-launch",</span>
<span class="ln hl" data-v="hl 4.8 0.5; off 8.8 0.4">  "idempotency_key": "launch-notes-v1",</span>
<span class="ln hl" data-v="hl 9.2 0.5">  "events": [{ "content": "${t(L('Keep it short and direct…', '短く、率直に…'))}" }]</span>
<span class="ln">}</span></pre>
        </div>
        <div class="callouts">
          <div class="callout" ${a('right', 1.4, 0.6)}><code>actor</code><p>${t(L('Who wrote it. Agent or human, always named.', '誰が書いたか。エージェントでも人でも、必ず名乗る。'))}</p></div>
          <div class="callout" ${a('right', 4.8, 0.6)}><code>idempotency_key</code><p>${t(L('The same key replays the first result.', '同じキーなら、最初の結果をそのまま返す。'))}</p>
            <div class="resp"><span ${a('rise', 6.0, 0.5)}>${t(L('1st call', '1 回目'))} <code>inserted: 1</code></span><span ${a('rise', 7.0, 0.5)}>${t(L('2nd call', '2 回目'))} <code>replayed: true · inserted: 0</code></span></div></div>
          <div class="callout" ${a('right', 9.2, 0.6)}><code>events</code><p>${t(L('The source text itself.', '出典の文章そのもの。'))}</p></div>
        </div>
      </div>`,
  },
  {
    id: 'save-c',
    dur: 10.8,
    captions: [
      [0.4, L('On the way in, text is sanitized, secrets are masked, and exact duplicates collapse.', '取り込み時に、文章は無害化され、秘密情報は伏せられ、完全な重複はまとめられます。')],
      [5.6, L('Then it lands in your own encrypted store. A local write: nothing is sent anywhere.', 'そして、あなた専用の暗号化ストアに入ります。ローカルへの書き込みで、どこにも送信されません。')],
    ],
    html: (t) => `<div class="beat">
        ${beatTitle(t, 3, L('On the way in', '取り込みの途中で'))}
        <div class="pipe">
          <div class="pipe-src" ${a('rise', 0.5, 0.6)}><span class="eyebrow">${t(L('Incoming text', '届いた文章'))}</span><p>${t(L('Alex reviews first. Deploy token:', 'Alex が先にレビュー。デプロイ用トークン：'))} <span class="secret"><span class="raw" data-v="mask 2.4 0.5">tok_demo_001</span><span class="masked" data-v="mask 2.4 0.5">••••••••••••</span></span></p></div>
          <svg viewBox="0 0 1500 40" class="pipe-line"><path d="M0 20 H1500" ${a('draw', 0.9, 1.6)}/></svg>
          <div class="pipe-steps">
            ${[L('Sanitized', '無害化'), L('Secrets masked', '秘密情報を伏せる'), L('Duplicates collapsed', '重複をまとめる')].map((x, i) => `<span class="step" ${a('pop', 1.3 + i * 0.9, 0.6)}>${icon.check}${t(x)}</span>`).join('')}
            ${store({ attrs: a('pop', 5.4, 0.7), title: t(L('Your store', 'あなたのストア')), file: t(L('Encrypted · on your machine', '暗号化 · あなたのマシン上')) })}
          </div>
          <div class="pipe-foot" ${a('fade', 6.4, 0.7)}>${toolChip('memory.ingest', [t(L('L1 · local write', 'L1 · ローカル書き込み')), t(L('nothing sent', '送信なし'))])}</div>
        </div>
      </div>`,
  },
];

// ───────────────────────── 02 Recall ─────────────────────────
const recallPanel = (t, s) => `<div class="recall-panel" ${a('rise', s, 0.7)}>
    <div class="code-bar"><span class="verb">memory.recall</span>${t(L('result', '結果'))}</div>
    <div class="rp-band" ${a('rise', s + 0.5, 0.6)}><span class="band band-high">${t(L('Facts · High', '事実 · 確信度 高'))}</span><p>${t(L('Alex reviews the copy before publishing.', '公開前に Alex がコピーをレビューする。'))}</p></div>
    <div class="rp-band" ${a('rise', s + 3.4, 0.6)}><span class="band band-medium">${t(L('Medium', '確信度 中'))}</span><p><code>possibly:</code> ${t(L('the launch is on Friday.', '公開は金曜日。'))}</p></div>
    <div class="rp-band ev" ${a('rise', s + 7.2, 0.6)}><span class="band band-ev">${t(L('Evidence', '根拠'))}</span>
      <p><small>${t(L('Mon 10:42 · event 1042', '月曜 10:42 · イベント 1042'))}</small>${t(L('“Keep it short and direct. Alex should review before we publish.”', '「短く、率直に。公開前に Alex のレビューを。」'))}</p>
      <p ${a('rise', s + 8.0, 0.6)}><small>${t(L('Mon 10:43 · event 1043', '月曜 10:43 · イベント 1043'))}</small>${t(L('“Friday could work, but we haven’t agreed on a date.”', '「金曜でもよさそうですが、日付はまだ決めていません。」'))}</p></div>
  </div>`;

const recall = [
  {
    id: 'recall-a',
    dur: 9,
    captions: [
      [0.4, L('A new session starts with nothing. So the first call is recall.', '新しいセッションは、何も知らない状態で始まります。だから最初に recall を呼びます。')],
      [4.8, L('The agent asks its question and sets a token budget.', 'エージェントは質問を渡し、トークンの上限を指定します。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 1, L('The first call', '最初の呼び出し'))}
        ${chat({ cls: 'w760', attrs: a('rise', 0.5, 0.7), name: t(L('Coding agent', 'コーディングエージェント')), sub: t(L('Wednesday · a fresh conversation', '水曜日 · 新しい会話')), rows: [
          userMsg(t(L('Where are we with the website?', 'サイトの件、今どうなってる？')), a('rise', 0.9, 0.4), '1.0 30'),
          toolChip('memory.recall', [t(L('read', '読み取り')), t(L('free', '無料'))], a('rise', 2.6, 0.5)),
        ] })}
        <div class="code w640" ${a('right', 4.6, 0.7)}>
          <div class="code-bar"><span class="verb">memory.recall</span>${t(L('arguments', '引数'))}</div>
          <pre><span class="ln">{</span>
<span class="ln">  "query": "website",</span>
<span class="ln hl" data-v="hl 5.4 0.5">  "budget_tokens": 1000</span>
<span class="ln">}</span></pre>
        </div>
      </div>`,
  },
  {
    id: 'recall-b',
    dur: 13.4,
    captions: [
      [0.4, L('High-confidence facts come back as facts.', '確信度の高いものは、事実として返ります。')],
      [3.8, L('Medium ones are prefixed “possibly”, so no agent states a guess as true.', '中程度のものには「possibly」が付きます。推測を事実として語らせないためです。')],
      [7.8, L('Then dated evidence with event ids, until the budget is spent.', '続いて、日付とイベント ID の付いた根拠が、上限に達するまで並びます。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 2, L('What comes back', '返ってくるもの'))}
        ${recallPanel(t, 0.5)}
        <div class="budget" ${a('right', 0.9, 0.7)}><span class="eyebrow">budget_tokens</span><div class="meter"><i data-v="m1 1.0 1.0; m2 3.9 1.0; m3 7.7 2.4"></i></div><div class="budget-num"><b>1000</b><span>${t(L('tokens at most', 'トークンまで'))}</span></div>
          <p>${t(L('Facts first, then evidence. The answer always fits the budget you set.', 'まず事実、次に根拠。返答は必ず、指定した上限に収まります。'))}</p></div>
      </div>`,
  },
  {
    id: 'recall-c',
    dur: 13,
    captions: [
      [0.4, L('The agent answers from the source, and says what is still open.', 'エージェントは出典をもとに答え、まだ決まっていないことも伝えます。')],
      [4.8, L('Until a review step is configured, recall returns your saved sources as evidence, with no extracted facts.', 'レビュー処理を設定するまでは、保存した出典が根拠として返るだけで、抽出された事実は含まれません。')],
      [9.6, L('Reads are metered for observability, and never billed.', '読み取りは観測のために計測されますが、課金されることはありません。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 3, L('The answer', '答え'))}
        ${chat({ cls: 'w820', attrs: a('rise', 0.5, 0.7), name: t(L('Coding agent', 'コーディングエージェント')), sub: t(L('Wednesday · a fresh conversation', '水曜日 · 新しい会話')), rows: [
          userMsg(t(L('Where are we with the website?', 'サイトの件、今どうなってる？'))),
          agentMsg(`<p>${t(L('The copy should be short and direct. Alex still needs to review it before you publish.', 'コピーは短く率直に。公開前に Alex のレビューがまだ必要です。'))}</p>
            <p class="uncertain" ${a('rise', 1.8, 0.6)}><i></i>${t(L('The launch date is still undecided.', '公開日はまだ決まっていません。'))}</p>
            <div class="source-jump" ${a('rise', 2.6, 0.6)}>${icon.file}<span>${t(L('From your project note · saved Monday 10:42', 'プロジェクトのメモより · 月曜 10:42 に保存'))}</span></div>`, a('rise', 0.9, 0.5)),
        ] })}
        <div class="side-notes">
          <div class="callout" ${a('right', 4.8, 0.6)}><code>facts: [] · items: []</code><p>${t(L('Expected before review is configured. Sources still come back.', 'レビュー設定前は空で正常。出典は返ります。'))}</p></div>
          <div class="callout" ${a('right', 9.6, 0.6)}><code>${t(L('reads: free', '読み取り：無料'))}</code><p>${t(L('Billing attaches to ingest, model-invoking operations, tool runs and storage.', '課金対象は、取り込み、モデルを呼ぶ処理、ツールの実行、保存だけです。'))}</p></div>
        </div>
      </div>`,
  },
];

// ───────────────────────── 03 State ─────────────────────────
const logRows = (t) => [
  { h: L('Mon 10:00–11:00', '月曜 10:00–11:00'), rows: [[L('10:42', '10:42'), 'agent:planner', L('Keep it short and direct. Alex should review…', '短く、率直に。公開前に Alex のレビューを…')], [L('10:43', '10:43'), 'agent:planner', L('Friday could work, but no date agreed.', '金曜でもよさそうだが、日付は未定。')]] },
  { h: L('Wed 15:00–16:00', '水曜 15:00–16:00'), rows: [[L('15:10', '15:10'), 'agent:coder', L('Homepage copy is in. Waiting on Alex.', 'トップページのコピーを反映。Alex 待ち。')]] },
];

const state = [
  {
    id: 'state-a',
    dur: 9.8,
    captions: [
      [0.4, L('Saved sources join an event log.', '保存した出典は、イベントログに加わります。')],
      [3.8, L('A review step that you configure reads each completed hourly window.', 'あなたが設定したレビュー処理が、完了した 1 時間ごとの窓を読みます。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 1, L('The event log', 'イベントログ'))}
        <div class="log" ${a('rise', 0.5, 0.7)}>
          ${logRows(t).map((w, i) => `<div class="log-window" ${a('rise', 0.8 + i * 0.7, 0.6)} data-v="scan ${4.2 + i * 1.6} 1.2"><div class="log-h">${icon.clock}${t(w.h)}<span class="log-done">${t(L('completed window', '完了した窓'))}</span></div>
            ${w.rows.map((r) => `<div class="log-row"><time>${t(r[0])}</time><code>${r[1]}</code><span>${t(r[2])}</span></div>`).join('')}</div>`).join('')}
        </div>
        <div class="review-box" ${a('right', 3.8, 0.7)}><span class="review-node big">${t(L('Review', 'レビュー'))}</span><p>${t(L('Runs on the provider you configure. Not part of the basic quickstart.', '設定したプロバイダで動きます。基本のクイックスタートには含まれません。'))}</p></div>
      </div>`,
  },
  {
    id: 'state-b',
    dur: 11.4,
    captions: [
      [0.4, L('It proposes people, projects, commitments and open loops.', 'レビューは、人・プロジェクト・約束・未完了の仕事を提案します。')],
      [5.6, L('Each one is tied to the quote that proves it.', 'その一つひとつが、裏づけとなる引用に結びついています。')],
    ],
    html: (t) => `<div class="beat">
        ${beatTitle(t, 2, L('Typed state', '型付きの状態'))}
        <div class="state-flow tall">
          <div class="state-notes">${SOURCE_NOTES(t).map((n, i) => note({ cls: 'compact', attrs: a('left', 0.5 + i * 0.25, 0.6), meta: n.meta, quote: n.quote })).join('')}</div>
          <div class="state-mid"><svg viewBox="0 0 200 420" class="wires"><path d="M0 70 C110 70 90 210 200 210" ${a('draw', 1.2, 0.8)}/><path d="M0 210 L200 210" ${a('draw', 1.3, 0.8)}/><path d="M0 350 C110 350 90 210 200 210" ${a('draw', 1.4, 0.8)}/></svg><span class="review-node" ${a('pop', 1.6, 0.6)}>${t(L('Review', 'レビュー'))}</span></div>
          <div class="state-grid">${STATE_CARDS(t).map((c, i) => stateCard({ ...c, attrs: `${a('rise', 2.1 + i * 0.55, 0.7)} data-v="prov 5.8 0.6"` })).join('')}</div>
        </div>
      </div>`,
  },
  {
    id: 'state-c',
    dur: 13,
    captions: [
      [0.4, L('A state row without provenance is rejected at the repository layer.', '出典（provenance）のない状態は、リポジトリ層で拒否されます。')],
      [4.8, L('Confidence decides how a row is shown: as a fact, as “possibly”, or not as a fact at all.', '確信度が見せ方を決めます。事実として、「possibly」として、あるいは事実としては見せない。')],
      [9.2, L('A person or project appears normally only after evidence in two separate hourly sessions.', '人やプロジェクトは、別々の 1 時間のセッション 2 つに根拠があって、初めて通常どおり表示されます。')],
    ],
    html: (t) => {
      const rows = [
        { title: L('Alex reviews the copy before publishing', '公開前に Alex がコピーをレビュー'), prov: L('quote · Mon 10:42', '引用 · 月曜 10:42'), band: 'high', bandLabel: L('High', '高'), verdict: L('Shown as a fact', '事実として表示'), ok: true },
        { title: L('The launch is on Friday', '公開は金曜日'), prov: L('quote · Mon 10:43', '引用 · 月曜 10:43'), band: 'medium', bandLabel: L('Medium', '中'), verdict: L('Shown as “possibly”', '「possibly」として表示'), ok: true },
        { title: L('Budget approved by finance', '経理が予算を承認'), prov: L('no provenance', '出典なし'), band: 'none', bandLabel: L('—', '—'), verdict: L('Rejected', '拒否'), ok: false },
        { title: L('Sam owns the pricing page', '料金ページの担当は Sam'), prov: L('quote · one mention', '引用 · 言及 1 回'), band: 'low', bandLabel: L('Low', '低'), verdict: L('Never rendered as fact', '事実としては表示しない'), ok: false },
      ];
      return `<div class="beat">
        ${beatTitle(t, 3, L('The gate', '関門'))}
        <div class="gate">
          <div class="gate-head" ${a('fade', 0.5, 0.6)}><span>${t(L('Proposed row', '提案された行'))}</span><span>${t(L('Provenance', '出典'))}</span><span>${t(L('Confidence', '確信度'))}</span><span>${t(L('Result', '結果'))}</span></div>
          ${rows.map((r, i) => `<div class="gate-row ${r.ok ? '' : 'no'}" ${a('rise', 0.8 + i * 0.4, 0.6)}><span class="gate-title">${t(r.title)}</span><span class="gate-prov">${r.band === 'none' ? icon.cross : icon.file}${t(r.prov)}</span><span class="band band-${r.band}">${t(r.bandLabel)}</span><span class="gate-verdict" ${a('left', [5.2, 6.0, 2.6, 6.8][i], 0.6)}>${r.ok ? icon.check : icon.cross}${t(r.verdict)}</span></div>`).join('')}
        </div>
      </div>`;
    },
  },
  {
    id: 'state-d',
    dur: 11,
    captions: [
      [0.4, L('When the work changes, the state follows: a new due date…', '仕事が変われば、状態も追従します。期限が変わったとき…')],
      [5.6, L('…or a completed commitment. The earlier evidence stays.', '…あるいは約束が完了したとき。以前の根拠は残ります。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 4, L('State over time', '時間とともに変わる状態'))}
        <div class="tl-notes">
          ${note({ cls: 'compact', attrs: a('left', 0.5, 0.6), meta: t(L('Mon 10:42', '月曜 10:42')), quote: t(L('“Alex will review the copy on Thursday.”', '「Alex が木曜日にコピーをレビューします。」')) })}
          ${note({ cls: 'compact', attrs: a('left', 2.2, 0.6), meta: t(L('Wed 15:10', '水曜 15:10')), quote: t(L('“Alex asked to move the review to Friday.”', '「Alex から、レビューを金曜日に変えたいとのこと。」')) })}
          ${note({ cls: 'compact', attrs: a('left', 6.0, 0.6), meta: t(L('Fri 11:05', '金曜 11:05')), quote: t(L('“Alex has reviewed the copy. Approved.”', '「Alex がコピーをレビュー済み。承認です。」')) })}
        </div>
        <div class="sc sc-commitment evolving" ${a('rise', 0.9, 0.7)}>
          <div class="sc-top"><span class="sc-glyph">${icon.flag}</span><span class="eyebrow">${t(L('Commitment', '約束'))}</span>
            <span class="status-swap"><span class="band band-high" data-v="gone 7.0 0.5">${t(L('Open', '未完了'))}</span><span class="band band-done" data-v="here 7.0 0.5">${icon.check}${t(L('Done', '完了'))}</span></span></div>
          <h3>${t(L('Alex reviews the copy before publishing', '公開前に Alex がコピーをレビュー'))}</h3>
          <div class="due"><span>${t(L('Due', '期限'))}</span><b class="due-old" data-v="strike 3.2 0.5">${t(L('Thursday', '木曜日'))}</b><b class="due-new" ${a('left', 3.5, 0.6)}>${t(L('Friday', '金曜日'))}</b></div>
          <ul class="history"><li ${a('rise', 1.3, 0.5)}>${icon.file}${t(L('Created from Mon 10:42', '月曜 10:42 の出典から作成'))}</li><li ${a('rise', 3.9, 0.5)}>${icon.file}${t(L('Due date changed · Wed 15:10', '期限を変更 · 水曜 15:10'))}</li><li ${a('rise', 7.4, 0.5)}>${icon.file}${t(L('Completed · Fri 11:05', '完了 · 金曜 11:05'))}</li></ul>
        </div>
      </div>`,
  },
];

// ───────────────────────── 04 Faces ─────────────────────────
const CORE_TOOLS = ['memory.search', 'memory.get_context', 'memory.get_context_pack', 'memory.get_events', 'memory.read_session', 'memory.get_wrap', 'memory.get_periods', 'memory.recall', 'calendar.upcoming', 'profile.whoami', 'state.commitments.list', 'state.open_loops.list', 'state.people.list', 'state.projects.list', 'state.people.get', 'state.projects.get', 'wiki.evidence', 'actions.status', 'memory.append_note', 'memory.ingest', 'actions.execute'];

const faces = [
  {
    id: 'faces-a',
    dur: 11.4,
    captions: [
      [0.4, L('Every capability has three faces: MCP, REST and a CLI.', 'すべての機能には 3 つの入口があります。MCP、REST、CLI です。')],
      [4.8, L('Same permission level, same result. A tool that exists on one exists on all three.', '権限レベルも結果も同じです。1 つの入口にあるツールは、3 つすべてにあります。')],
    ],
    html: (t) => {
      const cols = [
        { face: 'MCP', lines: ['tools/call', 'handoff.create'] },
        { face: 'REST', lines: ['POST /v1/handoff', '{ "task": "…" }'] },
        { face: 'CLI', lines: ['shogun handoff create', '\'{"task":"…"}\''] },
      ];
      return `<div class="beat">
        ${beatTitle(t, 1, L('One capability, three faces', '1 つの機能、3 つの入口'))}
        <div class="faces3">
          ${cols.map((c, i) => `<div class="code face-card" ${a('rise', 0.5 + i * 0.4, 0.7)}><div class="code-bar"><span class="verb">${c.face}</span></div><pre>${c.lines.map((x) => `<span class="ln">${x.replace(/</g, '&lt;')}</span>`).join('\n')}</pre></div>`).join('')}
        </div>
        <svg viewBox="0 0 1500 110" class="merge"><path d="M250 0 C250 70 750 40 750 110" ${a('draw', 4.6, 0.7)}/><path d="M750 0 V110" ${a('draw', 4.7, 0.7)}/><path d="M1250 0 C1250 70 750 40 750 110" ${a('draw', 4.8, 0.7)}/></svg>
        <div class="same" ${a('pop', 5.4, 0.7)}>${icon.check}<span>${t(L('Same permission level', '同じ権限レベル'))}</span><i></i>${icon.check}<span>${t(L('Same result', '同じ結果'))}</span></div>
      </div>`;
    },
  },
  {
    id: 'faces-b',
    dur: 9.6,
    captions: [
      [0.4, L('So every client you use can connect to the same local store.', 'だから、あなたが使うどのクライアントも、同じローカルのストアにつなげます。')],
      [4.8, L('Each client is configured separately. The repo records which ones are verified so far.', 'クライアントごとに設定します。どれが確認済みかは、リポジトリに記録しています。')],
    ],
    html: (t) => `<div class="beat">
        ${beatTitle(t, 2, L('Many agents, one store', 'たくさんのエージェント、1 つのストア'))}
        <div class="hub-wrap">${hub(t, 0.5)}</div>
      </div>`,
  },
  {
    id: 'faces-c',
    dur: 11.6,
    captions: [
      [0.4, L('One agent can hand a task to another, together with the state it touches.', 'エージェントは、タスクと、それに関わる状態を、別のエージェントに引き継げます。')],
      [5.0, L('Every row carries the quote that proves it. Low-confidence rows never travel.', 'すべての行に、裏づけとなる引用が付きます。確信度の低い行は引き継がれません。')],
    ],
    html: (t) => `<div class="beat">
        ${beatTitle(t, 3, L('Handoff', '引き継ぎ'))}
        <div class="handoff">
          <div class="agent-box" ${a('left', 0.5, 0.7)}><span class="chat-glyph">✳</span>${t(L('Planning agent', '企画エージェント'))}<small>handoff.create</small></div>
          <svg viewBox="0 0 160 40" class="hand-arrow"><path d="M0 20 H150 M132 6 L150 20 L132 34" ${a('draw', 1.2, 0.6)}/></svg>
          <div class="hand-card" ${a('pop', 1.6, 0.7)}>
            <span class="eyebrow">${t(L('State-of-work handoff', '作業状態の引き継ぎ'))}</span>
            <h3>${t(L('Task: draft the launch update', 'タスク：公開のお知らせを下書きする'))}</h3>
            <ul>
              <li ${a('rise', 2.2, 0.5)}>${icon.folder}<span>${t(L('Project · Website launch', 'プロジェクト · サイト公開'))}</span></li>
              <li ${a('rise', 2.6, 0.5)}>${icon.flag}<span>${t(L('Commitment · Alex reviews the copy', '約束 · Alex がコピーをレビュー'))}</span></li>
              <li ${a('rise', 3.0, 0.5)}>${icon.file}<span>${t(L('Quote · “Alex should review before we publish.”', '引用 · 「公開前に Alex のレビューを。」'))}</span></li>
            </ul>
            <div class="hand-foot" ${a('fade', 5.2, 0.6)}>${pill(t(L('High and Medium only', '確信度 高・中のみ')), 'band band-high')}${pill(t(L('Valid until Fri 18:00', '有効期限：金曜 18:00')), 'band band-ev')}</div>
          </div>
          <svg viewBox="0 0 160 40" class="hand-arrow"><path d="M0 20 H150 M132 6 L150 20 L132 34" ${a('draw', 3.6, 0.6)}/></svg>
          <div class="agent-box" ${a('right', 4.0, 0.7)}><span class="chat-glyph">✳</span>${t(L('Writing agent', 'ライティングエージェント'))}<small>handoff.accept</small></div>
        </div>
      </div>`,
  },
  {
    id: 'faces-d',
    dur: 9.4,
    captions: [
      [0.4, L('The 21 core tool names and their schemas are frozen and snapshot-tested.', '21 個のコアツールの名前とスキーマは凍結され、スナップショットでテストされています。')],
      [4.8, L('A change means a new name and a deprecation note. Never an edit in place.', '変更するときは、新しい名前と非推奨の告知を出します。その場で書き換えることはありません。')],
    ],
    html: (t) => `<div class="beat">
        ${beatTitle(t, 4, L('Names that stay put', '変わらない名前'))}
        <div class="toolgrid">${CORE_TOOLS.map((name, i) => `<code ${a('rise', 0.5 + i * 0.07, 0.5)}>${name}</code>`).join('')}<span class="frozen" ${a('pop', 2.6, 0.7)}>${icon.lock}${t(L('21 names · frozen', '21 個の名前 · 凍結'))}</span></div>
      </div>`,
  },
];

// ───────────────────────── 05 Permissions ─────────────────────────
const perms = [
  {
    id: 'perms-a',
    dur: 11.8,
    captions: [
      [0.4, L('Every action has a level. L1 reads, and saves to your own store.', 'すべての操作にレベルがあります。L1 は読み取りと、自分のストアへの保存です。')],
      [4.2, L('L2 changes something of yours, so it asks for confirmation first.', 'L2 はあなたのものを変更するので、先に確認を求めます。')],
      [7.8, L('L3 leaves your machine: sending, posting, paying. Always a human decision.', 'L3 は外に出ていく操作です。送信・投稿・支払い。必ず人が決めます。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 1, L('Three levels', '3 つのレベル'))}
        <div class="ladder-big">${ladder(t, 0.5, 7.8)}</div>
        <div class="examples">
          <div class="callout" ${a('right', 0.9, 0.6)}><span class="eyebrow">L1</span><div class="chips"><code>memory.recall</code><code>memory.ingest</code><code>state.*</code></div></div>
          <div class="callout" ${a('right', 4.2, 0.6)}><span class="eyebrow">L2</span><p>${t(L('Answers “pending” until you confirm.', '確認するまで「保留」と答えます。'))}</p></div>
          <div class="callout" ${a('right', 7.8, 0.6)}><span class="eyebrow">L3</span><div class="chips"><code>send_email</code><code>post_message</code><code>post_comment</code></div></div>
        </div>
      </div>`,
  },
  {
    id: 'perms-b',
    dur: 12.4,
    captions: [
      [0.4, L('Ask an agent to send an email. The engine does not send it.', 'エージェントにメールの送信を頼んでも、エンジンは送信しません。')],
      [4.4, L('It answers “approval required”, with an id the agent can poll.', '「承認が必要」と答え、エージェントが確認に使える ID を返します。')],
      [8.4, L('The draft waits for you.', '下書きは、あなたを待ちます。')],
    ],
    html: (t) => {
      const steps = [
        { who: L('Agent', 'エージェント'), code: 'actions.execute', arg: 'kind: send_email', at: 0.6 },
        { who: L('Engine', 'エンジン'), code: 'approval_required', arg: 'approval_id: apr_27', at: 4.4 },
        { who: L('Agent', 'エージェント'), code: 'actions.status', arg: 'pending', at: 8.8 },
      ];
      return `<div class="beat cols">
        ${beatTitle(t, 2, L('What an L3 call returns', 'L3 の呼び出しが返すもの'))}
        <div class="seq">${steps.map((s, i) => `<div class="seq-step" ${a('left', s.at, 0.6)}><b>${i + 1}</b><span class="seq-who">${t(s.who)}</span><code>${s.code}</code><span class="seq-arg">${s.arg}</span></div>`).join('')}</div>
        ${approvalCard(t, 5.6)}
      </div>`;
    },
  },
  {
    id: 'perms-c',
    dur: 10.4,
    captions: [
      [0.4, L('Approved is not the same as delivered. The status says what actually happened.', '承認は、届いたことの証明ではありません。実際に何が起きたかは、ステータスが示します。')],
      [5.4, L('An agent must never report a send as done before the status says “sent”.', 'ステータスが「sent」になるまで、エージェントは送信済みと報告してはいけません。')],
    ],
    html: (t) => {
      const states = ['pending', 'sent', 'rejected', 'timed_out', 'send_failed', 'draft_saved', 'outcome_unknown'];
      return `<div class="beat">
        ${beatTitle(t, 3, L('Approved is not delivered', '承認と、送信完了は別'))}
        <div class="statuses">
          <div class="code-bar solo" ${a('fade', 0.5, 0.6)}><span class="verb">actions.status</span>${t(L('one of', '次のいずれか'))}</div>
          <div class="status-row">${states.map((s, i) => `<code class="st st-${s}" ${a('rise', 0.8 + i * 0.25, 0.5)} ${s === 'sent' ? 'data-v="hl 5.6 0.6"' : ''}>${s}</code>`).join('')}</div>
          <p class="status-rule" ${a('rise', 5.8, 0.7)}>${icon.shield}${t(L('Only “sent” means it went out.', '外に出たと言えるのは「sent」だけ。'))}</p>
        </div>
      </div>`;
    },
  },
  {
    id: 'perms-d',
    dur: 9.6,
    captions: [
      [0.4, L('The rules do not depend on who is asking. Agent or human, an external send is L3.', 'ルールは、頼む相手によって変わりません。エージェントでも人でも、外部への送信は L3 です。')],
      [5.2, L('And an operation the engine does not recognize is denied.', 'そして、エンジンが知らない操作は拒否されます。')],
    ],
    html: (t) => {
      const rules = [
        { glyph: icon.send, h: L('External send', '外部への送信'), p: L('Always L3', '常に L3') },
        { glyph: icon.person, h: L('Every caller', 'すべての呼び出し元'), p: L('Agent or human, same rule', 'エージェントも人も、同じルール') },
        { glyph: icon.cross, h: L('Not in the table', '表にない操作'), p: L('Denied', '拒否') },
      ];
      return `<div class="beat">
        ${beatTitle(t, 4, L('No exceptions by caller', '呼び出し元による例外なし'))}
        <div class="tiles rules">${rules.map((r, i) => `<div class="tile" ${a('rise', [0.5, 0.9, 5.2][i], 0.7)}><span class="tile-glyph">${r.glyph}</span><h3>${t(r.h)}</h3><p>${t(r.p)}</p></div>`).join('')}</div>
      </div>`;
    },
  },
];

// ───────────────────────── 06 Local-first ─────────────────────────
const local = [
  {
    id: 'local-a',
    dur: 10.8,
    captions: [
      [0.4, L('One person, one encrypted file.', '1 人につき、暗号化されたファイルが 1 つ。')],
      [3.8, L('Tenancy is a file boundary, not a filter in a query. Another person’s rows are simply not in your file.', 'テナントの境界はファイルそのものです。クエリの条件ではありません。他人の行は、あなたのファイルにそもそも存在しません。')],
    ],
    html: (t) => {
      const people = [L('You', 'あなた'), 'Alex', 'Maya'];
      return `<div class="beat">
        ${beatTitle(t, 1, L('One principal, one file', '1 人に 1 ファイル'))}
        <div class="principals">${people.map((p, i) => `${i ? `<span class="wall" ${a('fade', 3.8 + i * 0.2, 0.6)}></span>` : ''}<div class="principal" ${a('rise', 0.5 + i * 0.35, 0.7)}><span class="avatar">${icon.person}</span><b>${typeof p === 'string' ? p : t(p)}</b>${store({ title: t(L('Encrypted store', '暗号化ストア')), file: 'SQLCipher' })}</div>`).join('')}</div>
        <p class="footnote" ${a('fade', 4.6, 0.7)}>${t(L('Sharing across people happens only through explicit access lists, never by query.', '人をまたぐ共有は、明示的なアクセスリストを通してのみ行われます。クエリでは行いません。'))}</p>
      </div>`;
    },
  },
  {
    id: 'local-b',
    dur: 13.4,
    captions: [
      [0.4, L('The Rust core owns the data: storage, permission decisions and the approval queue.', 'データを持つのは Rust のコアです。保存、権限の判断、承認キューを受け持ちます。')],
      [4.8, L('The optional gateway does auth and metering. It is designed to relay, never to store request or response bodies.', '任意のゲートウェイは認証と計量を担当します。中継するだけで、リクエストやレスポンスの本文を保存しない設計です。')],
      [10.2, L('Public hosted access is not available yet. Today, you run it yourself.', '公開のホスト型サービスはまだありません。現時点では、自分で動かします。')],
    ],
    html: (t) => `<div class="beat">
        ${beatTitle(t, 2, L('Who holds what', '誰が何を持つか'))}
        <div class="arch">
          <div class="arch-box" ${a('rise', 0.5, 0.7)}><span class="chat-glyph">✳</span><h3>${t(L('Your agent', 'あなたのエージェント'))}</h3><p>MCP · REST · CLI</p></div>
          <svg viewBox="0 0 120 40" class="hand-arrow"><path d="M0 20 H110 M94 7 L110 20 L94 33" ${a('draw', 4.6, 0.5)}/></svg>
          <div class="arch-box gw" ${a('rise', 4.8, 0.7)}><span class="eyebrow">${t(L('Optional · hosted', '任意 · ホスト型'))}</span><h3>${t(L('Gateway', 'ゲートウェイ'))}</h3><p>${t(L('Auth · metering · relay', '認証 · 計量 · 中継'))}</p><div class="nobody" ${a('rise', 6.0, 0.6)}>${icon.cross}${t(L('No bodies stored', '本文は保存しない'))}</div></div>
          <svg viewBox="0 0 120 40" class="hand-arrow"><path d="M0 20 H110 M94 7 L110 20 L94 33" ${a('draw', 5.2, 0.5)}/></svg>
          <div class="arch-box core" ${a('rise', 0.9, 0.7)}><span class="eyebrow">${t(L('Rust core', 'Rust コア'))}</span><h3>${t(L('Engine', 'エンジン'))}</h3><p>${t(L('Data · permissions · approvals', 'データ · 権限 · 承認'))}</p>${store({ attrs: a('rise', 1.5, 0.6), title: t(L('Your store', 'あなたのストア')), file: t(L('Encrypted file', '暗号化ファイル')) })}</div>
        </div>
      </div>`,
  },
  {
    id: 'local-c',
    dur: 10.8,
    captions: [
      [0.4, L('Your state is portable. Export people, projects, commitments and open loops with their evidence.', '状態は持ち運べます。人・プロジェクト・約束・未完了の仕事を、根拠ごと書き出せます。')],
      [5.8, L('Import it into a fresh store, and the quotes, relations and timestamps come with it.', '新しいストアに取り込めば、引用・関係・日時もそのまま付いてきます。')],
    ],
    html: (t) => {
      const lines = [
        [icon.person, L('person', 'person'), 'Alex'],
        [icon.folder, L('project', 'project'), L('Website launch', 'サイト公開')],
        [icon.flag, L('commitment', 'commitment'), L('Alex reviews the copy', 'Alex がコピーをレビュー')],
        [icon.loop, L('open_loop', 'open_loop'), L('Launch date not confirmed', '公開日が未確定')],
      ];
      return `<div class="beat">
        ${beatTitle(t, 3, L('Take it with you', '持って出られる'))}
        <div class="export">
          ${store({ attrs: a('left', 0.5, 0.7), title: t(L('Your store', 'あなたのストア')), file: t(L('Encrypted file', '暗号化ファイル')) })}
          <svg viewBox="0 0 120 40" class="hand-arrow"><path d="M0 20 H110 M94 7 L110 20 L94 33" ${a('draw', 1.0, 0.5)}/></svg>
          <div class="code jsonl" ${a('pop', 1.3, 0.7)}><div class="code-bar"><span class="verb">state.export</span>export-v2 · JSON Lines</div>
            <div class="jl">${lines.map((l, i) => `<div class="jl-row" ${a('rise', 1.8 + i * 0.35, 0.5)}>${l[0]}<code>${t(l[1])}</code><span>${typeof l[2] === 'string' ? l[2] : t(l[2])}</span><small>${t(L('confidence · evidence · timestamps', '確信度 · 根拠 · 日時'))}</small></div>`).join('')}</div></div>
          <svg viewBox="0 0 120 40" class="hand-arrow"><path d="M0 20 H110 M94 7 L110 20 L94 33" ${a('draw', 5.8, 0.5)}/></svg>
          ${store({ attrs: a('right', 6.2, 0.7), title: t(L('A fresh store', '新しいストア')), file: 'memory.import' })}
        </div>
      </div>`;
    },
  },
  {
    id: 'local-d',
    dur: 8.6,
    captions: [
      [0.4, L('The engine is open source under Apache-2.0.', 'エンジンは Apache-2.0 のオープンソースです。')],
      [3.8, L('You bring the infrastructure, and a model provider if you want extraction.', '用意するのは動かす環境と、抽出を使うならモデルのプロバイダです。')],
    ],
    html: (t) => {
      const tiles = [
        { glyph: icon.code, h: L('Open engine', 'オープンなエンジン'), p: L('Apache-2.0', 'Apache-2.0') },
        { glyph: icon.box, h: L('Your infrastructure', 'あなたの環境'), p: L('Linux and macOS, built from source', 'Linux / macOS。ソースからビルド') },
        { glyph: icon.shield, h: L('Your model provider', 'あなたのモデルプロバイダ'), p: L('Only if you configure extraction', '抽出を設定する場合のみ') },
      ];
      return `<div class="beat">
        ${beatTitle(t, 4, L('Open, and yours to run', 'オープンで、自分で動かせる'))}
        <div class="tiles rules">${tiles.map((r, i) => `<div class="tile" ${a('rise', [0.5, 3.8, 4.2][i], 0.7)}><span class="tile-glyph">${r.glyph}</span><h3>${t(r.h)}</h3><p>${t(r.p)}</p></div>`).join('')}</div>
      </div>`;
    },
  },
];

// ───────────────────────── 07 Quickstart ─────────────────────────
const side = (items) => `<div class="term-side">${items.join('')}</div>`;
const sideNote = (t, label, text, at) => `<div class="callout" ${a('right', at, 0.6)}><span class="eyebrow">${t(label)}</span><p>${t(text)}</p></div>`;
const EXPECT = L('Expect', '期待される結果');

const quick = [
  {
    id: 'quick-a',
    dur: 10,
    captions: [
      [0.4, L('Build the server from source. You need Git, Rust stable and a C toolchain.', 'サーバーをソースからビルドします。必要なのは Git、Rust stable、C のツールチェーンです。')],
      [5.2, L('Linux and macOS. There are no published npm or pip packages yet.', '対応は Linux と macOS。npm や pip のパッケージは、まだ公開していません。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 1, L('Build', 'ビルドする'))}
        ${terminal({ attrs: a('rise', 0.5, 0.7), title: t(L('Terminal 1', 'ターミナル 1')), lines: [
          cmd('git clone https://github.com/Torutesu/Shogunagent.git', 0.9, 55),
          cmd('cd Shogunagent', 2.2, 40),
          cmd('cargo build --release --locked -p shogun-core \\', 2.8, 60),
          `<div class="tl cmd cont"><span data-type="3.7 60">  --features daemon-server --bin shogun-api --bin shogun-mcp</span></div>`,
        ] })}
        ${side([sideNote(t, L('You need', '必要なもの'), L('Git · Rust stable · C/C++ toolchain · Python 3 · curl', 'Git · Rust stable · C/C++ ツールチェーン · Python 3 · curl'), 1.2), sideNote(t, L('Not needed here', 'ここでは不要'), L('Node and pnpm. No `npx`, no `pip install`.', 'Node と pnpm。`npx` も `pip install` も使いません。'), 5.2)])}
      </div>`,
  },
  {
    id: 'quick-b',
    dur: 10.4,
    captions: [
      [0.4, L('Create an evaluation key and token once. Keep them out of chat, logs and Git.', '評価用の鍵とトークンを一度だけ作ります。チャット、ログ、Git には貼らないでください。')],
      [5.0, L('Start the server. It listens on your own machine: 127.0.0.1, port 7464.', 'サーバーを起動します。待ち受けるのは自分のマシンだけ。127.0.0.1 のポート 7464 です。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 2, L('Keys, then start', '鍵を作り、起動する'))}
        ${terminal({ attrs: a('rise', 0.5, 0.7), title: t(L('Terminal 1', 'ターミナル 1')), lines: [
          out(t(L('# README step 2 writes two private files:', '# README の手順 2 で、非公開のファイルを 2 つ作ります：')), a('fade', 0.8, 0.5), 'dim'),
          out('.shogun-demo/db.key &nbsp; .shogun-demo/api.token', a('fade', 1.3, 0.5)),
          cmd('export SHOGUN_API_PORT=7464', 4.6, 50),
          cmd('./target/release/shogun-api \\', 5.3, 55),
          `<div class="tl cmd cont"><span data-type="6.0 60">  --multi "$PWD/.shogun-demo/principals" --key-provider env</span></div>`,
        ] })}
        ${side([sideNote(t, L('Private', '非公開'), L('Mode 0600, ignored by Git. Replacing a key does not reset the store.', 'モード 0600、Git の対象外。鍵を置き換えてもストアはリセットされません。'), 1.6), sideNote(t, L('Listening on', '待ち受け先'), L('`127.0.0.1:7464`. An API, not a dashboard.', '`127.0.0.1:7464`。API であり、画面のあるダッシュボードではありません。'), 7.4)])}
      </div>`,
  },
  {
    id: 'quick-c',
    dur: 12,
    captions: [
      [0.4, L('In a second terminal, save one line of synthetic text.', '2 つ目のターミナルで、合成したテキストを 1 行保存します。')],
      [5.0, L('One event inserted. Send it again and it comes back as a replay: nothing new is stored.', '1 件が挿入されます。もう一度送ると再送として返り、新しく保存されるものはありません。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 3, L('Save', '保存する'))}
        ${terminal({ attrs: a('rise', 0.5, 0.7), title: t(L('Terminal 2', 'ターミナル 2')), lines: [
          cmd('curl http://127.0.0.1:7464/v1/memory/ingest \\', 0.9, 60),
          `<div class="tl cmd cont"><span data-type="1.7 70">  -H "Authorization: Bearer $SHOGUN_DEMO_TOKEN" \\</span></div>`,
          `<div class="tl cmd cont"><span data-type="2.5 90">  --data '{"source":"website-demo","actor":"agent:website-demo",</span></div>`,
          `<div class="tl cmd cont"><span data-type="3.2 90">    "thread_key":"kestrel-demo","idempotency_key":"kestrel-demo-v1",</span></div>`,
          `<div class="tl cmd cont"><span data-type="3.9 90">    "events":[{"content":"The Kestrel demo codeword is violet."}]}'</span></div>`,
        ] })}
        ${side([sideNote(t, L('First call', '1 回目'), L('`accepted 1 · inserted 1`', '`accepted 1 · inserted 1`'), 5.0), sideNote(t, L('Same key again', '同じキーでもう一度'), L('`replayed: true · inserted: 0`', '`replayed: true · inserted: 0`'), 7.6)])}
      </div>`,
  },
  {
    id: 'quick-d',
    dur: 11.6,
    captions: [
      [0.4, L('Now recall it. The evidence contains the sentence you saved.', '次に呼び出します。根拠の中に、保存した一文が入っています。')],
      [5.4, L('Typed state is still empty here. That is expected: no review step is configured.', 'この段階では、型付きの状態は空です。レビュー処理を設定していないので、想定どおりです。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 4, L('Recall', '呼び出す'))}
        ${terminal({ attrs: a('rise', 0.5, 0.7), title: t(L('Terminal 2', 'ターミナル 2')), lines: [
          cmd('curl http://127.0.0.1:7464/v1/memory/recall \\', 0.9, 60),
          `<div class="tl cmd cont"><span data-type="1.7 70">  -H "Authorization: Bearer $SHOGUN_DEMO_TOKEN" \\</span></div>`,
          `<div class="tl cmd cont"><span data-type="2.5 80">  --data '{"query":"Kestrel","budget_tokens":1000}'</span></div>`,
        ] })}
        ${side([sideNote(t, EXPECT, L('Evidence includes “The Kestrel demo codeword is violet.”', '根拠に “The Kestrel demo codeword is violet.” が含まれる'), 3.6), sideNote(t, L('Also expected', 'これも想定どおり'), L('`items` is empty. Saved sources and reviewed state are separate outputs.', '`items` は空。保存した出典と、レビュー済みの状態は別の出力です。'), 5.4)])}
      </div>`,
  },
  {
    id: 'quick-e',
    dur: 10.6,
    captions: [
      [0.4, L('Last, check the MCP bridge. Three checks should pass.', '最後に、MCP のブリッジを確認します。3 つのチェックが通るはずです。')],
      [5.2, L('No model call. No hosted account. Then connect your coding agent.', 'モデルの呼び出しも、ホスト型のアカウントも不要です。あとは、コーディングエージェントをつなぐだけ。')],
    ],
    html: (t) => `<div class="beat cols">
        ${beatTitle(t, 5, L('Check the bridge', 'ブリッジを確認する'))}
        ${terminal({ attrs: a('rise', 0.5, 0.7), title: t(L('Terminal 2', 'ターミナル 2')), lines: [
          cmd('python3 scripts/mcp-local-bridge.py \\', 0.9, 55),
          `<div class="tl cmd cont"><span data-type="1.7 70">  --token-file "$PWD/.shogun-demo/api.token" --check</span></div>`,
        ] })}
        <div class="term-side checks">
          <span class="eyebrow" ${a('fade', 2.8, 0.5)}>${t(L('Expect three PASS lines', 'PASS が 3 行出れば正常'))}</span>
          ${['initialize', 'tools/list', 'tools/call'].map((x, i) => `<div class="check-row" ${a('right', 3.2 + i * 0.5, 0.5)}>${icon.check}<b>PASS</b><code>${x}</code></div>`).join('')}
          <div class="callout" ${a('right', 5.4, 0.6)}><span class="eyebrow">${t(L('Next', '次は'))}</span><p><code>docs/local-agent-setup.md</code></p></div>
        </div>
      </div>`,
  },
];

const SUMMARIES = [
  L('How a decision gets from a chat into your store.', '決定が、チャットからストアに入るまで。'),
  L('How a fresh session gets its context back.', '新しいセッションが、文脈を取り戻すまで。'),
  L('How saved notes become people, projects and commitments.', '保存したメモが、人・プロジェクト・約束になるまで。'),
  L('How different agents share one store.', '異なるエージェントが、1 つのストアを共有する仕組み。'),
  L('Why an agent can draft, but cannot send.', 'エージェントが下書きはできても、送信はできない理由。'),
  L('Where your data lives, and how it leaves.', 'データがどこにあり、どう持ち出せるか。'),
  L('From a clone to a working recall, on your machine.', 'クローンから recall が動くまで。自分のマシンで。'),
];

const BEATS = [save, recall, state, faces, perms, local, quick];
const SLUGS = ['save', 'recall', 'state', 'faces', 'permissions', 'local-first', 'quickstart'];

export const EXPLAINERS = BEATS.map((beats, i) => ({
  id: SLUGS[i],
  kind: 'explainer',
  n: i + 1,
  file: `${String(i + 1).padStart(2, '0')}-${SLUGS[i]}`,
  title: TITLES[i],
  summary: SUMMARIES[i],
  scenes: [intro(i + 1, TITLES[i], SUMMARIES[i]), ...beats, outro(TITLES[i + 1])],
}));
