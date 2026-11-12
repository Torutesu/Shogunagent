// The launch film: nine scenes, about 80 seconds. Every on-screen example is
// fictional and labelled as such; nothing here shows a send succeeding, a hosted
// account, or a capability the README lists as unverified.
import { L, a, logo, icon, chat, userMsg, agentMsg, toolChip, note, store, stateCard, pill, headline } from './ui.js';

const QUOTE = L(
  'Let’s keep it short and direct. Alex should review before we publish. Friday could work, but we haven’t agreed on a date.',
  '短く、率直にいきましょう。公開前に Alex のレビューを。金曜でもよさそうですが、日付はまだ決めていません。',
);

const pain = {
  id: 'pain',
  dur: 10.6,
  bare: true,
  html: (t) => {
    const cards = [
      { at: 0.3, day: L('Monday', '月曜日'), agent: L('Planning agent', '企画エージェント'), recap: L('The website launch: keep the copy short, Alex reviews first, and the date isn’t set yet.', 'サイト公開の件です。コピーは短く、先に Alex のレビュー、公開日はまだ未定です。') },
      { at: 2.5, day: L('Wednesday', '水曜日'), agent: L('Coding agent', 'コーディングエージェント'), recap: L('OK, background again. Copy stays short. Alex reviews before we publish. No date yet.', 'もう一度、背景から。コピーは短く。公開前に Alex がレビュー。日付は未定です。') },
      { at: 4.5, day: L('Friday', '金曜日'), agent: L('Writing agent', 'ライティングエージェント'), recap: L('Once more: short copy, Alex’s review comes first, launch date still open…', '繰り返しになりますが、コピーは短く、Alex のレビューが先、公開日は未定で…') },
    ];
    const ask = L('New session. What’s the background?', '新しいセッションです。背景を教えてください。');
    return `<div class="pain-row" data-v="dim 7 0.8">${cards.map((c, i) => `
      <div class="pain-col" ${a('rise', c.at, 0.7)}>
        <div class="pain-day"><span class="eyebrow">${t(c.day)}</span><span class="pain-count">${t(L(`Explained ×${i + 1}`, `説明 ${i + 1} 回目`))}</span></div>
        ${chat({ name: t(c.agent), sub: t(L('New conversation', '新しい会話')), rows: [
          agentMsg(`<p>${t(ask)}</p>`, a('rise', c.at + 0.35, 0.5)),
          userMsg(t(c.recap), a('rise', c.at + 0.9, 0.4), `${c.at + 1.0} ${i === 0 ? 46 : 62}`),
        ] })}
      </div>`).join('')}</div>
      <div class="pain-over">
        <h1 ${a('rise', 7.2, 0.8)}>${t(L('You’ve explained this\n*three times.*', '同じ説明、\n*これで 3 回目。*'))}</h1>
        <p ${a('rise', 7.9, 0.8)}>${t(L('Every new chat starts from zero.', '新しいチャットは、毎回ゼロから始まる。'))}</p>
      </div>`;
  },
};

const title = {
  id: 'title',
  dur: 6.2,
  bare: true,
  html: (t) => `<div class="title-card">
      ${logo('title-logo', 0.1)}
      <div class="title-word" ${a('rise', 1.0, 0.7)}>ShogunAgent</div>
      <h1 ${a('rise', 1.6, 0.9)}>${t(L('Your next agent.\n*Already up to speed.*', '次のエージェントは、\n*もう把握している。*'))}</h1>
      <p ${a('rise', 2.5, 0.8)}>${t(L('The state layer for AI agents', 'AI エージェントのための状態レイヤー'))}</p>
    </div>`,
};

const save = {
  id: 'save',
  dur: 11,
  html: (t) => `<div class="split">
      ${headline(t,
        L('Save what matters.\n*Source included.*', '大事なことを保存。\n*出典ごと。*'),
        L('At useful checkpoints, your agent saves decisions and progress, together with the original note.', '区切りのよいところで、エージェントが決定や進捗を保存します。元のメモも一緒に。'))}
      <div class="vis vis-save">
        ${chat({ cls: 'save-chat', attrs: a('rise', 0.9, 0.7), name: t(L('Planning agent', '企画エージェント')), sub: t(L('Monday · working session', '月曜日 · 作業セッション')), rows: [
          userMsg(t(QUOTE), a('rise', 1.3, 0.5), '1.4 62'),
          toolChip('memory.ingest', [t(L('L1 · local write', 'L1 · ローカル書き込み'))], a('rise', 4.0, 0.5)),
        ] })}
        <div class="flyer" data-v="fly 6.0 1.0">
          ${note({ attrs: a('rise', 4.6, 0.6), meta: t(L('Source note · Monday 10:42', '出典メモ · 月曜 10:42')), quote: `“${t(QUOTE)}”` })}
        </div>
        ${store({ attrs: a('rise', 5.2, 0.7), title: t(L('Your store', 'あなたのストア')), file: t(L('Encrypted · on your machine', '暗号化 · あなたのマシン上')), rows: [
          `<li ${a('left', 7.0, 0.5)}>${icon.check}<span>${t(L('1 source saved', '出典 1 件を保存'))}</span></li>`,
          `<li ${a('left', 7.4, 0.5)}>${icon.person}<span>${t(L('Written by', '書き込み元'))} <code>agent:planner</code></span></li>`,
          `<li ${a('left', 7.8, 0.5)}>${icon.loop}<span>${t(L('Sent twice, stored once', '2 回送っても保存は 1 回'))}</span></li>`,
        ] })}
      </div>
    </div>`,
};

const recall = {
  id: 'recall',
  dur: 11.4,
  html: (t) => `<div class="split rev">
      <div class="vis vis-recall">
        ${chat({ cls: 'recall-chat', attrs: a('rise', 0.6, 0.7), name: t(L('Coding agent', 'コーディングエージェント')), sub: t(L('Wednesday · a fresh conversation', '水曜日 · 新しい会話')), rows: [
          userMsg(t(L('Where are we with the website?', 'サイトの件、今どうなってる？')), a('rise', 1.0, 0.4), '1.1 30'),
          toolChip('memory.recall', [t(L('read', '読み取り')), t(L('free', '無料'))], a('rise', 2.6, 0.5)),
          agentMsg(`<p ${a('fade', 3.5, 0.6)}>${t(L('The copy should be short and direct. Alex still needs to review it before you publish.', 'コピーは短く率直に。公開前に Alex のレビューがまだ必要です。'))}</p>
            <p class="uncertain" ${a('rise', 5.0, 0.6)}><i></i>${t(L('The launch date is still undecided.', '公開日はまだ決まっていません。'))}</p>
            <div class="source-jump" ${a('rise', 6.3, 0.6)}>${icon.file}<span>${t(L('From your project note · saved Monday 10:42', 'プロジェクトのメモより · 月曜 10:42 に保存'))}</span></div>`, a('rise', 3.3, 0.5)),
        ] })}
      </div>
      ${headline(t,
        L('A fresh chat.\n*The same context.*', '新しいチャット。\n*同じ文脈。*'),
        L('Before the next task, your agent recalls what’s relevant: the source behind it, and what’s still uncertain.', '次の作業の前に、関連する文脈を呼び出します。根拠となる出典と、まだ決まっていないことも。'), 0.8)}
    </div>`,
};

export const STATE_CARDS = (t) => [
  { kind: 'person', glyph: icon.person, label: t(L('Person', '人')), title: 'Alex', quote: t(L('Reviews the copy before it goes out', '公開前にコピーをレビューする')), band: 'high', bandLabel: t(L('High', '高')), sources: t(L('2 sources', '出典 2 件')) },
  { kind: 'project', glyph: icon.folder, label: t(L('Project', 'プロジェクト')), title: t(L('Website launch', 'サイト公開')), quote: t(L('Short, direct homepage copy', '短く率直なトップページのコピー')), band: 'high', bandLabel: t(L('High', '高')), sources: t(L('3 sources', '出典 3 件')) },
  { kind: 'commitment', glyph: icon.flag, label: t(L('Commitment', '約束')), title: t(L('Alex reviews the copy before publishing', '公開前に Alex がコピーをレビュー')), quote: t(L('“Alex should review before we publish.”', '「公開前に Alex のレビューを。」')), band: 'high', bandLabel: t(L('High', '高')), sources: t(L('Quote · Mon 10:42', '引用 · 月曜 10:42')) },
  { kind: 'loop', glyph: icon.loop, label: t(L('Open loop', '未完了')), title: t(L('Launch date not confirmed', '公開日が未確定')), quote: t(L('possibly: Friday', 'possibly: 金曜日')), band: 'medium', bandLabel: t(L('Medium', '中')), sources: t(L('Quote · Mon 10:42', '引用 · 月曜 10:42')) },
];

export const SOURCE_NOTES = (t) => [
  { meta: t(L('Mon 10:42 · planning agent', '月曜 10:42 · 企画エージェント')), quote: t(L('“Keep it short and direct. Alex should review before we publish.”', '「短く、率直に。公開前に Alex のレビューを。」')) },
  { meta: t(L('Mon 10:43 · planning agent', '月曜 10:43 · 企画エージェント')), quote: t(L('“Friday could work, but we haven’t agreed on a date.”', '「金曜でもよさそうですが、日付はまだ決めていません。」')) },
  { meta: t(L('Wed 15:10 · coding agent', '水曜 15:10 · コーディングエージェント')), quote: t(L('“Homepage copy is in. Waiting on Alex.”', '「トップページのコピーを反映。Alex 待ちです。」')) },
];

const state = {
  id: 'state',
  dur: 11.4,
  html: (t) => `<div class="stack">
      <div class="copy center"><h1 ${a('rise', 0.5, 0.8)}>${t(L('Not a pile of memories. *A model of your work.*', 'メモの山ではなく、*仕事のモデル。*'))}</h1></div>
      <div class="state-flow">
        <div class="state-notes">${SOURCE_NOTES(t).map((n, i) => note({ cls: 'compact', attrs: a('left', 1.3 + i * 0.3, 0.6), meta: n.meta, quote: n.quote })).join('')}</div>
        <div class="state-mid"><svg viewBox="0 0 200 420" class="wires"><path d="M0 70 C110 70 90 210 200 210" ${a('draw', 2.5, 0.8)}/><path d="M0 210 L200 210" ${a('draw', 2.6, 0.8)}/><path d="M0 350 C110 350 90 210 200 210" ${a('draw', 2.7, 0.8)}/></svg><span class="review-node" ${a('pop', 3.0, 0.6)}>${t(L('Review', 'レビュー'))}</span></div>
        <div class="state-grid">${STATE_CARDS(t).map((c, i) => stateCard({ ...c, attrs: a('rise', 3.7 + i * 0.6, 0.7) })).join('')}</div>
      </div>
      <p class="footnote" ${a('fade', 7.2, 0.8)}>${t(L('Typed state comes from a review step you configure. Every fact keeps the quote that proves it.', '型付きの状態は、あなたが設定したレビュー処理から生まれます。すべての事実に、それを裏づける引用が付きます。'))}</p>
    </div>`,
};

export const hub = (t, start = 0.8) => {
  const nodes = [
    { name: 'Claude Code', x: 150, y: 120, d: 'M150 120 C300 120 330 330 450 330' },
    { name: 'Codex', x: 750, y: 120, d: 'M750 120 C600 120 570 330 450 330' },
    { name: 'Cursor', x: 150, y: 540, d: 'M150 540 C300 540 330 330 450 330' },
    { name: 'Claude Desktop', x: 750, y: 540, d: 'M750 540 C600 540 570 330 450 330' },
  ];
  return `<div class="hub">
      <svg viewBox="0 0 900 660" class="hub-wires">
        ${nodes.map((n, i) => `<path id="hw${i}" d="${n.d}" ${a('draw', start + 0.6 + i * 0.15, 0.8)}/>`).join('')}
        ${nodes.map((n, i) => `<circle r="7" class="hub-dot" data-along="#hw${i} 2.4 ${i * 0.25} ${start + 1.6}"/>`).join('')}
      </svg>
      ${nodes.map((n, i) => `<div class="hub-node" style="left:${n.x}px;top:${n.y}px" ${a('pop', start + i * 0.15, 0.6)}><span class="chat-glyph">✳</span>${n.name}</div>`).join('')}
      <div class="hub-core" ${a('pop', start + 0.5, 0.7)}>${store({ title: t(L('One local store', '1 つのローカルストア')), file: t(L('Encrypted · yours', '暗号化 · あなたのもの')) })}
        <div class="faces">${['MCP', 'REST', 'CLI'].map((f, i) => pill(f, 'face', a('rise', start + 1.4 + i * 0.15, 0.5))).join('')}</div>
      </div>
    </div>`;
};

const agents = {
  id: 'agents',
  dur: 8.6,
  html: (t) => `<div class="split">
      ${headline(t,
        L('Switch agents.\n*Keep the thread.*', '道具を替えても、\n*話は続く。*'),
        L('Connect each client to the same local store. One behavior over MCP, REST and CLI.', 'どのクライアントも、同じローカルのストアにつなぐ。MCP・REST・CLI のどれでも同じ動作です。'))}
      <div class="vis vis-hub">${hub(t)}</div>
    </div>`,
};

export const ladder = (t, start = 0.9, hl = 3.0) => {
  const rows = [
    { lv: 'L1', glyph: icon.eye, title: t(L('Read, and save locally', '読み取りと、ローカルへの保存')), sub: t(L('Runs right away', 'すぐに実行')) },
    { lv: 'L2', glyph: icon.hand, title: t(L('Change something of yours', 'あなたのものを変更する')), sub: t(L('Needs your confirmation', 'あなたの確認が必要')) },
    { lv: 'L3', glyph: icon.send, title: t(L('Send, post, pay', '送信・投稿・支払い')), sub: t(L('Waits for a human, every time', '毎回、人の承認を待つ')) },
  ];
  return `<div class="ladder">${rows.map((r, i) => `<div class="rung rung-${r.lv}" ${a('left', start + i * 0.4, 0.6)} ${i === 2 ? `data-v="hl ${hl} 0.6"` : ''}><b>${r.lv}</b><span class="rung-glyph">${r.glyph}</span><span class="rung-text">${r.title}<small>${r.sub}</small></span></div>`).join('')}</div>`;
};

export const approvalCard = (t, start) => `<div class="approval" ${a('rise', start, 0.7)}>
    <div class="approval-top"><span class="eyebrow">${t(L('Approval request', '承認リクエスト'))}</span><span class="pending" data-loop="pulse 1.8"><i></i>${t(L('Waiting for you', 'あなたの承認待ち'))}</span></div>
    <h3>${t(L('Ready to send the update?', 'この更新を送信しますか？'))}</h3>
    <p class="recipient">${t(L('To: Project team', '宛先：プロジェクトチーム'))}</p>
    <blockquote>${t(L('“The homepage copy is ready for Alex’s review.”', '「トップページのコピーは、Alex のレビュー待ちです。」'))}</blockquote>
    <div class="approval-actions"><span class="btn solid">${t(L('Approve', '承認する'))}</span><span class="btn">${t(L('Reject', '却下する'))}</span></div>
    <div class="approval-foot"><i></i>${t(L('Draft prepared. Nothing sent.', '下書きを用意しました。まだ何も送信していません。'))}</div>
  </div>`;

const approve = {
  id: 'approve',
  dur: 9.4,
  html: (t) => `<div class="split rev">
      <div class="vis vis-approve">${approvalCard(t, 3.0)}</div>
      <div class="copy"><h1 ${a('rise', 0.6, 0.8)}>${t(L('Agents prepare.\n*You decide.*', '準備するのは AI。\n*決めるのはあなた。*'))}</h1>${ladder(t, 1.2, 3.2)}</div>
    </div>`,
};

const local = {
  id: 'local',
  dur: 7.6,
  html: (t) => {
    const tiles = [
      { glyph: icon.lock, title: L('One encrypted file per person', '1 人につき、暗号化ファイル 1 つ'), sub: L('Your store stays on infrastructure you run.', 'ストアは、あなたが動かす環境に置かれます。') },
      { glyph: icon.code, title: L('Open engine', 'オープンなエンジン'), sub: L('Apache-2.0. Read the source, build it yourself.', 'Apache-2.0。ソースを読み、自分でビルドできます。') },
      { glyph: icon.box, title: L('Portable by design', '持ち運べる設計'), sub: L('Export your state and its evidence as JSON Lines.', '状態とその根拠を JSON Lines で書き出せます。') },
    ];
    return `<div class="stack">
      <div class="copy center"><h1 ${a('rise', 0.5, 0.8)}>${t(L('Your context. *Your infrastructure.*', 'あなたの文脈は、*あなたの環境に。*'))}</h1></div>
      <div class="tiles">${tiles.map((x, i) => `<div class="tile" ${a('rise', 1.3 + i * 0.35, 0.7)}><span class="tile-glyph">${x.glyph}</span><h3>${t(x.title)}</h3><p>${t(x.sub)}</p></div>`).join('')}</div>
    </div>`;
  },
};

export const cta = {
  id: 'cta',
  dur: 8.2,
  bare: true,
  html: (t) => `<div class="title-card cta-card">
      ${logo('title-logo small', 0.3)}
      <h1 ${a('rise', 1.0, 0.9)}>${t(L('You’ve already done the explaining.\n*Keep going from there.*', '説明は、もう済んでいる。\n*続きから始めよう。*'))}</h1>
      <div class="cta-links" ${a('rise', 2.2, 0.8)}><span class="btn solid big">github.com/Torutesu/Shogunagent</span><span class="cta-site">shogunagent.com</span></div>
      <p class="cta-note" ${a('fade', 3.0, 0.8)}>${t(L('Developer preview · Build from source · Linux and macOS · Apache-2.0', '開発者プレビュー · ソースからビルド · Linux / macOS · Apache-2.0'))}</p>
    </div>`,
};

// Subtitle track for the launch film. The film itself is typographic, so these
// lines are not burned in; they ship as .srt / .vtt and double as a narration script.
const SUBTITLES = {
  pain: [[0.4, L('Monday. Wednesday. Friday. Every new chat asks for the background again.', '月曜、水曜、金曜。新しいチャットのたびに、また背景を聞かれます。')], [7.0, L('You’ve explained this three times. Every new chat starts from zero.', '同じ説明、これで 3 回目。新しいチャットは、毎回ゼロから始まります。')]],
  title: [[1.0, L('ShogunAgent. Your next agent, already up to speed.', 'ShogunAgent。次のエージェントは、もう把握している。')]],
  save: [[0.5, L('At a useful checkpoint, your agent saves what matters, together with the original note.', '区切りのよいところで、エージェントが大事なことを保存します。元のメモも一緒に。')], [6.0, L('It lands in your own encrypted store, on your machine.', 'あなたのマシン上の、あなた専用の暗号化ストアに入ります。')]],
  recall: [[0.6, L('In a fresh chat, the next agent recalls it.', '新しいチャットで、次のエージェントがそれを呼び出します。')], [5.0, L('With the source behind the answer, and what is still undecided.', '答えの根拠となる出典と、まだ決まっていないことも一緒に。')]],
  state: [[0.5, L('Saved notes become a model of your work: people, projects, commitments and open loops.', '保存したメモは、仕事のモデルになります。人、プロジェクト、約束、未完了の仕事。')], [7.0, L('Typed state comes from a review step you configure. Every fact keeps the quote that proves it.', '型付きの状態は、あなたが設定したレビュー処理から生まれます。すべての事実に、裏づけとなる引用が付きます。')]],
  agents: [[0.5, L('Switch agents and keep the thread. Each client connects to the same local store, over MCP, REST or the CLI.', '道具を替えても、話は続きます。どのクライアントも、MCP・REST・CLI で同じローカルのストアにつながります。')]],
  approve: [[0.5, L('Agents prepare. You decide.', '準備するのは AI。決めるのはあなた。')], [3.2, L('Sending, posting and paying always wait for a human.', '送信・投稿・支払いは、必ず人の承認を待ちます。')]],
  local: [[0.5, L('One encrypted file per person, an open engine, and a portable export.', '1 人につき暗号化ファイル 1 つ。オープンなエンジン。持ち運べるエクスポート。')]],
  cta: [[1.0, L('You’ve already done the explaining. Keep going from there.', '説明は、もう済んでいる。続きから始めよう。')], [3.0, L('Developer preview. Build it from source at github.com/Torutesu/Shogunagent.', '開発者プレビュー。github.com/Torutesu/Shogunagent のソースからビルドできます。')]],
};
for (const scene of [pain, title, save, recall, state, agents, approve, local, cta]) scene.captions = SUBTITLES[scene.id];

export const LAUNCH = {
  id: 'launch',
  kind: 'launch',
  file: '00-launch',
  title: L('ShogunAgent launch film', 'ShogunAgent ローンチ動画'),
  scenes: [pain, title, save, recall, state, agents, approve, local, cta],
};
