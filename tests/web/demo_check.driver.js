// Demo-mode assertion driver for the log viewer. Append this file's body to a
// copy of src/web/index.html and run it headless:
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
  setTimeout(() => { const tr = document.querySelector('#logs tbody tr[data-action]'); if (tr) tr.click(); }, 1500);
  setTimeout(() => {
    document.querySelector('#lv-jump').click();
    const de = document.documentElement;
    A('background-locked', getComputedStyle(de).overflow === 'hidden',
      `html overflow ${getComputedStyle(de).overflow}`); // hidden = no user scroll, no bar
    const b = document.querySelector('#dlg-log .lv-body');
    A('body-scrollable', b.scrollHeight > b.clientHeight && getComputedStyle(b).scrollbarWidth !== 'thin');
    const whens = [...document.querySelectorAll('#lv-rail .lv-item .when')].map(e => e.textContent);
    A('stamps-shaped', whens.length > 0 && whens.every(w => /^(\d{2}:\d{2}:\d{2}|\+\d+s)$/.test(w)), JSON.stringify(whens.slice(0, 3)));
    A('first-stamp-absolute', whens[0] === fmtTime(LV.inEv.at_ms), `${whens[0]} want ${fmtTime(LV.inEv.at_ms)}`);
    const d = LV.inEv && LV.outEv ? Math.floor((LV.outEv.at_ms - LV.inEv.at_ms) / 1000) : null;
    const resp = [...document.querySelectorAll('#lv-rail .lv-item.sec .when')]
      .find(e => e.closest('.lv-item').querySelector('.tag').textContent === 'resp');
    const want = d !== null && d < 60 ? `+${d}s` : LV.outEv && fmtTime(LV.outEv.at_ms);
    A('resp-stamp', !!resp && resp.textContent === want, `got ${resp?.textContent} want ${want} (delta ${d}s)`);
  }, 3000);
});
