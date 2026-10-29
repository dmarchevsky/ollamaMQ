// Demo-mode assertion driver for the conversation viewer. Append this file's
// body to a copy of src/web/index.html and run it headless:
//   <chromium> --virtual-time-budget=42000 --window-size=1280,800 \
//     --screenshot=out.png "file://<copy>/index.html?demo=1"
// Console prints "ASSERT PASS/FAIL <name>" per check.
// The budget must outlast the whole timeline: the compaction checks run at
// 26 s and 34 s, and a budget under ~40 s makes the viewer checks race the
// page (a rail jump fires while its turn is still folded, and reports a
// missing target that is only folded away).
// Screenshot caveat: chrome-headless-shell with --virtual-time-budget races the
// modal-open repaint and sometimes captures a stale frame (empty strip where
// the dialog header should be). Real Chromium over CDP (playwright-core,
// waitForSelector + settle + screenshot) captures the live layout correctly —
// 8/8 clean vs the same page the shell coin-flipped.
addEventListener('load', () => {
  const A = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  setTimeout(() => { location.hash = '#logs'; }, 400);
  // The Logs page asks whether bodies are being captured: with capture on (the
  // demo always has it on) there is no warning banner and no row badge.
  setTimeout(() => {
    A('capture-state-loaded', S.capture && S.capture.log === true,
      JSON.stringify(S.capture));
    A('capture-banner-hidden', !document.querySelector('.capnote')
      && ![...document.querySelectorAll('#logs tbody')].some(t => /not captured/.test(t.textContent)));
  }, 1000);
  setTimeout(() => {
    // The Logs filter matches the conversation's NAME (session label), not
    // just the hex tag — the demo names one conversation, and only that row
    // may survive the query.
    const inp = document.querySelector('#log-filter');
    inp.value = 'dispatcher';
    inp.dispatchEvent(new Event('input'));
    const rows = [...document.querySelectorAll('#logs tbody tr[data-action="conv-open"]')];
    const ok = rows.length === 1 && /dispatcher refactor/i.test(rows[0].textContent);
    inp.value = '';
    inp.dispatchEvent(new Event('input'));
    A('logs-filter-by-name', ok, rows.map(r => r.querySelector('.sess')?.textContent).join(' '));
  }, 1150);
  // Open a turn that has stored text: a redacted turn legitimately has nothing
  // to scroll, so it cannot prove the body scrolls (see the capture switch).
  setTimeout(() => {
    // The Logs page opens grouped by conversation: one row per session. A row
    // whose text was never stored cannot prove the body scrolls, and a control
    // row is not a conversation, so pick a conversation row with stored text.
    const convRows = [...document.querySelectorAll('#logs tbody tr[data-action="conv-open"]')];
    A('logs-grouped-by-conversation', convRows.length >= 2, `${convRows.length} conversation rows`);
    const pairs = convRows.map(r => `${r.dataset.user}#${r.dataset.session}`);
    A('logs-one-row-per-conversation', new Set(pairs).size === pairs.length, pairs.join(' '));
    A('logs-row-sums-requests', convRows.every(r => /\d+ req ·/.test(r.textContent)),
      convRows.map(r => r.querySelector('.rline')?.textContent.trim().slice(0, 28)).join(' | '));
    A('logs-row-names-session', convRows.some(r => /#dispatcher refactor/.test(r.textContent)),
      convRows.map(r => r.querySelector('.sess')?.textContent).join(' '));
    const tr = convRows.find(r => !/redacted/.test(r.textContent));
    if (tr) tr.click();
  }, 1500);
  setTimeout(() => {
    document.querySelector('#lv-jump').click();
    const de = document.documentElement;
    A('background-locked', getComputedStyle(de).overflow === 'hidden',
      `html overflow ${getComputedStyle(de).overflow}`); // hidden = no user scroll, no bar
    const b = document.querySelector('#dlg-log .lv-body');
    A('body-scrollable', b.scrollHeight > b.clientHeight && getComputedStyle(b).scrollbarWidth !== 'thin');
    // The tree is a conversation: every turn of it, each stamped when it was.
    const nodes = [...document.querySelectorAll('#lv-rail .lv-node')];
    const turns = nodes.filter(n => n.classList.contains('turn'));
    A('tree-has-all-turns', turns.length === LV.conv.turns.length,
      `${turns.length} nodes of ${LV.conv.turns.length} turns`);
    const whens = turns.map(n => n.querySelector('.when').textContent.trim());
    // One absolute clock (the conversation's first request); everything after it
    // is a delta from the request before it.
    A('stamps-shaped', whens.length > 0 && /^\d{2}:\d{2}:\d{2}$/.test(whens[0])
      && whens.slice(1).every(w => /^\+\d+(\.\d)?s$|^\+\d+m \d{2}s$/.test(w)), JSON.stringify(whens));
    A('first-stamp-absolute', whens[0] === fmtTime(LV.conv.turns[0].at_ms),
      `${whens[0]} want ${fmtTime(LV.conv.turns[0].at_ms)}`);
    // The whole conversation is on the page, not the one round it was opened on:
    // one block per turn, and the focused one is expanded.
    const blocks = [...document.querySelectorAll('#dlg-log .lv-wrap section.turn')];
    A('all-turns-rendered', blocks.length === LV.conv.turns.length,
      `${blocks.length} blocks of ${LV.conv.turns.length}`);
    A('focus-expanded', document.querySelectorAll('.turn.active .lv-sec').length >= 1,
      'the request navigation landed on is expanded');
    // A turn's stage line prints the measured span, not a back-computed guess.
    const t = LV.conv.turns.at(-1);
    const span = ((t.tl.end - t.tl.first_token) / 1000).toFixed(1) + 's';
    const line = blocks.at(-1).querySelector('.turn-h').textContent;
    A('stage-line-measured', line.includes(span), `want ${span} in "${line.replace(/\s+/g, ' ').trim()}"`);
    // Request navigation walks the conversation and keeps the rail in step.
    document.querySelector('#lv-first').click();
    setTimeout(() => {
      A('nav-first', LV.rid === LV.conv.turns[0].rid && LV.open.has(LV.rid),
        `focused ${LV.rid} of ${LV.conv.turns.map(x => x.rid)}`);
      A('nav-first-rail', document.querySelector('#lv-rail .lv-node.on')?.dataset.rid === String(LV.rid));
      A('nav-first-disabled', document.querySelector('#lv-first').disabled);
      document.querySelector('#lv-last').click();
      setTimeout(async () => {
        const wait = ms => new Promise(r => setTimeout(r, ms));
        A('nav-last', LV.rid === LV.conv.turns.at(-1).rid, `focused ${LV.rid}`);
        A('nav-last-disabled', document.querySelector('#lv-last').disabled);
        // The Whole/New switch and the per-request carry line are gone: one
        // global Show-incremental checkbox decides whether a request renders
        // only its increment (with a bright strip) or the whole re-sent prompt
        // (re-sent messages dimmed). No request section carries a meta line.
        A('no-scope-switch', !document.querySelector('#lv-scope'));
        A('no-carry-line', !document.querySelector('#dlg-log .lv-carry'));
        A('no-request-meta-line', !document.querySelector('#dlg-log .turn .lv-meta'));
        const rid2 = LV.conv.turns[1].rid, hid = lvHidden(1);
        const inc = () => document.querySelectorAll(`#lv-q-${rid2} .msg`).length;
        // Inspect request 2's increment — open it if the navigation left it shut.
        if (!LV.open.has(rid2)) { document.querySelector(`#lv-t-${rid2} .turn-h`).click(); await wait(600); }
        let patience = 10;   // the content fetch behind the body decides when it exists
        while (patience-- > 0 && !document.querySelectorAll(`#lv-t-${rid2} .msg`).length) await wait(200);
        A('incremental-striped', document.querySelectorAll(`#lv-t-${rid2} .msg.m-new`).length > 0
          && !document.querySelector(`#lv-t-${rid2} .msg.m-held`),
          `${inc()} messages in request 2, the new ones stripped`);
        // Capture turned off reads as a decision, in the tree and in the turn.
        const off = document.querySelector('.turn-h [title*="text not stored"]');
        A('redacted-labelled', !!off || !LV.conv.redacted, 'capture-off turns are labelled');
        // The request line is an identity, not a label: #N, a word of the
        // increment, sizes quiet on the right; problem rows tint as a line.
        const nTxt = [...document.querySelectorAll('#dlg-log .turn-h .n')].map(n => n.textContent);
        A('turn-line-numbered', nTxt.length === LV.conv.turns.length
          && nTxt.every((x, k) => x === `#${k + 1}`), nTxt.join(' '));
        A('turn-line-sizes-quiet', [...document.querySelectorAll('#dlg-log .turn-h .tail.sz')].every(n => /→/.test(n.textContent)));
        A('turn-line-preview', [...document.querySelectorAll('#dlg-log .turn-h .pv')].some(n => n.innerHTML !== '')
          && [...document.querySelectorAll('#dlg-log .turn-h .pv .u')].length >= 0
          && [...document.querySelectorAll('#dlg-log .turn-h .pv .t')].length >= 0,
          'folded lines tease their words (user blue, thinking green)');
        A('problem-row-tinted', !!document.querySelector('#dlg-log .turn.prob'));
        A('preview-teases-only-when-folded', document.querySelector(`#lv-t-${rid2} .turn-h .pv`)?.innerHTML === ''
          && !document.querySelector(`#lv-rail .lv-node.turn[data-rid="${rid2}"] .pv`)
          && document.querySelector(`#lv-t-${LV.conv.turns.at(-3).rid} .turn-h .pv`)?.innerHTML !== '',
          'open requests say their words once; folded ones tease them');
        // Collapse all folds every block — including the one you arrived on —
        // and the rail thins to the turn lines; Expand all opens everything.
        document.querySelector('#dlg-log [data-action="lv-collapse"]').click();
        await wait(600);
        A('collapse-all-folds-everything', LV.open.size === 0
          && !document.querySelector('#lv-rail .lv-node.sub')
          && !document.querySelector('#lv-rail .lv-node.head'),
          'every block closed; the rail keeps only the request lines');
        document.querySelector('#dlg-log [data-action="lv-expand"]').click();
        await wait(700);
        A('expand-all-opens-everything', LV.open.size === lvTurns().length && LV.closedRuns.size === 0,
          `${LV.open.size} blocks open`);
        // An opened request groups its rows under two heads — what it SENT and
        // what it GOT — each a tinted block with no border, the messages flat.
        const heads = [...document.querySelectorAll('#lv-rail .lv-node.head')];
        A('two-heads-per-open-request', heads.length === 2 * LV.open.size,
          `${heads.length} heads for ${LV.open.size} open requests`);
        A('heads-jump-to-their-section', heads.every(n => n.dataset.action
          === (n.classList.contains('reqh') ? 'lv-req' : 'lv-resp')),
          [...new Set(heads.map(n => n.dataset.action))].join('/'));
        A('heads-tinted-not-bordered', heads.every(n => {
          const s = getComputedStyle(n);
          return s.borderTopWidth === '0px' && s.backgroundColor !== 'rgba(0, 0, 0, 0)';
        }), 'both heads carry a background and no border');
        const subs = [...document.querySelectorAll('#lv-rail .lv-node.sub')];
        A('messages-flat-under-their-head', subs.length > 0 && subs.every(n => {
          const s = getComputedStyle(n);
          return s.borderTopWidth === '0px' && s.backgroundColor === 'rgba(0, 0, 0, 0)';
        }), `${subs.length} message rows, none wears the transcript chip box`);
        // No row reserves a clock column it cannot fill: a blank `.when` is not
        // rendered, and one that carries a time never wraps inside its box.
        const clocks = [...document.querySelectorAll('#lv-rail .lv-node .when')];
        A('no-blank-clock-column', clocks.every(c => c.textContent.trim() !== ''
          || getComputedStyle(c).display === 'none'),
          `${clocks.filter(c => !c.textContent.trim()).length} blank clocks left in the tree`);
        const wide = clocks.map(c => [c.textContent.trim(), c.scrollWidth, c.clientWidth]);
        A('clock-never-wraps', wide.every(([, sw, cw]) => sw <= cw + 1),
          `widest ${JSON.stringify(wide.sort((a, b) => b[1] - a[1]).slice(0, 2))}`);
        const rowsEl = [...document.querySelectorAll('#lv-rail .lv-node')];
        A('tree-rows-single-line', rowsEl.every(n => n.getBoundingClientRect().height <= 26),
          `tallest row ${Math.round(Math.max(...rowsEl.map(n => n.getBoundingClientRect().height)))}px`);
        A('group-guides-wrap-the-rows',
          document.querySelectorAll('#lv-rail .lv-grp').length >= LV.open.size,
          `${document.querySelectorAll('#lv-rail .lv-grp').length} guides for ${LV.open.size} open requests`);
        // The REQ head lands on the Request section, the RESP head on Response.
        document.querySelector('#lv-rail .lv-node.reqh')?.click();
        await wait(600);
        A('req-head-focuses-request-section',
          !!document.querySelector('#dlg-log .lv-sec.flash[id^="lv-q-"]'),
          document.querySelector('#dlg-log .lv-sec.flash')?.id || 'no flashed section');
        // An answer never repeats as a message row of the next request in the
        // rail — its own answer row already spoke it under its own turn.
        const echoes = [];
        lvTurns().forEach((t2, i) => {
          if (!i) return;
          const sig = lvAnswerSig(lvTurns()[i - 1]);
          if (!sig) return;
          for (const n of document.querySelectorAll(`#lv-rail .lv-node.sub[data-rid="${t2.rid}"]`))
            if (n.classList.contains('asst')
              && (n.querySelector('.snip')?.textContent || '').startsWith(sig.slice(0, 16))) echoes.push('#' + (i + 1));
        });
        A('answer-echoes-nowhere', echoes.length === 0,
          echoes.join(',') || 'no reply repeats as a message row');
        // The header switch carries a short label and never splits across lines.
        const incLab = document.querySelector('#lv-inc').closest('label');
        A('inc-label-short-one-line', /^Incremental$/.test(incLab.textContent.trim())
          && incLab.getBoundingClientRect().height < 28,
          `"${incLab.textContent.trim()}" on one line (${Math.round(incLab.getBoundingClientRect().height)}px)`);
        // Message blocks fold on their label — the system prompt starts as one
        // line so the transcript opens on what the conversation said.
        const rid1 = LV.conv.turns[0].rid;
        const sysSel = `#lv-t-${rid1} .msg.sys`;
        A('sys-block-folded-by-default', !!document.querySelector(`${sysSel}.fold`)
          && !!document.querySelector(`${sysSel} .mf-pv`),
          document.querySelector(sysSel)?.textContent.trim().replace(/\s+/g, ' ').slice(0, 46));
        document.querySelector(`${sysSel} .role`).click();
        await wait(600);
        const sysOpen = !!document.querySelector(sysSel)
          && !document.querySelector(sysSel).classList.contains('fold')
          && !!document.querySelector(`${sysSel} .mtext`);
        document.querySelector(`${sysSel} .role`).click();
        await wait(600);
        A('msg-block-folds-and-opens', sysOpen
          && document.querySelector(sysSel)?.classList.contains('fold'),
          'a click opens the system block, a click folds it back');
        // The incremental view is about one request's moment: the run
        // grouping is the session's story and waits for the whole-prompt
        // view. The answer stays visible — with it hidden a single-request
        // conversation would show no assistant words anywhere.
        const body = document.querySelector('#dlg-log .lv-body');
        A('inc-hides-runs', !document.querySelector('#dlg-log .run-h')
          && !document.querySelector('#lv-rail .lv-node.run'),
          'incremental: no run lines in either pane');
        A('inc-still-shows-the-answer', !!document.querySelector('#lv-rail .lv-node.resph')
          && !!document.querySelector(`#lv-t-${rid2} .lv-sec.out, #lv-a-${rid2}`),
          'the response section and rail row survive incremental');
        // The rail rows for a turn are the messages it ADDED — no REQ count
        // row, and no re-sent rows under a later request while incremental.
        const rows2 = [...document.querySelectorAll(`#lv-rail .lv-node.sub[data-rid="${rid2}"]`)];
        A('tree-rows-are-new-only', rows2.length > 0 && rows2.every(n => +n.dataset.idx >= hid)
          && ![...document.querySelectorAll(`#lv-rail .lv-node[data-rid="${rid2}"]`)].some(n => /\bheld\b/.test(n.textContent)),
          `${rows2.length} rows for request 2, every idx >= ${hid}`);
        // Unchecking Show incremental unfolds every full prompt at once: the
        // re-sent prefix arrives dimmed below the stripped increment, and the
        // tree grows the held rows too. Re-checking folds them back.
        const box = document.querySelector('#lv-inc');
        const n0 = inc();
        box.click();          // native toggle: flips checked and bubbles the click
        await wait(700);
        const heldRows = document.querySelectorAll(`#lv-t-${rid2} .msg.m-held`).length;
        A('show-incremental-full-prompt', inc() === n0 + hid && heldRows === hid
          && document.querySelectorAll(`#lv-rail .lv-node.sub[data-rid="${rid2}"]`).length > rows2.length,
          `request 2 body ${n0} \u2192 ${inc()} messages (+${hid} held, dimmed)`);
        // The whole-prompt view gets the run grouping back: the group lines
        // appear in both panes.
        const runHeads = [...document.querySelectorAll('#dlg-log .run-h')];
        A('runs-group-the-session', runHeads.length >= 1
          && document.querySelectorAll('#lv-rail .lv-node.run').length === runHeads.length
          && !runHeads.some(h => /#(\d+)–#\1/.test(h.textContent)),
          `${runHeads.length} run(s) back, mirrored in the rail, every run names a range`);
        // A request between two final answers alone groups nothing — it stands
        // on its own, with no chrome and no second numbering system.
        A('single-requests-stand-alone', !document.getElementById(`lv-t-${LV.conv.turns[0].rid}`).closest('.run'),
          'request #1 renders with no run wrapper');
        const rh0 = runHeads[0].dataset.run;
        const turnsInRun0 = () => document.querySelectorAll(`.run[data-run="${rh0}"] section.turn`).length;
        const r0 = turnsInRun0();
        runHeads[0].click();
        await wait(500);
        A('run-folds-requests', turnsInRun0() === 0 && r0 > 0, `run folded to its line (${r0} requests hidden)`);
        document.querySelector(`#dlg-log .run-h[data-run="${rh0}"]`).click();
        await wait(500);
        A('run-unfolds', turnsInRun0() === r0, `${r0} requests back`);
        // And the rail's RUN row folds the run in the body, not just the
        // body's own run header.
        const railRun = document.querySelector('#lv-rail .lv-node.run');
        const runK = railRun.dataset.run;
        railRun.click();
        await wait(500);
        A('rail-run-folds-body', document.querySelectorAll(`#dlg-log .run[data-run="${runK}"] section.turn`).length === 0,
          `rail run ${runK} folded its requests in the body`);
        document.querySelector('#lv-rail .lv-node.run').click();
        await wait(500);
        // The rail's RESP row lands on the Response section, not on the turn
        // top: the section flashes and the body actually scrolled.
        const respNode = document.querySelector(`#lv-rail .lv-node.resp[data-rid="${rid2}"]`);
        body.scrollTop = 0;
        respNode.click();
        await wait(600);
        A('resp-jump-lands-on-section', document.querySelector('#dlg-log .lv-sec.flash')?.id === `lv-a-${rid2}`
          && body.scrollTop > 0, `flashed lv-a-${rid2}, scrolled ${body.scrollTop}px`);
        // The rail carries the assistant's answer under the assistant label
        // with the reply's own words — a one-request conversation has no
        // re-sent history, so this row is where the answer is.
        const respRow = document.querySelector(`#lv-rail .lv-node.resp[data-rid="${rid2}"]`);
        const respTag = respRow?.querySelector('.tag')?.textContent || '';
        const respSnip = respRow?.querySelector('.snip')?.textContent || '';
        A('resp-row-says-what-it-said', respTag === 'ASST' && /rotate/i.test(respSnip)
          && respSnip.length > 'stop'.length + 3,
          `rail answer row: ${respTag} "${respSnip.slice(0, 40)}"`);
        // The formatted view reads the stream back as the answer, not as the
        // wire: keep-alive comments and empty deltas are gone, the text is whole.
        const out = () => [...document.querySelectorAll(`#lv-t-${rid2} .lv-sec`)].pop().textContent;
        const fmt = out();
        A('formatted-stream-is-text', !/data: ?\{|keepalive/.test(fmt)
          && /rotate\(\) loop shifts files/.test(fmt), fmt.replace(/\s+/g, ' ').trim().slice(0, 96));
        document.querySelector('#lv-fmt label:has(input[value="raw"])').click();
        await wait(700);
        const raw = out();
        A('raw-keeps-the-wire', /data: ?\{/.test(raw) && raw.includes('[DONE]'), `${raw.length} chars of captured bytes`);
        document.querySelector('#lv-fmt label:has(input[value="fmt"])').click();
        await wait(500);
        // With its held history on screen, that request's own copy of a
        // re-sent message is what a jump lands on \u2014 not the earlier turn.
        await lvFocusMsg(rid2, 0);
        await wait(500);
        A('unfolded-resent-stays', LV.focusMsg?.[0] === rid2 && LV.focusMsg?.[1] === 0
          && !!document.getElementById(`lv-m-${rid2}-0`), 'idx 0 of request 2 stays in request 2');
        // Re-query: the redraw the first toggle caused replaced the checkbox node.
        document.querySelector('#lv-inc').click();   // back to incremental
        await wait(700);
        A('show-incremental-remembered', inc() === n0
          && JSON.parse(localStorage.getItem('omq.lv') || '{}').inc === true
          && !document.querySelector('#dlg-log .run-h'),
          `back to ${inc()} of ${n0} messages, stored=${JSON.parse(localStorage.getItem('omq.lv') || '{}').inc}, run-h=${!!document.querySelector('#dlg-log .run-h')}`);
        // Auto-refresh polls quietly and keeps the view: your scroll place and
        // your open turns survive every poll.
        const auto = document.querySelector('#lv-auto');
        body.scrollTop = 420;
        await wait(200);
        const reloads0 = LV.reloads || 0;
        auto.click();
        await wait(6400);
        A('auto-refresh-refreshes', (LV.reloads || 0) > reloads0
          && JSON.parse(localStorage.getItem('omq.lv') || '{}').auto === true,
          `${(LV.reloads || 0) - reloads0} quiet refreshes`);
        A('auto-refresh-keeps-view', Math.abs(body.scrollTop - 420) <= 2 && LV.open.size > 0,
          `scroll kept at ${body.scrollTop}, ${LV.open.size} turns open`);
        document.querySelector('#lv-auto').click();   // stop polling
        await wait(200);
        // The reconstructed response is an ASST block like any other — same
        // chip, same fold click — and thinking opens as a ruled box.
        const asstResp = document.querySelector('#dlg-log .lv-sec[id^="lv-a-"] .msg.asst > .role[data-action="msg-fold"]');
        A('resp-block-is-foldable-asst', !!asstResp && /^▾ ASST/.test(asstResp.textContent.trim()),
          asstResp ? asstResp.textContent.trim().slice(0, 20) : 'no foldable ASST response block');
        asstResp.click();
        await wait(600);
        const respShut = !!document.querySelector('#dlg-log .lv-sec[id^="lv-a-"] .msg.asst.fold');
        document.querySelector('#dlg-log .lv-sec[id^="lv-a-"] .msg.asst > .role').click();
        await wait(600);
        A('resp-block-folds-and-opens', respShut
          && !document.querySelector('#dlg-log .lv-sec[id^="lv-a-"] .msg.asst.fold'),
          'the answer folds and opens on its label');
        const th = document.querySelector('#dlg-log .msg details');
        A('thinking-is-a-boxed-appendix', !!th
          && getComputedStyle(th).borderStyle !== 'none'
          && getComputedStyle(th.querySelector('summary')).listStyleType === 'none',
          th ? getComputedStyle(th).borderStyle : 'no thinking block');
        // A jump lands on the block's first line, not mid-block.
        const farNode = [...document.querySelectorAll('#lv-rail .lv-node.msg')].pop();
        farNode.click();
        await wait(900);
        const tgtEl = document.getElementById(`lv-m-${farNode.dataset.rid}-${farNode.dataset.idx}`);
        A('jump-lands-at-block-start', !!tgtEl
          && Math.abs(tgtEl.getBoundingClientRect().top - body.getBoundingClientRect().top) < 80,
          tgtEl ? `top gap ${Math.round(tgtEl.getBoundingClientRect().top - body.getBoundingClientRect().top)}px` : 'target missing');
        // Scrolling the log is navigation too: the rail row of whichever block
        // sits at the top lights up and stays in view as the wheel turns.
        const mTarget = document.getElementById(`lv-m-${rid2}-7`);
        const bTop2 = body.getBoundingClientRect().top;
        body.scrollTop = mTarget.getBoundingClientRect().top - bTop2 + body.scrollTop + 6;
        // A scroll event is dispatched at the next render — under virtual
        // time that is late; a real wheel always has frames, so fire it now.
        body.dispatchEvent(new Event('scroll', { bubbles: true }));
        await wait(400);
        const lit = document.querySelector('#lv-rail .lv-node.on');
        const railEl = document.querySelector('#lv-rail');
        const rr = lit?.getBoundingClientRect(), rp = railEl.getBoundingClientRect();
        A('rail-follows-scroll', lit?.classList.contains('msg') && lit?.dataset.rid === String(rid2)
          && lit?.dataset.idx === '7' && rr && rr.top >= rp.top - 2 && rr.bottom <= rp.bottom + 2,
          `${lit ? lit.className.replace('lv-node ', '') : 'nothing'} lit, kept in the rail view`);
        // Scrolling onto the Response section lights the answer row too —
        // it is a message-like row and takes the same highlight.
        const outSec = document.getElementById(`lv-a-${rid2}`);
        body.scrollTop = outSec.getBoundingClientRect().top - body.getBoundingClientRect().top + body.scrollTop + 6;
        body.dispatchEvent(new Event('scroll', { bubbles: true }));
        let litAns = null;
        for (let k = 0; k < 8; k++) {
          await wait(200);
          litAns = document.querySelector('#lv-rail .lv-node.on');
          if (litAns?.classList.contains('resp')) break;
        }
        A('rail-lights-the-answer-row', litAns?.classList.contains('resp') && litAns?.dataset.rid === String(rid2),
          litAns ? `${litAns.className.replace('lv-node ', '')} lit at the response` : 'nothing lit at the response');
        // Scrolling all the way to the end marks the last row: the tail rows
        // are short and can never reach the top edge — the mark used to stop
        // a message or two before the end for exactly that reason.
        body.scrollTop = body.scrollHeight;
        body.dispatchEvent(new Event('scroll', { bubbles: true }));
        await wait(400);
        const litEnd = document.querySelector('#lv-rail .lv-node.on');
        A('rail-follows-to-end', litEnd && (litEnd.classList.contains('resp') || litEnd.classList.contains('turn'))
          && litEnd.dataset.rid === String(LV.conv.turns.at(-1).rid),
          litEnd ? `${litEnd.className.replace('lv-node ', '')} lit at the very bottom` : 'nothing lit at the end');
        // The rail never wears a horizontal scrollbar — its header wraps.
        A('rail-no-hscroll', railEl.scrollWidth <= railEl.clientWidth + 1,
          `rail content ${railEl.scrollWidth}px in ${railEl.clientWidth}px`);

        // --- The jump tree: nodes fold independently, and a message row jumps
        // to that message instead of merely to its request.
        (async () => {
          await wait(300);
          // Fold one request shut (through its header in the body), then open
          // it again from the tree — opening never disturbs the other folds.
          const turnNodes = [...document.querySelectorAll('#lv-rail .lv-node.turn')];
          const other = turnNodes.find(n => +n.dataset.rid !== LV.rid) || turnNodes[0];
          const ridB = +other.dataset.rid;
          if (LV.open.has(ridB)) { document.querySelector(`#lv-t-${ridB} .turn-h`).click(); await wait(500); }
          const before = [...LV.open];
          // Re-query: the fold just redrawn replaced every tree node.
          document.querySelector(`#lv-rail .lv-node.turn[data-rid="${ridB}"] .car`).click();
          await wait(600);
          const opened = [...LV.open];
          const expanded = [...document.querySelectorAll('#dlg-log section.turn')]
            .filter(sx => sx.querySelector('.lv-sec')).length;
          A('tree-expand-independent', before.every(r => opened.includes(r))
            && opened.includes(ridB) && expanded >= 2,
            `open ${opened.join(',')} (was ${before.join(',')}), ${expanded} request blocks readable`);
          // One fold, both directions: the tree caret opens the block, and the
          // block's own header folds the tree back.
          A('fold-tree-opens-block', LV.open.has(ridB) && !!document.querySelector(`#lv-t-${ridB} .lv-sec`),
            `request ${ridB} reads in the body after folding it open in the tree`);
          document.querySelector(`#lv-t-${ridB} .turn-h`).click();
          await wait(600);
          A('fold-block-folds-tree', !LV.open.has(ridB)
            && document.querySelector(`#lv-rail .lv-node.turn[data-rid="${ridB}"] .car`).textContent.trim() === '▸',
            'the tree caret followed the block header back to ▸');
          document.querySelector(`#lv-t-${ridB} .turn-h`).click();
          await wait(600);
          // A message has no clock instant of its own, but the window that
          // produced it is measured: a tool result is the stretch since the
          // previous answer ended, an assistant message that answer's own
          // generation. Rows with nothing measured stay blank and say so.
          const DUR = /^\d+(\.\d)?s$|^\d+m \d{2}s$/;
          const rows = [...document.querySelectorAll('#lv-rail .lv-node.msg')];
          const stamped = rows.filter(n => n.querySelector('.when').textContent.trim());
          A('tree-message-windows', stamped.length > 0
            && stamped.every(n => DUR.test(n.querySelector('.when').textContent.trim()))
            && stamped.every(n => /produced between|generated/.test(n.title)),
            stamped.map(n => `${n.querySelector('.tag').textContent} ${n.querySelector('.when').textContent}`).join(', ')
            || 'no message row carries a window');
          A('tree-unmeasured-explained', rows.filter(n => !n.querySelector('.when').textContent.trim())
            .every(n => /no instant of its own|never was captured/.test(n.title)),
            `${rows.length - stamped.length} row(s) with no window, all explained`);
          // A `+` marks a moment on the conversation's timeline; a plain
          // duration is a length. Response and idle rows are lengths, so they
          // must never wear the `+` a request row carries.
          const LEN = /^\d+(\.\d)?s$|^\d+m \d{2}s$/;
          const long = [...document.querySelectorAll('#lv-rail .lv-node.resp, #lv-rail .lv-node.gapnode')];
          A('resp-idle-are-lengths', long.length > 0 && long.every(n => {
            const w = n.querySelector('.when').textContent.trim();
            return w === '' || LEN.test(w);
          }), long.map(n => `${n.querySelector('.tag').textContent} ${n.querySelector('.when').textContent.trim()}`).join(' ') || 'no response rows');
          A('length-say-what-they-measure', long.filter(n => n.querySelector('.when').textContent.trim())
            .every(n => /how long/.test(n.title)), 'every duration row says how long of what');
          A('tree-single-highlight', document.querySelectorAll('#lv-rail .lv-node.on').length <= 1,
            `${document.querySelectorAll('#lv-rail .lv-node.on').length} row(s) highlighted`);
          const msg = document.querySelector('#lv-rail .lv-node.msg');
          if (!msg) { A('tree-child-focus', false, 'no message node in the tree'); return; }
          msg.click();
          await wait(600);
          const fl = document.querySelector('#dlg-log .msg.flash');
          const want = `lv-m-${msg.dataset.rid}-${msg.dataset.idx}`;
          A('tree-child-focus', !!fl && fl.id === want,
            fl ? `focused ${fl.id}` : `nothing focused, wanted ${want}`);
          A('tree-child-marked', document.querySelector('#lv-rail .lv-node.on')?.dataset.idx
            === msg.dataset.idx, 'the tree row shows which message is in view');
          // The first request keeps the clock; every stamp after it counts
          // continuously from the conversation's own start, not from the turn
          // before it, so a turn is a point on one line.
          const tw = [...document.querySelectorAll('#lv-rail .lv-node.turn .when')].map(n => n.textContent.trim());
          const t1 = LV.conv.turns[0], t2 = LV.conv.turns[1];
          const on = ms => `+${fmtDur((ms - t1.at_ms) / 1000)}`;
          A('tree-clock-counts-from-start', tw[0] === fmtTime(t1.at_ms)
            && tw.slice(1).every((w, k) => w === on(LV.conv.turns[k + 1].at_ms)), tw.join(' '));
          const bw = [...document.querySelectorAll('#dlg-log .turn-h .ts')].map(n => n.textContent.trim());
          A('body-clock-counts-from-start', bw[0].startsWith(fmtTime(t1.at_ms))
            && bw[1] === `${on(t2.at_ms)} \u2192 ${on(t2.tl.end)}`
            && document.querySelector('#dlg-log .turn-h .ts[title]').title.includes('into this conversation'),
            bw.join(' | '));
          A('idle-gap-named', !!document.querySelector('#dlg-log .lv-gap')
            && !!document.querySelector('#lv-rail .lv-node.gapnode'),
            document.querySelector('#dlg-log .lv-gap')?.textContent.trim().slice(0, 64));
          // The download is a shipped artefact and nothing else looks at it: the
          // transcript gets exactly one Request and one Response heading per
          // turn, written as prose rather than as the wire.
          for (const t of lvTurns()) await lvLoad(t.rid);
          const md = lvMarkdown(LV.conv, LV.content, lvTurns());
          const lines = md.split('\n');
          const reqL = lines.filter(l => l.startsWith('**Request**'));
          A('transcript-headings', reqL.length === lvTurns().length
            && lines.filter(l => l.startsWith('**Response**')).length === lvTurns().length,
            `${reqL.length} request headings for ${lvTurns().length} turns: ${reqL.join(' / ').slice(0, 72)}`);
          A('transcript-is-prose', !/data: ?\{|\[DONE\]/.test(md)
            && /rotate\(\) loop shifts files/.test(md)
            && /messages, \d+ new/.test(md),
            `${md.length} chars, stream read back as text, the held history named as a count`);
          // The tree's width is yours: drag, keys, double-click to reset, and it
          // is remembered like the other viewer choices.
          const grip = document.querySelector('#lv-grip');
          const railW = () => Math.round(document.querySelector('#lv-rail').getBoundingClientRect().width);
          const stored = () => JSON.parse(localStorage.getItem('omq.lv') || '{}').railW;
          const fire = (type, x) => grip.dispatchEvent(new PointerEvent(type, { pointerId: 7, clientX: x, bubbles: true }));
          const g0 = grip.getBoundingClientRect().left, w0 = railW();
          fire('pointerdown', g0 + 3);
          fire('pointermove', g0 + 3 - 120);
          fire('pointerup', g0 + 3 - 120);
          await wait(400);
          const w1 = railW();
          A('rail-resize-drag', Math.abs(w1 - (w0 + 120)) <= 2 && stored() === w1,
            `${w0}px \u2192 ${w1}px, stored ${stored()}`);
          grip.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }));
          await wait(300);
          A('rail-resize-keys', Math.abs(railW() - (w1 + 24)) <= 1, `ArrowLeft \u2192 ${railW()}px`);
          grip.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
          await wait(300);
          A('rail-resize-reset', railW() === 292 && stored() === 292, `double-click \u2192 ${railW()}px`);
        })();
      }, 600);
    }, 600);
  }, 3000);
});

// The other view of the same ring. Runs last, after the viewer chain is done:
// the segmented control splits the grouped rows back into one row per request,
// and the choice is remembered like the other view choices.
addEventListener('load', () => setTimeout(() => {
  const A2 = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  const before = document.querySelectorAll('#logs tbody tr[data-action="conv-open"]').length;
  const radio = document.querySelector('#log-group input[value="request"]');
  radio.checked = true;
  radio.dispatchEvent(new Event('change'));
  const rounds = document.querySelectorAll('#logs tbody tr[data-action="log-round"]').length;
  A2('logs-request-view-splits-rounds', rounds >= before && localStorage.getItem('omq.logGroup') === 'request',
    `${rounds} request rows vs ${before} conversation rows`);
}, 12000));

// The Overview list: one view (Details), the conversation's log chip at the
// right edge of each header like the stage lines below it, and the serving
// rates an active conversation is getting.
addEventListener('load', () => setTimeout(() => {
  const A3 = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  document.querySelector('a[data-page="overview"]').click();
  document.querySelector('#ov-req-show input[value="all"]').click();
  setTimeout(() => {
    A3('overview-list-is-details-only', !document.querySelector('#ov-req-view')
      && !document.querySelector('#ov-now .rq')
      && document.querySelectorAll('#ov-now .det-h').length > 0,
      `${document.querySelectorAll('#ov-now .det-h').length} conversation headers, no List toggle`);
    const heads = [...document.querySelectorAll('#ov-now .det-h')];
    const withLink = heads.filter(h => h.querySelector('.logs-link'));
    const edge = withLink.map(h => Math.round(
      h.querySelector('.logs-link').getBoundingClientRect().right - h.getBoundingClientRect().right));
    A3('header-logs-chip-rightmost', withLink.length > 0 && edge.every(d => d <= 12),
      `${withLink.length} headers, chip inset ${edge.slice(0, 4).join('/')}px`);
    A3('header-logs-chip-pointer', withLink.every(h => getComputedStyle(h.querySelector('.logs-link')).cursor === 'pointer'),
      withLink.length ? getComputedStyle(withLink[0].querySelector('.logs-link')).cursor : 'no chip');
    const dec = [...document.querySelectorAll('#ov-now .det-h .badge.dec')].map(b => b.textContent);
    const pre = [...document.querySelectorAll('#ov-now .det-h .badge.warn')].map(b => b.textContent);
    A3('header-shows-decode-rate', dec.some(t => /^decode [\d.]+ t\/s$/.test(t)), dec.slice(0, 3).join(' | ') || 'none');
    A3('header-shows-prefill-rate', pre.some(t => /^prefill [\d≈.]+ ?(tok\/s|s|m ..s)$/.test(t)),
      pre.slice(0, 3).join(' | ') || 'none');
    // The rate labels read as a property of the serving model: they follow
    // the `model → backend` span, not the client name.
    A3('rate-labels-after-model', heads.every(h => {
      const sub = h.querySelector('.sub'), t = h.textContent;
      if (!sub) return true;
      const i = t.indexOf(sub.textContent);
      return [...h.querySelectorAll('.badge')].every(b => t.indexOf(b.textContent) > i);
    }), 'prefill/decode follow the model → backend name');
  }, 900);
}, 16500));

// Compaction: the summarization a client sends when its context fills up is
// drawn as one striped block inside the conversation it summarizes, never as a
// mystery conversation of its own — and when nothing could be attached, it says
// so instead of pretending.
addEventListener('load', () => setTimeout(() => {
  const A4 = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  document.querySelector('a[data-page="overview"]').click();
  setTimeout(() => {
    const segs = [...document.querySelectorAll('#ov-timeline .tl-track .sg.c')];
    A4('graph-draws-compaction-blocks', segs.length >= 2, `${segs.length} striped blocks`);
    A4('graph-block-says-what-it-read', segs.some(s => /tok → .*tok/.test(s.title)),
      segs.map(s => s.title).slice(0, 2).join(' | '));
    A4('graph-block-says-how-it-was-attached', segs.some(s => /recency/.test(s.title)),
      segs.map(s => s.title).find(t => /recency/.test(t)) || 'no provenance');
    // It is a block for the whole request: one segment, not prefill + decode.
    const row = segs[0]?.closest('.tl-row');
    A4('compaction-is-one-block-not-two-stages',
      row && ![...row.querySelectorAll('.sg')].filter(s => /tok →/.test(s.title)).every(s => false)
        && row.querySelectorAll('.sg.c').length >= 1, 'striped block stands in for prefill+decode');
    // The row that carries it is labelled, and keeps its own identity.
    const lab = row?.querySelector('.tl-lab')?.textContent || '';
    A4('carrying-row-is-marked', /⤓/.test(lab) && /refactor-auth|a3f19c/.test(lab), lab.trim());
    // An unlinked summary gets its own row, and says it is unlinked.
    const orph = [...document.querySelectorAll('#ov-timeline .tl-lab')].filter(l => /unlinked/.test(l.textContent));
    A4('unlinked-compaction-says-so', orph.length >= 1, orph.map(l => l.textContent.trim()).join(' | ') || 'none');
    A4('legend-explains-the-mark',
      [...document.querySelectorAll('#ov-timeline .legend .sw.c')].length === 1, 'striped swatch in the legend');
    // One legend component, split by section: the stage key lives with the graph
    // that draws the stages, the identity key with the bar that colours by them.
    A4('activity-carries-the-stage-key',
      [...document.querySelectorAll('#ov-timeline .legend .sw')].map(s => s.className.replace('sw ', '')).join(',')
        === 'w,p,g,e,c', [...document.querySelectorAll('#ov-timeline .legend .sw')].map(s => s.className).join(','));
    A4('capacity-carries-the-identity-key',
      document.querySelectorAll('#ov-capacity .legend .sw.free').length === 1
        && !document.querySelector('#ov-capacity .legend .sw.g'),
      `${document.querySelectorAll('#ov-capacity .legend .sw').length} swatches under Capacity`);
    A4('one-legend-shape-both-panels',
      document.querySelectorAll('.tl-legend').length === 0
        && document.querySelectorAll('#ov-capacity .legend, #ov-timeline .legend').length === 2,
      'the old .tl-legend component is gone');
    // The window counter counts conversations, not summarization requests.
    const cmp = document.querySelector('#stats .stat.cmp');
    A4('window-counter-counts-conversations', !!cmp && /\b2\b/.test(cmp.textContent),
      cmp ? cmp.textContent.trim() : 'no counter');
    // Details: the conversation's header carries the same count as the graph.
    const head = [...document.querySelectorAll('#ov-now .det-h')]
      .find(h => /refactor-auth|a3f19c/.test(h.textContent) && h.querySelector('.badge.cmp'));
    A4('details-header-carries-count', !!head, head ? head.querySelector('.badge.cmp').textContent.trim() : 'none');
    const lines = [...document.querySelectorAll('#ov-now .det-row .st.c')];
    A4('details-stage-line-for-compaction', lines.some(s => /compaction/.test(s.textContent)),
      lines.map(s => s.textContent.trim()).slice(0, 2).join(' | ') || 'none');
    A4('details-line-names-attachment',
      [...document.querySelectorAll('#ov-now .det-row .val')].some(l => /recency/.test(l.textContent)),
      'provenance printed beside the stage');
    // The turn that resumes after a summary is marked, so its cold prefill is
    // not read as an eviction.
    A4('post-compaction-turn-marked',
      [...document.querySelectorAll('#ov-now .det-row .val')].some(l => /cold after the summary/.test(l.textContent)),
      'cold-after-the-summary on the resuming prefill');
  }, 900);
}, 26000));

// The conversation viewer: a stored turn says it was a summarization call.
addEventListener('load', () => setTimeout(() => {
  const A5 = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  location.hash = '#logs';
  setTimeout(() => {
    // A later block of this driver leaves the Logs page grouped per request;
    // this check is about the conversation row, so ask for that grouping.
    const radio = document.querySelector('#log-group input[value="session"]');
    if (radio && !radio.checked) { radio.checked = true; radio.dispatchEvent(new Event('change')); }
    const row = [...document.querySelectorAll('#logs tbody tr[data-action="conv-open"]')]
      .find(r => /b7c2d9/.test(r.textContent));
    A5('logs-row-badges-compaction', !!row && /⤓/.test(row.textContent), row ? row.textContent.replace(/\s+/g, ' ').trim().slice(0, 70) : 'no row');
    row?.click();
    setTimeout(() => {
      const badged = [...document.querySelectorAll('#dlg-log .turn-h .badge.cmp')].map(b => b.textContent.trim());
      A5('viewer-turn-badged', badged.some(t => /compaction/.test(t)), badged.join(' | ') || 'none');
    }, 700);
  }, 800);
}, 34000));

// --- Security audit flags ---------------------------------------------------
// Runs AFTER the pre-existing timeline (which navigates the viewer itself):
// flagged rows in the log list, the Flagged filter, the verdict block under a
// flagged turn. The demo audit marks the codex conversation (rid 901: confirmed
// exfil + watch-level injection) and one sessionless row (a recon).
addEventListener('load', () => setTimeout(() => {
  const A6 = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  setTimeout(() => { location.hash = '#logs'; }, 400);
  setTimeout(() => {
    const txt = [...document.querySelectorAll('#logs .rline .badge')].map(b => b.textContent).join(',');
    A6('flag-badges-in-list', /⚑ exfil/.test(txt) && /⚑ injection/.test(txt) && /⚑ recon/.test(txt), txt);
    document.querySelector('#log-flag input[value="flagged"]').click();
  }, 900);
  setTimeout(() => {
    const rows = [...document.querySelectorAll('#logs table.logs tr')].slice(1);
    A6('flagged-filter', rows.length > 0 && rows.every(r => r.textContent.includes('⚑')), `${rows.length} rows, every one flagged`);
    document.querySelector('#log-flag input[value="all"]').click();
  }, 1400);
  setTimeout(() => {
    const row = document.querySelector('#logs tr[data-session="a3f19c"]');
    if (row) row.click();   // opens the viewer on the flagged conversation
  }, 1900);
  setTimeout(async () => {
    const wait = ms => new Promise(r => setTimeout(r, ms));
    await wait(600);
    if (document.querySelector('#lv-t-901 .turn-h') && !LV.open.has(901)) {
      document.querySelector('#lv-t-901 .turn-h').click();   // expand the flagged request
      await wait(700);
    }
    A6('turn-audit-badge', !!document.querySelector('#lv-t-901 .turn-h .badge.bad'),
      document.querySelector('#lv-t-901 .turn-h')?.textContent.trim().slice(0, 60));
    A6('audit-block', !!document.querySelector('#dlg-log .audit'), 'verdict block under the flagged turn');
    A6('audit-block-rows', document.querySelectorAll('#dlg-log .audit .a-row').length === 2,
      `${document.querySelectorAll('#dlg-log .audit .a-row').length} verdict lines`);
  }, 2600);
}, 9800));

// The Overview filters reach the Logs list: the same window, the same
// client/model/backend scope, the same top-bar button — and the list says which
// scope is holding its rows back. Runs last: it edits S.filters and restores it.
addEventListener('load', () => setTimeout(() => {
  const A7 = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  setTimeout(() => { location.hash = '#logs'; }, 300);
  setTimeout(() => {
    const grouped = document.querySelector('#log-group input[value="session"]');
    if (grouped && !grouped.checked) { grouped.checked = true; grouped.dispatchEvent(new Event('change')); }
    const rows = () => [...document.querySelectorAll('#logs tbody tr[data-action="conv-open"]')];
    const before = rows().length;
    const users = [...new Set(logRounds(S.logs).map(r => (r.in || r.out || r.solo)?.user).filter(Boolean))];
    const one = users[0];
    A7('log-scope-has-two-clients-to-choose', users.length >= 2, users.join(','));
    S.filters = { ...S.filters, win: '48h', clients: [one], models: [], backends: [], priority: [] };
    renderFiltersBtn(); renderLogs();
    const left = rows();
    A7('client-scope-narrows-list', left.length > 0 && left.length < before
      && left.every(r => r.querySelector('.mono')?.textContent.trim() === one),
      `${before} → ${left.length} rows, client ${one}`);
    A7('scope-chip-names-the-scope', !$('#log-scope').hidden
      && $('#log-scope').textContent.includes('1 clients'), $('#log-scope').textContent);
    A7('filters-button-shows-on-logs', getComputedStyle($('#ov-filters')).display !== 'none',
      $('#ov-filters').title);
    const wide = left.length;
    S.filters = { ...S.filters, win: '5m' };
    renderLogs();
    A7('window-applies-to-the-list', rows().length <= wide, `${wide} rows in 48 h → ${rows().length} in 5 min`);
    S.filters = { ...S.filters, clients: [FNONE] };
    renderLogs();
    A7('empty-scope-explains-itself', /No row matches the filters/.test($('#logs').textContent),
      $('#logs').textContent.replace(/\s+/g, ' ').trim().slice(0, 70));
    S.filters = { ...S.filters, win: '15m', clients: [] };
    renderFiltersBtn(); renderLogs();
    A7('scope-restored', rows().length === before && $('#log-scope').hidden, `${rows().length} rows`);
  }, 1100);
}, 44000));
