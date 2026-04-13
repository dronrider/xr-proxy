<script setup lang="ts">
import { RouterView, useRouter } from 'vue-router'
import { useAuthStore } from './stores/auth'

const auth = useAuthStore()
const router = useRouter()

function logout() {
  auth.logout()
  router.push('/login')
}
</script>

<template>
  <div class="app-container">
    <header v-if="auth.token" class="app-header">
      <h1>xr-hub</h1>
      <nav>
        <router-link to="/presets">Presets</router-link>
        <router-link to="/invites">Invites</router-link>
      </nav>
      <div class="header-right">
        <span class="username">{{ auth.username }}</span>
        <button class="btn-logout" @click="logout">Logout</button>
      </div>
    </header>
    <main>
      <RouterView />
    </main>
  </div>
</template>

<style>
:root {
  --bg: #f5f5f5;
  --bg-card: #fff;
  --bg-input: #fff;
  --bg-preview: #f9f9f9;
  --text: #333;
  --text-muted: #666;
  --text-heading: #1a1a2e;
  --border: #ccc;
  --border-light: #eee;
  --btn-bg: #1a1a2e;
  --btn-text: #fff;
  --btn-hover: #e8eaf6;
  --danger: #d32f2f;
  --success: #2e7d32;
  --warning: #f57c00;
  --toast-bg: #2e7d32;
  --toast-text: #fff;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #1a1a2e;
    --bg-card: #16213e;
    --bg-input: #0f3460;
    --bg-preview: #0f3460;
    --text: #e0e0e0;
    --text-muted: #a0a0a0;
    --text-heading: #e0e0e0;
    --border: #333;
    --border-light: #2a2a3e;
    --btn-bg: #533483;
    --btn-text: #fff;
    --btn-hover: #2a2a3e;
    --danger: #ef5350;
    --success: #66bb6a;
    --warning: #ffb74d;
    --toast-bg: #388e3c;
    --toast-text: #fff;
  }
}

* {
  margin: 0;
  padding: 0;
  box-sizing: border-box;
}

body {
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
  background: var(--bg);
  color: var(--text);
}

.app-container {
  max-width: 1200px;
  margin: 0 auto;
  padding: 1rem;
}

.app-header {
  display: flex;
  align-items: center;
  gap: 2rem;
  margin-bottom: 2rem;
  padding: 1rem;
  background: var(--bg-card);
  border-radius: 8px;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
}

.app-header h1 {
  font-size: 1.5rem;
  color: var(--text-heading);
}

.app-header nav {
  display: flex;
  gap: 1rem;
}

.app-header nav a {
  color: var(--text-muted);
  text-decoration: none;
  padding: 0.5rem 1rem;
  border-radius: 4px;
  transition: background 0.2s;
}

.app-header nav a:hover,
.app-header nav a.router-link-active {
  background: var(--btn-hover);
  color: var(--text-heading);
}

.header-right {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 0.75rem;
}

.username {
  font-size: 0.875rem;
  color: var(--text-muted);
}

.btn-logout {
  padding: 0.25rem 0.75rem;
  font-size: 0.8rem;
  border: 1px solid var(--border);
  background: transparent;
  color: var(--text-muted);
  border-radius: 4px;
  cursor: pointer;
}

.btn-logout:hover {
  color: var(--danger);
  border-color: var(--danger);
}

main {
  background: var(--bg-card);
  border-radius: 8px;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
  padding: 1.5rem;
}

/* ── Toast ─────────────────────────── */
.toast {
  position: fixed;
  bottom: 2rem;
  right: 2rem;
  padding: 0.75rem 1.5rem;
  background: var(--toast-bg);
  color: var(--toast-text);
  border-radius: 8px;
  font-size: 0.9rem;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
  z-index: 1000;
  animation: toast-in 0.3s ease;
}

@keyframes toast-in {
  from { opacity: 0; transform: translateY(1rem); }
  to { opacity: 1; transform: translateY(0); }
}
</style>
