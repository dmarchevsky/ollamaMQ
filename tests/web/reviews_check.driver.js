// Demo-mode assertion driver for the Reviews page and the Send-to-review
// dialog. Run it like demo_check.driver.js: splice this file's body into a
// copy of src/web/index.html as a <script> before </body>, then
//   <chromium> --headless --no-sandbox --disable-gpu --enable-logging=stderr --v=0 \
//     --virtual-time-budget=10000 --window-size=1280,800 "file://<copy>/index.html?demo=1" 2>&1 | grep ASSERT
// Console prints "ASSERT PASS/FAIL <name>" per check; the timeline ends at 7.5 s.
addEventListener('load', () => {
  const A = (name, ok, extra) => console.log(`ASSERT ${ok ? 'PASS' : 'FAIL'} ${name}${extra ? ' — ' + extra : ''}`);
  const $q = s => document.querySelector(s);
  setTimeout(() => { location.hash = '#reviews'; }, 300);
  setTimeout(() => {
    const rows = [...document.querySelectorAll('#rv-list .rv-row')];
    A('reviews-listed', rows.length === 5, String(rows.length));
    A('reviews-nav-count', $q('#nav-reviews').textContent === '2', $q('#nav-reviews').textContent);
    A('queued-says-why', /more of quiet/.test(rows[0]?.textContent || ''), rows[0]?.textContent.trim());
    // Pending keeps only queued and running.
    const pending = $q('#rv-show input[value="open"]');
    pending.checked = true;
    pending.dispatchEvent(new Event('change'));
    const left = [...document.querySelectorAll('#rv-list .rv-row .badge')].map(b => b.textContent);
    A('pending-filter', left.length === 2 && left.every(t => t === 'queued' || t === 'running'), left.join(','));
    const all = $q('#rv-show input[value="all"]');
    all.checked = true;
    all.dispatchEvent(new Event('change'));
    $q('#rv-list [data-id="19a4c-0"]').click();
  }, 1200);
  setTimeout(() => {
    // The finished review's answer is drawn as Markdown, its text escaped.
    const md = $q('#rv-detail .md');
    A('done-answer-rendered', !!md && md.querySelectorAll('h3, h4').length >= 5 && md.querySelector('ol li b'),
      md ? md.innerHTML.slice(0, 80) : 'no .md');
    A('re-run-offered', !!$q('#rv-detail [data-action="rv-retry"]'));
    A('md-escapes-html', !/<img/i.test(mdLite('<img src=x onerror=alert(1)> **b**')) && /<b>b<\/b>/.test(mdLite('**b**')));
    // Send a conversation with an edited prompt from the viewer.
    openConv({ key: 'n8n-agents#b7c2d9' });
  }, 2200);
  setTimeout(() => { $q('#lv-review').click(); }, 3200);
  setTimeout(() => {
    const dlg = $q('#dlg-review');
    A('review-dialog-open', dlg.open);
    A('review-dialog-default-prompt', dlg.querySelector('select').value === 'default');
    A('review-dialog-model', dlg.querySelector('input[name="model"]').value === 'Qwen3.8-27B');
    const t = dlg.querySelector('textarea');
    t.value = 'Count the tool calls.';
    t.dispatchEvent(new Event('input'));
    A('review-dialog-edited-hint', /this review only/.test(dlg.querySelector('[data-edited]').textContent));
    dlg.querySelector('button[value="ok"]').click();
    // Headless virtual time holds a dialog's `close` event until the budget
    // runs out; deliver it now so the send happens inside the timeline (the
    // late real one finds its listener gone and does nothing).
    dlg.dispatchEvent(new Event('close'));
  }, 4200);
  setTimeout(() => {
    // S.reviews follows the Reviews page only; ask the API, as the page would.
    api('GET', 'reviews?conv=' + encodeURIComponent('n8n-agents#b7c2d9')).then(r => {
      const sent = r.reviews.find(x => x.custom && x.status === 'queued');
      A('review-sent-custom', !!sent && sent.id === S.reviewSel, JSON.stringify(sent || null).slice(0, 120));
    });
    A('viewer-count-updated', /\(\d+\)/.test($q('#lv-review').textContent), $q('#lv-review').textContent);
  }, 7500);
});
