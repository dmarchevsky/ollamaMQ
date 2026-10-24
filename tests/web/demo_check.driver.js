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
          // The jump tree lists only what each request added — no Held row, and
          // no re-sent rows under a later request. The body keeps the fold line.
          const rid2 = LV.conv.turns[1].rid, hid = lvHidden(1);
          const cl = () => document.querySelector(`#lv-t-${rid2} .lv-carry`);
          if (LV.hist.has(rid2)) { cl().click(); await wait(700); }   // Expand all opens folds too
          const rows2 = [...document.querySelectorAll(`#lv-rail .lv-node.l3[data-rid="${rid2}"]`)];
          A('tree-rows-are-new-only', rows2.length > 0 && rows2.every(n => +n.dataset.idx >= hid)
            && !document.querySelector('#lv-rail .lv-node.carry'),
            `${rows2.length} rows for request 2, every idx >= ${hid}, no Held row`);
          if (!cl()) { A('carry-line-folds-body', false, 'request 2 shows no carried-history line'); return; }
          const n0 = document.querySelectorAll(`#lv-t-${rid2} .msg`).length;
          cl().click();
          await wait(700);
          const n1 = document.querySelectorAll(`#lv-t-${rid2} .msg`).length;
          A('carry-line-folds-body', LV.hist.has(rid2) && n1 === n0 + hid
            && cl().textContent.trim().startsWith('\u25be'),
            `request 2 body ${n0} \u2192 ${n1} messages (+${hid} held)`);
          // With its held history open on screen, that request's own copy of a
          // re-sent message is what a jump lands on \u2014 not the earlier turn.
          await lvFocusMsg(rid2, 0);
          await wait(500);
          A('unfolded-resent-stays', LV.focusMsg?.[0] === rid2 && LV.focusMsg?.[1] === 0
            && !!document.getElementById(`lv-m-${rid2}-0`), 'idx 0 of request 2 stays in request 2');
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
          // Formatted view reads the stream back as the answer, not as the wire:
          // keep-alive comments and empty deltas are gone, the text is whole.
          // (the Response section is the last one in the turn; its class sits on
          // the heading, not the section)
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
  }, 900);
}, 16500));
