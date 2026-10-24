// Demo-mode assertion driver for the conversation viewer. Append this file's
// body to a copy of src/web/index.html and run it headless:
//   <chromium> --virtual-time-budget=10000 --window-size=1280,800 \
//     --screenshot=out.png "file://<copy>/index.html?demo=1"
// Console prints "ASSERT PASS/FAIL <name>" per check.
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
    const tr = convRows.find(r => !/no text/.test(r.textContent));
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
    const whens = turns.map(n => n.querySelector('.when').textContent);
    A('stamps-shaped', whens.length > 0 && whens.every(w => /^(\d{2}:\d{2}:\d{2}|\+\d+s)$/.test(w)), JSON.stringify(whens));
    A('first-stamp-absolute', whens[0] === fmtTime(LV.conv.turns[0].at_ms),
      `${whens[0]} want ${fmtTime(LV.conv.turns[0].at_ms)}`);
    // The whole conversation is on the page, not the one round it was opened on:
    // one block per turn, and the focused one is expanded.
    const blocks = [...document.querySelectorAll('#dlg-log .lv-wrap section.turn')];
    A('all-turns-rendered', blocks.length === LV.conv.turns.length,
      `${blocks.length} blocks of ${LV.conv.turns.length}`);
    A('focus-expanded', document.querySelectorAll('.turn.active .lv-sec').length >= 2,
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
      setTimeout(() => {
        A('nav-last', LV.rid === LV.conv.turns.at(-1).rid, `focused ${LV.rid}`);
        A('nav-last-disabled', document.querySelector('#lv-last').disabled);
        // The Whole/New switch is gone. A turn shows what it added; what it only
        // re-sent collapses into one line naming it, in the body and in the tree.
        A('no-scope-switch', !document.querySelector('#lv-scope'));
        const cl = document.querySelector('#dlg-log .lv-carry');
        A('carried-history-one-line', !!cl && /held from request \d/.test(cl.textContent)
          && getComputedStyle(cl).cursor === 'pointer',
          cl ? cl.textContent.replace(/\s+/g, ' ').trim() : 'no turn here re-sends an earlier one');
        A('resent-prefix-counted', /· \d+ new this turn/.test(document.querySelector('#dlg-log').textContent),
          'the request line says what it added');
        // Capture turned off reads as a decision, in the tree and in the turn.
        const off = document.querySelector('.turn-h [title*="no body text"]');
        A('redacted-labelled', !!off || !LV.conv.redacted, 'capture-off turns are labelled');

        // --- The jump tree: nodes fold independently, and a message row jumps
        // to that message instead of merely to its request. Sequential waits,
        // so the chain stays readable instead of nesting one level per click.
        const wait = ms => new Promise(r => setTimeout(r, ms));
        (async () => {
          document.querySelector('#lv-rail [data-action="lv-collapse"]').click();
          await wait(500);
          const before = [...LV.open];
          const turnNodes = [...document.querySelectorAll('#lv-rail .lv-node.turn')];
          const other = turnNodes.find(n => !before.includes(n.dataset.rid)) || turnNodes[0];
          other.querySelector('.car').click();
          await wait(600);
          const opened = [...LV.open];
          const expanded = [...document.querySelectorAll('#dlg-log section.turn')]
            .filter(sx => sx.querySelector('.lv-sec')).length;
          A('tree-expand-independent', before.every(r => opened.includes(r))
            && opened.includes(+other.dataset.rid) && expanded >= 2,
            `open ${opened.join(',')} (was ${before.join(',')}), ${expanded} request blocks readable`);
          // One fold, both directions: the tree caret opens the block, and the
          // block's own header folds the tree back.
          const ridB = +other.dataset.rid;
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
          const rows = [...document.querySelectorAll('#lv-rail .lv-node.l3:not(.carry)')];
          const stamped = rows.filter(n => n.querySelector('.when').textContent.trim());
          A('tree-message-windows', stamped.length > 0
            && stamped.every(n => DUR.test(n.querySelector('.when').textContent.trim()))
            && stamped.every(n => /produced between|generated/.test(n.title)),
            stamped.map(n => `${n.querySelector('.tag').textContent} ${n.querySelector('.when').textContent}`).join(', ')
            || 'no message row carries a window');
          A('tree-unmeasured-explained', rows.filter(n => !n.querySelector('.when').textContent.trim())
            .every(n => /no instant of its own|never was captured/.test(n.title)),
            `${rows.length - stamped.length} row(s) with no window, all explained`);
          A('tree-response-delta', [...document.querySelectorAll('#lv-rail .lv-node.resp')]
            .every(n => /^\+\d+(\.\d)?s$|^$|^\d{2}:\d{2}:\d{2}$/.test(n.querySelector('.when').textContent)),
            [...document.querySelectorAll('#lv-rail .lv-node.resp')].map(n => n.querySelector('.when').textContent).join(' '));
          A('tree-single-highlight', document.querySelectorAll('#lv-rail .lv-node.on').length <= 1,
            `${document.querySelectorAll('#lv-rail .lv-node.on').length} row(s) highlighted`);
          const msg = document.querySelector('#lv-rail .lv-node.l3:not(.carry)');
          if (!msg) { A('tree-child-focus', false, 'no message node in the tree'); return; }
          msg.click();
          await wait(600);
          const fl = document.querySelector('#dlg-log .msg.flash');
          const want = `lv-m-${msg.dataset.rid}-${msg.dataset.idx}`;
          A('tree-child-focus', !!fl && fl.id === want,
            fl ? `focused ${fl.id}` : `nothing focused, wanted ${want}`);
          A('tree-child-marked', document.querySelector('#lv-rail .lv-node.on')?.dataset.idx
            === msg.dataset.idx, 'the tree row shows which message is in view');
          // The re-sent prefix is one line in the tree and one in the body, and
          // the two are the same fold: opening it grows both.
          const cnode = document.querySelector(`#lv-rail .lv-node.carry[data-rid="${LV.conv.turns[1].rid}"]`);
          if (!cnode) { A('carry-line-folds-both', false, 'request 2 shows no carried-history line'); return; }
          const rid2 = +cnode.dataset.rid;
          const hid = lvHidden(1);
          const car = s2 => document.querySelector(`#lv-rail .lv-node.carry[data-rid="${rid2}"] .car`).textContent.trim();
          const count = () => document.querySelectorAll(`#lv-t-${rid2} .msg`).length;
          const wasOpen = LV.hist.has(rid2);       // Expand all may have opened it already
          const before2 = count();
          cnode.click();
          await wait(700);
          const after2 = count();
          A('carry-line-folds-both', LV.hist.has(rid2) === !wasOpen
            && after2 === before2 + (wasOpen ? -hid : hid)
            && car() === (wasOpen ? '▸' : '▾') && !!document.querySelector(`#lv-t-${rid2} .lv-carry`),
            `request 2 ${before2} → ${after2} body messages (${wasOpen ? 'folded' : 'unfolded'} ${hid} held), tree caret ${car()}`);
          // (re-query: the redraw replaced the rail, so `cnode` is detached now)
          if (!LV.hist.has(rid2)) { document.querySelector(`#lv-rail .lv-node.carry[data-rid="${rid2}"]`).click(); await wait(700); }
          // With its history unfolded, a message that this request re-sent is on
          // screen here, so clicking it stays here instead of jumping away.
          const resent = [...document.querySelectorAll('#lv-rail .lv-node.l3:not(.carry)')]
            .find(n => +n.dataset.rid === rid2 && +n.dataset.idx < hid);
          if (!resent) { A('tree-resent-click-lands', false, `no unfolded row in request 2 (hidden ${hid})`); return; }
          resent.click();
          await wait(700);
          const want2 = `lv-m-${rid2}-${resent.dataset.idx}`;
          A('tree-resent-click-lands', LV.focusMsg?.[0] === rid2
            && String(LV.focusMsg[1]) === resent.dataset.idx && !!document.getElementById(want2),
            `${resent.querySelector('.tag').textContent} of request 2 -> ${want2} (unfolded here, so it stays here)`);
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
  }, 900);
}, 16500));
