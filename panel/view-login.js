// view-login.js — full-screen login, shown when /api/session says password_required && !authenticated.
// Server error messages (including the backend's details list) are rendered via textContent only.

import { api } from './api.js';
import { h, field, errorBlock, clearFieldErrors } from './dom.js';

export function renderLogin(root, { onSuccess }) {
  const form = h('form', { class: 'card login-card', novalidate: true });
  const pwd = h('input', { class: 'input', type: 'password', autocomplete: 'current-password', required: true, 'aria-label': 'Panel password' });
  const submit = h('button', { class: 'btn btn-primary login-submit', type: 'submit' }, 'Sign in');

  form.append(
    h('div', { class: 'login-brand' },
      h('img', { class: 'login-mark', src: './logo.svg', alt: '', width: '26', height: '26' }),
      h('div', {},
        h('div', { class: 'login-brand-name' }, 'Manarouter'),
        h('div', { class: 'login-brand-sub' }, 'Control plane'))),
    h('h1', { class: 'login-title' }, 'Sign in'),
    h('p', { class: 'login-lede' }, 'This control plane is password-protected.'),
    field('Password', pwd),
    submit,
  );

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    clearFieldErrors(form);
    submit.disabled = true;
    submit.textContent = 'Signing in…';
    try {
      // skipAuthRedirect: a wrong password is a normal error to display, not a session event
      // that should trigger the global 401 → login transition.
      await api.request('POST', '/api/login', { body: { password: pwd.value }, skipAuthRedirect: true });
      pwd.value = '';
      onSuccess();
    } catch (err) {
      const box = errorBlock(err);
      box.classList.add('login-error');
      form.insertBefore(box, submit);
      submit.disabled = false;
      submit.textContent = 'Sign in';
      pwd.focus();
    }
  });

  root.append(form);
  pwd.focus();
}
