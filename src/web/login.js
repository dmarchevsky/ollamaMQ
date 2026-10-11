// Sign-in page: posts the credentials, the server sets the session cookie,
// then we go back to where the user was headed.
'use strict';

(() => {
  const form = document.getElementById('login-form');
  const err = document.getElementById('login-error');
  const submit = document.getElementById('login-submit');

  /** Where to go after signing in: only paths inside the UI (no open redirect). */
  function target() {
    const next = new URLSearchParams(location.search).get('next') || '';
    // `/ui/…` only; `/ui//host` or `/ui/\host` could leave the site.
    const path = /^\/ui\/(?![/\\])/.test(next) ? next : '/ui/';
    // A server redirect cannot see the page's #fragment (e.g. #clients), but
    // the browser keeps it on this URL: carry it on.
    return path.includes('#') ? path : path + location.hash;
  }

  function showError(text) {
    err.textContent = text;
    err.hidden = false;
  }

  // Already signed in (e.g. the login page was bookmarked): go straight on.
  fetch('api/me', { headers: { 'X-OllamaMQ-UI': '1' } })
    .then(r => (r.ok ? r.json() : null))
    .then(d => { if (d && d.user) location.replace(target()); })
    .catch(() => {});

  form.addEventListener('submit', async e => {
    e.preventDefault();
    err.hidden = true;
    submit.disabled = true;
    submit.textContent = 'Signing in…';
    try {
      const res = await fetch('api/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-OllamaMQ-UI': '1' },
        body: JSON.stringify({
          username: form.username.value.trim(),
          password: form.password.value,
        }),
      });
      if (res.ok) {
        location.replace(target());
        return;
      }
      let msg = 'Sign-in failed';
      try { msg = (await res.json()).error || msg; } catch { /* not JSON */ }
      showError(msg);
      form.password.select();
    } catch {
      showError('Cannot reach ollamaMQ');
    }
    submit.disabled = false;
    submit.textContent = 'Sign in';
  });
})();
