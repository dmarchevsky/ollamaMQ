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
    const tr = [...document.querySelectorAll('#logs tbody tr[data-action]')]
      .find(r => !/no text/.test(r.textContent));
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
        // The Whole/New switch is gone: a turn folds its resent history instead.
        A('no-scope-switch', !document.querySelector('#lv-scope'));
        A('history-folded', !!document.querySelector('.lv-hist > summary'),
          'a later turn collapses the messages the previous one already sent');
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
          const before = [...LV.openTree];
          const turnNodes = [...document.querySelectorAll('#lv-rail .lv-node.turn')];
          const other = turnNodes.find(n => !before.includes(n.dataset.rid)) || turnNodes[0];
          other.querySelector('.car').click();
          await wait(600);
          const openTree = [...LV.openTree];
          const expanded = [...document.querySelectorAll('#dlg-log section.turn')]
            .filter(sx => sx.querySelector('.lv-sec')).length;
          A('tree-expand-independent', before.every(r => openTree.includes(r))
            && openTree.includes(+other.dataset.rid) && expanded >= 2,
            `open ${openTree.join(',')} (was ${before.join(',')}), ${expanded} request blocks readable`);
          A('tree-single-highlight', document.querySelectorAll('#lv-rail .lv-node.on').length <= 1,
            `${document.querySelectorAll('#lv-rail .lv-node.on').length} row(s) highlighted`);
          const msg = document.querySelector('#lv-rail .lv-node.l3');
          if (!msg) { A('tree-child-focus', false, 'no message node in the tree'); return; }
          msg.click();
          await wait(600);
          const fl = document.querySelector('#dlg-log .msg.flash');
          const want = `lv-m-${msg.dataset.rid}-${msg.dataset.idx}`;
          A('tree-child-focus', !!fl && fl.id === want,
            fl ? `focused ${fl.id}` : `nothing focused, wanted ${want}`);
          A('tree-child-marked', document.querySelector('#lv-rail .lv-node.on')?.dataset.idx
            === msg.dataset.idx, 'the tree row shows which message is in view');
        })();
      }, 600);
    }, 600);
  }, 3000);
});
